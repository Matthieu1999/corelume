//! CPU topology: physical cores, their logical processors, P/E kind and shared-L2 clusters.

use serde::Serialize;
use windows_sys::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, RelationAll, RelationCache, RelationProcessorCore,
    SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Core {
    /// "P" or "E" on hybrid CPUs, "C" when every core is alike.
    pub kind: &'static str,
    /// Logical processor numbers, the indices of the per-thread load list.
    pub threads: Vec<u32>,
    /// Cores sharing an L2 cache (Intel E-core modules) share an id and one temperature sensor.
    pub cluster: u32,
    #[serde(skip)]
    pub group: u16,
    #[serde(skip)]
    pub mask: usize,
}

#[allow(non_upper_case_globals)] // matching on Win32 constant names
pub fn cores() -> Vec<Core> {
    let mut len = 0u32;
    unsafe { GetLogicalProcessorInformationEx(RelationAll, std::ptr::null_mut(), &mut len) };
    let mut buf = vec![0u8; len as usize];
    if unsafe { GetLogicalProcessorInformationEx(RelationAll, buf.as_mut_ptr().cast(), &mut len) } == 0 {
        return Vec::new();
    }
    let mut raw = Vec::new();
    let mut l2 = Vec::new();
    let mut off = 0;
    while off + 8 <= len as usize {
        let size = u32::from_ne_bytes(buf[off + 4..off + 8].try_into().unwrap()) as usize;
        if size == 0 || off + size > len as usize {
            break;
        }
        // Records are variable-sized and packed; copy one into a zeroed struct so a short
        // record never reads past the buffer.
        let mut info: SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX = unsafe { std::mem::zeroed() };
        let n = size.min(size_of_val(&info));
        unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr().add(off), (&raw mut info).cast::<u8>(), n) };
        match info.Relationship {
            RelationProcessorCore => {
                let p = unsafe { info.Anonymous.Processor };
                let g = p.GroupMask[0]; // a core never spans processor groups
                raw.push((p.EfficiencyClass, g.Group, g.Mask));
            }
            RelationCache => {
                let c = unsafe { info.Anonymous.Cache };
                if c.Level == 2 {
                    let g = unsafe { c.Anonymous.GroupMask };
                    l2.push((g.Group, g.Mask));
                }
            }
            _ => {}
        }
        off += size;
    }
    build(raw, &l2)
}

/// Names kinds and clusters from (efficiency class, group, affinity mask) core records.
fn build(raw: Vec<(u8, u16, usize)>, l2: &[(u16, usize)]) -> Vec<Core> {
    let top = raw.iter().map(|r| r.0).max().unwrap_or(0);
    let hybrid = raw.iter().any(|r| r.0 != top);
    raw.into_iter()
        .enumerate()
        .map(|(i, (efficiency, group, mask))| Core {
            kind: match (hybrid, efficiency == top) {
                (false, _) => "C",
                (true, true) => "P",
                (true, false) => "E",
            },
            // shortcut: logical numbers assume one processor group (<= 64 logical CPUs);
            // offset by group size before supporting bigger machines.
            threads: (0..usize::BITS).filter(|b| mask >> b & 1 == 1).collect(),
            cluster: l2
                .iter()
                .position(|&(g, m)| g == group && m & mask != 0)
                .map_or(1000 + i as u32, |c| c as u32),
            group,
            mask,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hybrid_cpu_gets_kinds_and_e_core_clusters() {
        // Two P-cores with SMT (class 1), four E-cores (class 0) sharing one L2.
        let raw = vec![(1, 0, 0b11), (1, 0, 0b1100), (0, 0, 1 << 4), (0, 0, 1 << 5), (0, 0, 1 << 6), (0, 0, 1 << 7)];
        let l2 = [(0, 0b11), (0, 0b1100), (0, 0b1111_0000)];
        let cores = build(raw, &l2);
        let kinds: Vec<_> = cores.iter().map(|c| c.kind).collect();
        assert_eq!(kinds, ["P", "P", "E", "E", "E", "E"]);
        assert_eq!(cores[1].threads, [2, 3]);
        assert_eq!(cores.iter().map(|c| c.cluster).collect::<Vec<_>>(), [0, 1, 2, 2, 2, 2]);
    }

    #[test]
    fn uniform_cpu_without_cache_info() {
        let cores = build(vec![(0, 0, 0b11), (0, 0, 0b1100)], &[]);
        assert!(cores.iter().all(|c| c.kind == "C"));
        assert_ne!(cores[0].cluster, cores[1].cluster);
    }
}
