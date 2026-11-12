//! Memory beyond "in use": commit charge and cache (GetPerformanceInfo), where every byte
//! of RAM goes (Memory and Process performance counters), and the installed modules as the
//! firmware describes them (SMBIOS type 17). No admin needed.

use serde::Serialize;
use std::collections::HashMap;
use std::ptr::null_mut;
use windows_sys::Win32::System::Performance::{
    PDH_FMT_COUNTERVALUE, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_LARGE, PDH_HCOUNTER, PDH_HQUERY, PdhAddEnglishCounterW,
    PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhGetFormattedCounterValue, PdhOpenQueryW,
};
use windows_sys::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};
use windows_sys::Win32::System::SystemInformation::{GetPhysicallyInstalledSystemMemory, GetSystemFirmwareTable};

/// Bytes: committed, commit limit (RAM + page file), file cache.
pub fn commit_and_cache() -> Option<(u64, u64, u64)> {
    let mut p: PERFORMANCE_INFORMATION = unsafe { std::mem::zeroed() };
    let size = size_of::<PERFORMANCE_INFORMATION>() as u32;
    if unsafe { GetPerformanceInfo(&mut p, size) } == 0 {
        return None;
    }
    let page = p.PageSize as u64;
    Some((p.CommitTotal as u64 * page, p.CommitLimit as u64 * page, p.SystemCache as u64 * page))
}

/// Every byte of installed RAM, by use. "In use" is programs + compressed + kernel +
/// modified + other; available is standby + free; reserved never reaches Windows.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct Breakdown {
    pub installed: u64,
    /// Private working sets of every process except the compression store.
    pub programs: u64,
    /// Working set of the "Memory Compression" process: pages compressed instead of paged out.
    pub compressed: u64,
    /// Nonpaged pool (always in RAM), paged pool in RAM, kernel and driver code, system cache.
    pub nonpaged: u64,
    pub paged: u64,
    pub code: u64,
    pub system_cache: u64,
    /// Changed pages waiting to be written to disk before reuse.
    pub modified: u64,
    /// The rest of "in use": pages shared by programs (DLLs, mapped files) counted once,
    /// page tables, kernel stacks, memory locked by drivers.
    pub other: u64,
    pub standby: u64,
    pub free: u64,
    /// Installed but kept by the firmware and devices.
    pub reserved: u64,
}

/// Process name (without ".exe") → (private working set in bytes, process count).
pub type PrivateSets = HashMap<String, (u64, u32)>;

const MEMORY_COUNTERS: [&str; 10] = [
    r"\Memory\Available Bytes",
    r"\Memory\Pool Nonpaged Bytes",
    r"\Memory\Pool Paged Resident Bytes",
    r"\Memory\System Driver Resident Bytes",
    r"\Memory\System Code Resident Bytes",
    r"\Memory\System Cache Resident Bytes",
    r"\Memory\Modified Page List Bytes",
    r"\Memory\Standby Cache Core Bytes",
    r"\Memory\Standby Cache Normal Priority Bytes",
    r"\Memory\Standby Cache Reserve Bytes",
];
const FREE_COUNTER: &str = r"\Memory\Free & Zero Page List Bytes";
const PRIVATE_SETS: &str = r"\Process(*)\Working Set - Private";
const COMPRESSION: &str = "Memory Compression";

/// A PDH query over the Memory counters and every process's private working set. Only the
/// Memory page uses it: collecting the Process object walks every process.
pub struct Counters {
    query: PDH_HQUERY,
    memory: Vec<PDH_HCOUNTER>,
    free: PDH_HCOUNTER,
    processes: PDH_HCOUNTER,
}

// PDH handles are plain process-wide handles; the sampler thread owns this.
unsafe impl Send for Counters {}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

