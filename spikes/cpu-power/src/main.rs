//! Read-only diagnostic of an Intel CPU's power setup and its behaviour under load:
//! RAPL limits (PL1/PL2, time window, lock), ICCMax, package power, temperatures,
//! throttle reasons, effective clocks, core voltage request, and a throughput score
//! to compare before/after a tuning change. It only reads MSRs; it never writes one.
//! Needs admin. Usage: cpu-power-probe.exe [output-file] [load-seconds]

use std::ffi::c_void;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::Win32::System::SystemInformation::{
    GROUP_AFFINITY, GetLogicalProcessorInformationEx, RelationProcessorCore,
    SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentThread, SetThreadGroupAffinity, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};

static INTEL_MSR: &[u8] = include_bytes!("../../../vendor/pawnio/IntelMSR.bin");

const MPERF: u64 = 0xE7;
const APERF: u64 = 0xE8;
const PLATFORM_INFO: u64 = 0xCE;
const PERF_STATUS: u64 = 0x198;
const THERM_STATUS: u64 = 0x19C;
const TEMPERATURE_TARGET: u64 = 0x1A2;
const TURBO_RATIO_LIMIT: u64 = 0x1AD;
const PACKAGE_THERM_STATUS: u64 = 0x1B1;
const VR_CURRENT_CONFIG: u64 = 0x601;
const RAPL_POWER_UNIT: u64 = 0x606;
const PKG_POWER_LIMIT: u64 = 0x610;
const PKG_ENERGY_STATUS: u64 = 0x611;

type Handle = *mut c_void;
type OpenFn = unsafe extern "system" fn(*mut Handle) -> i32;
type LoadFn = unsafe extern "system" fn(Handle, *const u8, usize) -> i32;
type ExecFn =
    unsafe extern "system" fn(Handle, *const u8, *const u64, usize, *mut u64, usize, *mut usize) -> i32;

struct PawnIo {
    handle: Handle,
    exec: ExecFn,
}

impl PawnIo {
    fn open() -> Result<Self, String> {
        let dir = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into());
        let path: Vec<u16> = format!(r"{dir}\PawnIO\PawnIOLib.dll").encode_utf16().chain([0]).collect();
        let lib = unsafe {
            LoadLibraryExW(path.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32)
        };
        if lib.is_null() {
            return Err("PawnIO is not installed".into());
        }
        let sym = |n: &[u8]| unsafe { GetProcAddress(lib, n.as_ptr()) }.ok_or("PawnIOLib.dll is incomplete".to_string());
        unsafe {
            let open: OpenFn = std::mem::transmute(sym(b"pawnio_open\0")?);
            let load: LoadFn = std::mem::transmute(sym(b"pawnio_load\0")?);
            let exec: ExecFn = std::mem::transmute(sym(b"pawnio_execute\0")?);
            let mut handle = std::ptr::null_mut();
            if open(&mut handle) < 0 {
                return Err("pawnio_open failed: run this from an administrator terminal".into());
            }
            if load(handle, INTEL_MSR.as_ptr(), INTEL_MSR.len()) < 0 {
                return Err("IntelMSR module refused to load (not an Intel CPU?)".into());
            }
            Ok(Self { handle, exec })
        }
    }

    fn read(&self, msr: u64) -> Option<u64> {
        let (mut out, mut n) = (0u64, 0usize);
        let hr = unsafe { (self.exec)(self.handle, c"ioctl_read_msr".as_ptr().cast(), &msr, 1, &mut out, 1, &mut n) };
        (hr >= 0).then_some(out)
    }
}

struct Core {
    p: bool,
    group: u16,
    first: usize,
}

fn cores() -> Vec<Core> {
    let mut len = 0u32;
    unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, std::ptr::null_mut(), &mut len) };
    let mut buf = vec![0u8; len as usize];
    if unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, buf.as_mut_ptr().cast(), &mut len) } == 0 {
        return Vec::new();
    }
    let mut raw = Vec::new();
    let mut off = 0;
    while off + 8 <= len as usize {
        let size = u32::from_ne_bytes(buf[off + 4..off + 8].try_into().unwrap()) as usize;
        if size == 0 || off + size > len as usize {
            break;
        }
        let mut info: SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX = unsafe { std::mem::zeroed() };
        let n = size.min(size_of_val(&info));
        unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr().add(off), (&raw mut info).cast::<u8>(), n) };
        let p = unsafe { info.Anonymous.Processor };
        raw.push((p.EfficiencyClass, p.GroupMask[0].Group, p.GroupMask[0].Mask));
        off += size;
    }
    let top = raw.iter().map(|r| r.0).max().unwrap_or(0);
    raw.into_iter().map(|(e, g, m)| Core { p: e == top, group: g, first: 1 << m.trailing_zeros() }).collect()
}

