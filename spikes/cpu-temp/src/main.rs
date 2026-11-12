//! Phase 0b spike: read Intel CPU package and per-core temperatures through the PawnIO
//! driver, idle then under a 10 s all-core load. Becomes src-tauri/src/win/cpu_temp.rs.
//! Needs admin. Usage: cpu-temp-spike.exe [output-file]

use std::ffi::c_void;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::Win32::System::SystemInformation::{
    GROUP_AFFINITY, GetLogicalProcessorInformationEx, RelationProcessorCore,
    SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};
use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadGroupAffinity};

static INTEL_MSR: &[u8] = include_bytes!("../../../vendor/pawnio/IntelMSR.bin");

const IA32_THERM_STATUS: u64 = 0x19C;
const IA32_TEMPERATURE_TARGET: u64 = 0x1A2;
const IA32_PACKAGE_THERM_STATUS: u64 = 0x1B1;

type Handle = *mut c_void;
type OpenFn = unsafe extern "system" fn(*mut Handle) -> i32;
type LoadFn = unsafe extern "system" fn(Handle, *const u8, usize) -> i32;
type ExecFn =
    unsafe extern "system" fn(Handle, *const u8, *const u64, usize, *mut u64, usize, *mut usize) -> i32;
type CloseFn = unsafe extern "system" fn(Handle) -> i32;

struct PawnIo {
    handle: Handle,
    exec: ExecFn,
    close: CloseFn,
}

impl PawnIo {
    fn open() -> Result<Self, String> {
        let dir = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into());
        let path: Vec<u16> = format!(r"{dir}\PawnIO\PawnIOLib.dll").encode_utf16().chain([0]).collect();
        // Absolute path, dependencies from its own folder or System32 only: no DLL planting.
        let lib = unsafe {
            LoadLibraryExW(path.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32)
        };
        if lib.is_null() {
            return Err("PawnIO is not installed (PawnIOLib.dll not found)".into());
        }
        let sym = |name: &str| {
            let c = format!("{name}\0");
            unsafe { GetProcAddress(lib, c.as_ptr()) }.ok_or(format!("PawnIOLib.dll has no {name}"))
        };
        unsafe {
            let open: OpenFn = std::mem::transmute(sym("pawnio_open")?);
            let load: LoadFn = std::mem::transmute(sym("pawnio_load")?);
            let exec: ExecFn = std::mem::transmute(sym("pawnio_execute")?);
            let close: CloseFn = std::mem::transmute(sym("pawnio_close")?);
            let mut handle = std::ptr::null_mut();
            check(open(&mut handle), "pawnio_open")?;
            let io = Self { handle, exec, close };
            check(load(handle, INTEL_MSR.as_ptr(), INTEL_MSR.len()), "pawnio_load(IntelMSR)")?;
            Ok(io)
        }
    }

    /// Reads an MSR on the CPU the calling thread runs on.
    fn read_msr(&self, msr: u64) -> Result<u64, String> {
        let (mut out, mut returned) = (0u64, 0usize);
        let hr = unsafe {
            (self.exec)(self.handle, c"ioctl_read_msr".as_ptr().cast(), &msr, 1, &mut out, 1, &mut returned)
        };
        check(hr, "ioctl_read_msr").map(|_| out)
    }
}

impl Drop for PawnIo {
    fn drop(&mut self) {
        unsafe { (self.close)(self.handle) };
    }
}

fn check(hr: i32, what: &str) -> Result<(), String> {
    if hr >= 0 {
        Ok(())
    } else if hr as u32 == 0x8007_0005 {
        Err(format!("{what}: access denied (run as administrator)"))
    } else {
        Err(format!("{what}: HRESULT 0x{:08X}", hr as u32))
    }
}

struct Core {
    /// Higher means faster: P-cores have a higher efficiency class than E-cores.
    efficiency: u8,
    group: u16,
    mask: usize,
}

fn cores() -> Vec<Core> {
    let mut len = 0u32;
    unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, std::ptr::null_mut(), &mut len) };
    let mut buf = vec![0u8; len as usize];
    if unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, buf.as_mut_ptr().cast(), &mut len) } == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut off = 0;
    while off < len as usize {
        // Records are variable-sized and packed in a byte buffer, so copy each one out unaligned.
        let info: SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX =
            unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off).cast()) };
        let p = unsafe { info.Anonymous.Processor };
        let g = p.GroupMask[0]; // a core never spans processor groups
        out.push(Core { efficiency: p.EfficiencyClass, group: g.Group, mask: g.Mask });
        off += info.Size as usize;
    }
    out
}

fn pin(group: u16, mask: usize) -> GROUP_AFFINITY {
    let want = GROUP_AFFINITY { Mask: mask, Group: group, Reserved: [0; 3] };
    let mut previous = GROUP_AFFINITY { Mask: 0, Group: 0, Reserved: [0; 3] };
    unsafe { SetThreadGroupAffinity(GetCurrentThread(), &want, &mut previous) };
    previous
}