impl Counters {
    pub fn new() -> Option<Self> {
        let mut query = null_mut();
        let add = |query: PDH_HQUERY, path: &str| {
            let mut counter = null_mut();
            (unsafe { PdhAddEnglishCounterW(query, wide(path).as_ptr(), 0, &mut counter) } == 0).then_some(counter)
        };
        if unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut query) } != 0 {
            return None;
        }
        let memory = MEMORY_COUNTERS.iter().map(|p| add(query, p)).collect::<Option<Vec<_>>>()?;
        Some(Self { query, memory, free: add(query, FREE_COUNTER)?, processes: add(query, PRIVATE_SETS)? })
    }

    /// Collects once; `usable` is the RAM Windows manages (total physical memory).
    pub fn read(&mut self, usable: u64) -> Option<(Breakdown, PrivateSets)> {
        if unsafe { PdhCollectQueryData(self.query) } != 0 {
            return None;
        }
        let value = |counter: PDH_HCOUNTER| {
            let mut v: PDH_FMT_COUNTERVALUE = unsafe { std::mem::zeroed() };
            let ok = unsafe { PdhGetFormattedCounterValue(counter, PDH_FMT_LARGE, null_mut(), &mut v) } == 0;
            if ok && v.CStatus == 0 { unsafe { v.Anonymous.largeValue }.max(0) as u64 } else { 0 }
        };
        let m: Vec<u64> = self.memory.iter().map(|&c| value(c)).collect();
        let sets = private_sets(self.processes)?;
        let mut installed_kb = 0u64;
        let installed = if unsafe { GetPhysicallyInstalledSystemMemory(&mut installed_kb) } != 0 { installed_kb * 1024 } else { usable };
        Some((split(usable, installed, &m, value(self.free), &sets), sets))
    }
}

/// Every process instance ("Code", "Code#3", …) grouped by name, `_Total` and `Idle` left out.
fn private_sets(counter: PDH_HCOUNTER) -> Option<PrivateSets> {
    let (mut size, mut count) = (0u32, 0u32);
    unsafe { PdhGetFormattedCounterArrayW(counter, PDH_FMT_LARGE, &mut size, &mut count, null_mut()) };
    if size == 0 {
        return None;
    }
    // u64 cells keep the item array aligned.
    let mut buf = vec![0u64; (size as usize).div_ceil(8)];
    let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
    if unsafe { PdhGetFormattedCounterArrayW(counter, PDH_FMT_LARGE, &mut size, &mut count, items) } != 0 {
        return None;
    }
    let mut out = PrivateSets::new();
    for item in unsafe { std::slice::from_raw_parts(items, count as usize) } {
        let len = (0..).take_while(|&i| unsafe { *item.szName.add(i) } != 0).count();
        let name = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(item.szName, len) });
        let name = name.split('#').next().unwrap_or_default().to_string();
        if item.FmtValue.CStatus != 0 || name == "_Total" || name == "Idle" {
            continue;
        }
        let e = out.entry(name).or_default();
        e.0 += unsafe { item.FmtValue.Anonymous.largeValue }.max(0) as u64;
        e.1 += 1;
    }
    Some(out)
}

/// `m` holds MEMORY_COUNTERS in order. Pure, so the arithmetic is tested.
fn split(usable: u64, installed: u64, m: &[u64], free: u64, sets: &PrivateSets) -> Breakdown {
    let compressed = sets.get(COMPRESSION).map_or(0, |e| e.0);
    let programs = sets.values().map(|e| e.0).sum::<u64>().saturating_sub(compressed);
    let (nonpaged, paged, code, system_cache, modified) = (m[1], m[2], m[3] + m[4], m[5], m[6]);
    let in_use = usable.saturating_sub(m[0]);
    let known = programs + compressed + nonpaged + paged + code + system_cache + modified;
    Breakdown {
        installed: installed.max(usable),
        programs,
        compressed,
        nonpaged,
        paged,
        code,
        system_cache,
        modified,
        other: in_use.saturating_sub(known),
        standby: m[7] + m[8] + m[9],
        free,
        reserved: installed.saturating_sub(usable),
    }
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Module {
    pub slot: String,
    pub size_mb: u64,
    pub kind: &'static str,
    /// Rated and currently configured speed, in MT/s.
    pub speed: Option<u32>,
    pub configured: Option<u32>,
    pub maker: String,
    pub part: String,
}

pub fn modules() -> Vec<Module> {
    let rsmb = u32::from_be_bytes(*b"RSMB");
    let len = unsafe { GetSystemFirmwareTable(rsmb, 0, std::ptr::null_mut(), 0) };
    let mut buf = vec![0u8; len as usize];
    if len < 8 || unsafe { GetSystemFirmwareTable(rsmb, 0, buf.as_mut_ptr().cast(), len) } != len {
        return Vec::new();
    }
    // RawSMBIOSData: 4 version bytes, the table length, then the table.
    let table = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
    parse_modules(&buf[8..(8 + table).min(buf.len())])
}

fn memory_type(code: u8) -> &'static str {
    match code {
        0x18 => "DDR3",
        0x1A => "DDR4",
        0x1E => "LPDDR4",
        0x22 => "DDR5",
        0x23 => "LPDDR5",
        _ => "",
    }
}