fn pin(group: u16, mask: usize) {
    let want = GROUP_AFFINITY { Mask: mask, Group: group, Reserved: [0; 3] };
    unsafe { SetThreadGroupAffinity(GetCurrentThread(), &want, std::ptr::null_mut()) };
}

/// PL1 sits in bits 31:0 of MSR 0x610, PL2 in bits 63:32, with the same layout.
fn limit(half: u64, power_unit: f64, time_unit: f64) -> String {
    let watts = (half & 0x7FFF) as f64 * power_unit;
    let enabled = half >> 15 & 1 == 1;
    let y = (half >> 17 & 0x1F) as i32;
    let z = (half >> 22 & 0x3) as f64;
    let tau = 2f64.powi(y) * (1.0 + z / 4.0) * time_unit;
    format!("{watts:.0} W, {}, time window {tau:.0} s", if enabled { "enabled" } else { "disabled" })
}

struct Tick {
    package_w: f64,
    package_c: Option<i64>,
    max_core_c: Option<i64>,
    p_mhz: f64,
    e_mhz: f64,
    vid: f64,
    thermal: bool,
    power: bool,
    score: f64,
    read_ms: f64,
}

fn run(log: &mut String, load_secs: u64) -> Result<(), String> {
    let io = PawnIo::open()?;
    let cores = cores();
    let first_p = cores.iter().find(|c| c.p).ok_or("no core found")?;
    pin(first_p.group, first_p.first);
    let units = io.read(RAPL_POWER_UNIT).ok_or("cannot read RAPL units")?;
    let power_unit = 1.0 / (1u64 << (units & 0xF)) as f64;
    let energy_unit = 1.0 / (1u64 << (units >> 8 & 0x1F)) as f64;
    let time_unit = 1.0 / (1u64 << (units >> 16 & 0xF)) as f64;
    let base_ratio = io.read(PLATFORM_INFO).map_or(0, |v| v >> 8 & 0xFF);
    let base_mhz = base_ratio as f64 * 100.0;
    let tjmax = io.read(TEMPERATURE_TARGET).map_or(100, |v| v >> 16 & 0xFF) as i64;

    writeln!(log, "== Configuration (read-only) ==").unwrap();
    writeln!(log, "Cores: {} P + {} E", cores.iter().filter(|c| c.p).count(), cores.iter().filter(|c| !c.p).count()).unwrap();
    writeln!(log, "Base clock: {base_mhz:.0} MHz, TjMax: {tjmax} °C").unwrap();
    if let Some(v) = io.read(TURBO_RATIO_LIMIT) {
        writeln!(log, "Max 1-core turbo ratio: {} ({} MHz)", v & 0xFF, (v & 0xFF) * 100).unwrap();
    }
    if let Some(v) = io.read(PKG_POWER_LIMIT) {
        writeln!(log, "PL1 (long duration):  {}", limit(v & 0xFFFF_FFFF, power_unit, time_unit)).unwrap();
        writeln!(log, "PL2 (short duration): {}", limit(v >> 32, power_unit, time_unit)).unwrap();
        writeln!(log, "Power limit MSR locked by BIOS: {}", if v >> 63 == 1 { "yes" } else { "no" }).unwrap();
    }
    if let Some(v) = io.read(VR_CURRENT_CONFIG) {
        writeln!(log, "ICCMax (current limit): {:.0} A, locked: {}", (v & 0x1FFF) as f64 / 8.0, if v >> 31 & 1 == 1 { "yes" } else { "no" }).unwrap();
    }

    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
    let work = AtomicU64::new(0);
    let burning = AtomicBool::new(false);
    let done = AtomicBool::new(false);
    let mut ticks: Vec<(String, Tick)> = Vec::new();
    std::thread::scope(|s| {
        for _ in 0..std::thread::available_parallelism().map_or(1, |n| n.get()) {
            s.spawn(|| {
                let mut x = 1u64;
                while !done.load(Ordering::Relaxed) {
                    if !burning.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    for _ in 0..100_000 {
                        x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407));
                    }
                    work.fetch_add(1, Ordering::Relaxed);
                }
            });
        }

        let snapshot = |io: &PawnIo| -> (Vec<(u64, u64)>, u64, Instant, u64) {
            let perf: Vec<(u64, u64)> = cores
                .iter()
                .map(|c| {
                    pin(c.group, c.first);
                    (io.read(APERF).unwrap_or(0), io.read(MPERF).unwrap_or(0))
                })
                .collect();
            pin(first_p.group, first_p.first);
            (perf, io.read(PKG_ENERGY_STATUS).unwrap_or(0) & 0xFFFF_FFFF, Instant::now(), work.load(Ordering::Relaxed))
        };
        let mut prev = snapshot(&io);
        let total = 5 + load_secs + 10;
        for second in 0..total {
            if second == 5 {
                burning.store(true, Ordering::Relaxed);
            }
            if second == 5 + load_secs {
                burning.store(false, Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_secs(1));
            let started = Instant::now();
            let now = snapshot(&io);
            let mut temps = Vec::new();
            for c in &cores {
                pin(c.group, c.first);
                if let Some(v) = io.read(THERM_STATUS).filter(|v| v >> 31 & 1 == 1) {
                    temps.push(tjmax - (v >> 16 & 0x7F) as i64);
                }
            }
            pin(first_p.group, first_p.first);
            let pkg = io.read(PACKAGE_THERM_STATUS).unwrap_or(0);
            let vid = io.read(PERF_STATUS).map_or(0.0, |v| (v >> 32 & 0xFFFF) as f64 / 8192.0);
            let read_ms = started.elapsed().as_secs_f64() * 1000.0;
            let secs = now.2.duration_since(prev.2).as_secs_f64();
            let mhz = |p: bool| {
                let v: Vec<f64> = cores
                    .iter()
                    .zip(now.0.iter().zip(&prev.0))
                    .filter(|(c, _)| c.p == p)
                    .filter_map(|(_, ((a1, m1), (a0, m0)))| {
                        let dm = m1.wrapping_sub(*m0);
                        (dm > 0).then(|| base_mhz * a1.wrapping_sub(*a0) as f64 / dm as f64)
                    })
                    .collect();
                if v.is_empty() { 0.0 } else { v.iter().sum::<f64>() / v.len() as f64 }
            };
            let tick = Tick {
                package_w: (now.1.wrapping_sub(prev.1) & 0xFFFF_FFFF) as f64 * energy_unit / secs,
                package_c: (pkg >> 31 & 1 == 1).then(|| tjmax - (pkg >> 16 & 0x7F) as i64),
                max_core_c: temps.iter().max().copied(),
                p_mhz: mhz(true),
                e_mhz: mhz(false),
                vid,
                thermal: pkg & 1 == 1,
                power: pkg >> 10 & 1 == 1,
                score: (now.3 - prev.3) as f64 / secs,
                read_ms,
            };
            let phase = if second < 5 { "idle" } else if second < 5 + load_secs { "load" } else { "cool" };
            ticks.push((phase.into(), tick));
            prev = now;
        }
        done.store(true, Ordering::Relaxed);
    });

    writeln!(log, "\n== Per second ==").unwrap();
    writeln!(log, "phase    pkg W  pkg °C  max core °C  P MHz  E MHz  VID V  limit   score  read ms").unwrap();
    for (i, (phase, t)) in ticks.iter().enumerate() {
        let lim = match (t.thermal, t.power) {
            (true, _) => "thermal",
            (false, true) => "power",
            _ => "-",
        };
        writeln!(
            log,
            "{phase} {i:>2}s {:>6.0}  {:>6}  {:>11}  {:>5.0}  {:>5.0}  {:>5.3}  {lim:<7} {:>6.0}  {:>7.1}",
            t.package_w,
            t.package_c.map_or("n/a".into(), |v| v.to_string()),
            t.max_core_c.map_or("n/a".into(), |v| v.to_string()),
            t.p_mhz,
            t.e_mhz,
            t.vid,
            t.score,
            t.read_ms
        )
        .unwrap();
    }
    let load: Vec<&Tick> = ticks.iter().filter(|(p, _)| p == "load").map(|(_, t)| t).collect();
    let steady = &load[load.len().saturating_sub(load_secs as usize / 2)..];
    let avg = |f: &dyn Fn(&Tick) -> f64| steady.iter().map(|t| f(t)).sum::<f64>() / steady.len().max(1) as f64;
    writeln!(log, "\n== Steady load (last {} s) ==", steady.len()).unwrap();
    writeln!(
        log,
        "package {:.0} W, package {:.0} °C, P-cores {:.0} MHz, E-cores {:.0} MHz, score {:.0}/s, thermal-limited {} of {} s, power-limited {} of {} s",
        avg(&|t| t.package_w),
        avg(&|t| t.package_c.unwrap_or(0) as f64),
        avg(&|t| t.p_mhz),
        avg(&|t| t.e_mhz),
        avg(&|t| t.score),
        steady.iter().filter(|t| t.thermal).count(),
        steady.len(),
        steady.iter().filter(|t| t.power && !t.thermal).count(),
        steady.len()
    )
    .unwrap();
    Ok(())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args.next();
    let load_secs = args.next().and_then(|s| s.parse().ok()).unwrap_or(30);
    let mut log = String::new();
    if let Err(e) = run(&mut log, load_secs) {
        writeln!(log, "ERROR: {e}").unwrap();
    }
    print!("{log}");
    if let Some(path) = out {
        std::fs::write(path, &log).expect("cannot write the output file");
    }
}