/// Digital readout is "degrees below TjMax", bits 22:16, valid when bit 31 is set.
fn decode(status: u64, tjmax: u64) -> Option<i64> {
    (status >> 31 & 1 == 1).then(|| tjmax as i64 - (status >> 16 & 0x7F) as i64)
}

struct Reading {
    package: Option<i64>,
    cores: Vec<Option<i64>>,
    took: Duration,
}

fn read_all(io: &PawnIo, cores: &[Core], tjmax: &[u64]) -> Reading {
    let start = Instant::now();
    let mut restore = None;
    let mut temps = Vec::with_capacity(cores.len());
    for (core, &tj) in cores.iter().zip(tjmax) {
        let previous = pin(core.group, 1 << core.mask.trailing_zeros());
        restore.get_or_insert(previous);
        temps.push(io.read_msr(IA32_THERM_STATUS).ok().and_then(|v| decode(v, tj)));
    }
    let package = io.read_msr(IA32_PACKAGE_THERM_STATUS).ok().and_then(|v| decode(v, tjmax[0]));
    if let Some(p) = restore {
        pin(p.Group, p.Mask);
    }
    Reading { package, cores: temps, took: start.elapsed() }
}

fn run(log: &mut String) -> Result<(), String> {
    let cores = cores();
    let top = cores.iter().map(|c| c.efficiency).max().unwrap_or(0);
    let names: Vec<String> = {
        let (mut p, mut e) = (0, 0);
        cores
            .iter()
            .map(|c| if c.efficiency == top { p += 1; format!("P{p}") } else { e += 1; format!("E{e}") })
            .collect()
    };
    writeln!(log, "Cores: {} ({} logical)", cores.len(), cores.iter().map(|c| c.mask.count_ones()).sum::<u32>()).unwrap();

    let io = PawnIo::open()?;
    let tjmax: Vec<u64> = cores
        .iter()
        .map(|c| {
            let prev = pin(c.group, 1 << c.mask.trailing_zeros());
            let v = io.read_msr(IA32_TEMPERATURE_TARGET).map(|v| v >> 16 & 0xFF).unwrap_or(100);
            pin(prev.Group, prev.Mask);
            v
        })
        .collect();
    writeln!(log, "TjMax per core: {tjmax:?}").unwrap();

    let show = |log: &mut String, label: &str, r: &Reading| {
        let known: Vec<i64> = r.cores.iter().flatten().copied().collect();
        writeln!(
            log,
            "{label:>8}  package {:>4}  cores min {:>3} max {:>3}  read in {:>4} µs",
            r.package.map_or("n/a".into(), |t| format!("{t}°")),
            known.iter().min().map_or("n/a".into(), |t| format!("{t}°")),
            known.iter().max().map_or("n/a".into(), |t| format!("{t}°")),
            r.took.as_micros()
        )
        .unwrap();
    };
    let detail = |log: &mut String, r: &Reading| {
        let line: Vec<String> = names
            .iter()
            .zip(&r.cores)
            .map(|(n, t)| format!("{n}={}", t.map_or("n/a".into(), |t| t.to_string())))
            .collect();
        writeln!(log, "          {}", line.join(" ")).unwrap();
    };

    let mut peak: Option<Reading> = None;
    let burning = AtomicBool::new(false);
    std::thread::scope(|s| {
        for _ in 0..std::thread::available_parallelism().map_or(1, |n| n.get()) {
            s.spawn(|| {
                while !burning.load(Ordering::Relaxed) && !STOP.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(50));
                }
                let mut x = 1u64;
                while burning.load(Ordering::Relaxed) {
                    x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
                }
            });
        }
        for second in 0..20 {
            match second {
                3 => burning.store(true, Ordering::Relaxed),
                13 => burning.store(false, Ordering::Relaxed),
                _ => {}
            }
            let r = read_all(&io, &cores, &tjmax);
            let label = match second {
                0..3 => "idle",
                3..13 => "load",
                _ => "cooling",
            };
            show(log, &format!("{label} {second:>2}s"), &r);
            if second == 0 {
                detail(log, &r);
            }
            if peak.as_ref().is_none_or(|p| r.package > p.package) {
                peak = Some(r);
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        STOP.store(true, Ordering::Relaxed);
    });
    if let Some(p) = &peak {
        writeln!(log, "Hottest reading, per core:").unwrap();
        detail(log, p);
    }
    Ok(())
}

static STOP: AtomicBool = AtomicBool::new(false);

fn main() {
    let mut log = String::new();
    if let Err(e) = run(&mut log) {
        writeln!(log, "ERROR: {e}").unwrap();
    }
    print!("{log}");
    if let Some(path) = std::env::args().nth(1) {
        std::fs::write(path, &log).expect("cannot write the output file");
    }
}