/// Walks the SMBIOS structures: a formatted area of `length` bytes, then its strings, each
/// NUL-terminated, the set ending with an extra NUL. Type 17 is a memory device.
fn parse_modules(t: &[u8]) -> Vec<Module> {
    let mut out = Vec::new();
    let mut off = 0;
    while off + 4 <= t.len() {
        let (kind, len) = (t[off], t[off + 1] as usize);
        if len < 4 || off + len > t.len() {
            break;
        }
        let mut end = off + len;
        while end + 1 < t.len() && !(t[end] == 0 && t[end + 1] == 0) {
            end += 1;
        }
        let strings: Vec<&[u8]> = t[off + len..end].split(|&b| b == 0).collect();
        let f = &t[off..off + len];
        let text = |at: usize| {
            let i = *f.get(at).unwrap_or(&0) as usize;
            let s = if i == 0 { &[][..] } else { strings.get(i - 1).copied().unwrap_or_default() };
            String::from_utf8_lossy(s).trim().to_string()
        };
        let word = |at: usize| f.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
        let speed = |at: usize| word(at).filter(|&v| v != 0 && v != 0xFFFF).map(u32::from);
        if kind == 17 && len >= 0x1B {
            // Size: MB, KB when bit 15 is set, 0x7FFF means "see the 32-bit extended size".
            let size_mb = match word(0x0C).unwrap_or(0) {
                0 | 0xFFFF => 0,
                0x7FFF => f.get(0x1C..0x20).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()) as u64),
                s if s & 0x8000 != 0 => (s & 0x7FFF) as u64 / 1024,
                s => s as u64,
            };
            if size_mb > 0 {
                out.push(Module {
                    slot: text(0x10),
                    size_mb,
                    kind: memory_type(f[0x12]),
                    speed: speed(0x15),
                    configured: speed(0x20),
                    maker: text(0x17),
                    part: text(0x1A),
                });
            }
        }
        if kind == 127 {
            break;
        }
        off = end + 2;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_in_use_memory_and_keeps_the_rest_for_other() {
        const G: u64 = 1 << 30;
        let sets = PrivateSets::from([("Code".into(), (3 * G, 30)), (COMPRESSION.into(), (2 * G, 1)), ("explorer".into(), (G, 1))]);
        // available 10, nonpaged 1, paged 1, driver + kernel code 0 + 0, cache 0, modified 0, standby 4 + 0 + 0
        let m = [10 * G, G, G, 0, 0, 0, 0, 4 * G, 0, 0];
        let b = split(30 * G, 32 * G, &m, 6 * G, &sets);
        assert_eq!((b.programs, b.compressed), (4 * G, 2 * G));
        // In use 20 = programs 4 + compressed 2 + pools 2 + other 12.
        assert_eq!(b.other, 12 * G);
        assert_eq!((b.standby, b.free, b.reserved), (4 * G, 6 * G, 2 * G));
        // Counters can lag each other: "other" never goes negative.
        let m = [29 * G, G, G, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(split(30 * G, 30 * G, &m, 0, &sets).other, 0);
    }

    fn device(size: u16, strings: &[u8]) -> Vec<u8> {
        let mut f = vec![0u8; 0x22];
        f[0] = 17;
        f[1] = 0x22;
        f[0x0C..0x0E].copy_from_slice(&size.to_le_bytes());
        f[0x10] = 1; // slot
        f[0x12] = 0x22; // DDR5
        f[0x15..0x17].copy_from_slice(&5600u16.to_le_bytes());
        f[0x17] = 2; // maker
        f[0x1A] = 3; // part
        f[0x20..0x22].copy_from_slice(&5200u16.to_le_bytes());
        f.extend_from_slice(strings);
        f
    }

    #[test]
    fn reads_installed_modules_and_skips_empty_slots() {
        let mut t = device(16384, b"DIMM 1\0Example\0EX-DDR5-16G\0\0");
        t.extend(device(0, b"DIMM 2\0\0")); // empty slot
        t.extend([127, 4, 0, 0, 0, 0]);
        let m = parse_modules(&t);
        assert_eq!(m.len(), 1);
        assert_eq!(
            m[0],
            Module {
                slot: "DIMM 1".into(),
                size_mb: 16384,
                kind: "DDR5",
                speed: Some(5600),
                configured: Some(5200),
                maker: "Example".into(),
                part: "EX-DDR5-16G".into()
            }
        );
    }
}
