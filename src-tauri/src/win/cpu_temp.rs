//! Intel CPU package and per-core temperatures, package power and power limits, read from
//! MSRs through the PawnIO driver. Needs administrator rights. Validated by spikes/cpu-temp
//! and spikes/cpu-power (see PLAN.md).

use super::topology::Core;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::Win32::System::SystemInformation::GROUP_AFFINITY;
use windows_sys::Win32::System::Threading::{
    GetCurrentThread, SetThreadGroupAffinity, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};

static INTEL_MSR: &[u8] = include_bytes!("../../../vendor/pawnio/IntelMSR.bin");

const IA32_THERM_STATUS: u64 = 0x19C;
const IA32_TEMPERATURE_TARGET: u64 = 0x1A2;
const IA32_PACKAGE_THERM_STATUS: u64 = 0x1B1;
const RAPL_POWER_UNIT: u64 = 0x606;
const PKG_POWER_LIMIT: u64 = 0x610;
const PKG_ENERGY_STATUS: u64 = 0x611;
const PLATFORM_INFO: u64 = 0xCE;
const TURBO_RATIO_LIMIT: u64 = 0x1AD;
const IA32_MPERF: u64 = 0xE7;
const IA32_APERF: u64 = 0xE8;
const IA32_PERF_STATUS: u64 = 0x198;

#[derive(Clone, Default)]
pub struct Readings {
    pub package: Option<f32>,
    /// One entry per physical core, in topology order.
    pub cores: Vec<Option<f32>>,
    /// Package power in watts over the last second.
    pub power: Option<f32>,
    /// Enabled long-duration (PL1) and short-duration (PL2) power limits, in watts.
    pub pl1: Option<f32>,
    pub pl2: Option<f32>,
    /// Why the CPU runs below the speed Windows asked for, right now: "heat" or "power".
    pub limit: Option<&'static str>,
    /// Average clock of each physical core while it was busy over the last second, in MHz.
    pub clocks: Vec<Option<f32>>,
    /// Highest core voltage requested (VID) across cores, in volts.
    pub vid: Option<f32>,
}

/// What the MSRs say once: thermal limit and rated clocks.
pub struct Rated {
    pub tjmax: u32,
    pub base_mhz: Option<u32>,
    pub max_mhz: Option<u32>,
}

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

// The driver handle is a kernel handle, usable from whichever thread owns it.
unsafe impl Send for PawnIo {}

impl PawnIo {
    fn open() -> Result<Self, String> {
        let dir = super::paths::program_files().ok_or("CPU sensors unavailable: Program Files not found")?;
        let path: Vec<u16> = dir.join(r"PawnIO\PawnIOLib.dll").as_os_str().encode_wide().chain([0]).collect();
        // Absolute path, dependencies from its own folder or System32 only: no DLL planting.
        let lib = unsafe {
            LoadLibraryExW(path.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32)
        };
        if lib.is_null() {
            return Err("CPU temperatures and power need the PawnIO driver, which is not installed".into());
        }
        let broken = || "CPU sensors unavailable: the PawnIO install looks broken".to_string();
        let sym = |name: &[u8]| unsafe { GetProcAddress(lib, name.as_ptr()) }.ok_or_else(broken);
        unsafe {
            let open: OpenFn = std::mem::transmute(sym(b"pawnio_open\0")?);
            let load: LoadFn = std::mem::transmute(sym(b"pawnio_load\0")?);
            let exec: ExecFn = std::mem::transmute(sym(b"pawnio_execute\0")?);
            let close: CloseFn = std::mem::transmute(sym(b"pawnio_close\0")?);
            let mut handle = std::ptr::null_mut();
            let hr = open(&mut handle);
            if hr as u32 == 0x8007_0005 {
                return Err("CPU temperatures and power need Corelume to run as administrator".into());
            }
            if hr < 0 {
                return Err(format!("CPU sensors unavailable: PawnIO error 0x{:08X}", hr as u32));
            }
            let io = Self { handle, exec, close };
            // The module refuses to load on non-Intel CPUs.
            if load(handle, INTEL_MSR.as_ptr(), INTEL_MSR.len()) < 0 {
                return Err("CPU temperatures and power are not supported on this processor yet".into());
            }
            Ok(io)
        }
    }

    /// Reads an MSR on the CPU the calling thread runs on.
    fn read_msr(&self, msr: u64) -> Option<u64> {
        let (mut out, mut returned) = (0u64, 0usize);
        let hr = unsafe { (self.exec)(self.handle, c"ioctl_read_msr".as_ptr().cast(), &msr, 1, &mut out, 1, &mut returned) };
        (hr >= 0).then_some(out)
    }
}

impl Drop for PawnIo {
    fn drop(&mut self) {
        unsafe { (self.close)(self.handle) };
    }
}

fn pin(group: u16, mask: usize) {
    let want = GROUP_AFFINITY { Mask: mask, Group: group, Reserved: [0; 3] };
    unsafe { SetThreadGroupAffinity(GetCurrentThread(), &want, std::ptr::null_mut()) };
}

/// Digital readout is "degrees below TjMax", bits 22:16, valid when bit 31 is set.
fn decode(status: u64, tjmax: u64) -> Option<f32> {
    (status >> 31 & 1 == 1).then(|| (tjmax as i64 - (status >> 16 & 0x7F) as i64) as f32)
}

/// Energy counter delta (bits 31:0, wrapping) over `secs`, in watts.
fn watts(prev: u64, now: u64, unit: f64, secs: f64) -> f32 {
    ((now.wrapping_sub(prev) & 0xFFFF_FFFF) as f64 * unit / secs.max(0.001)) as f32
}

/// One half of MSR 0x610: the limit in bits 14:0 (power units), enabled when bit 15 is set.
fn limit_watts(half: u64, unit: f32) -> Option<f32> {
    (half >> 15 & 1 == 1).then(|| (half & 0x7FFF) as f32 * unit)
}

/// IA32_PACKAGE_THERM_STATUS: bit 0 = at the thermal limit now, bit 10 = held back by a power limit now.
fn limit_reason(status: u64) -> Option<&'static str> {
    if status & 1 == 1 {
        Some("heat")
    } else if status >> 10 & 1 == 1 {
        Some("power")
    } else {
        None
    }
}

/// Busy clock from APERF/MPERF deltas: MPERF ticks at the base clock, APERF at the real
/// one, both only while the core runs. None when the core slept the whole interval.
fn busy_mhz(prev: (u64, u64), now: (u64, u64), base_mhz: f32) -> Option<f32> {
    let (da, dm) = (now.0.wrapping_sub(prev.0), now.1.wrapping_sub(prev.1));
    (dm > 0).then(|| (da as f64 / dm as f64 * base_mhz as f64) as f32)
}

/// Opens PawnIO and starts a thread that refreshes the readings every second.
/// Returns the shared latest reading and the rated values, or a sentence saying why it is unavailable.
pub fn start(cores: &[Core]) -> Result<(Arc<Mutex<Readings>>, Rated), String> {
    let io = PawnIo::open()?;
    // Read each core on its first logical processor.
    let targets: Vec<(u16, usize)> = cores.iter().map(|c| (c.group, 1 << c.mask.trailing_zeros())).collect();
    let latest = Arc::new(Mutex::new(Readings::default()));
    let shared = latest.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("cpu-temp".into())
        .spawn(move || {
            // Pinning to a busy core waits for the scheduler: under full load a normal-priority
            // reader took up to 0.4 s per pass in the spike.
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
            let tjmax: Vec<u64> = targets
                .iter()
                .map(|&(g, m)| {
                    pin(g, m);
                    io.read_msr(IA32_TEMPERATURE_TARGET).map_or(100, |v| v >> 16 & 0xFF)
                })
                .collect();
            let tj0 = tjmax.first().copied().unwrap_or(100);
            // Ratios are in 100 MHz steps: base in 0xCE bits 15:8, best 1-core turbo in 0x1AD bits 7:0.
            let base_mhz = io.read_msr(PLATFORM_INFO).map(|v| (v >> 8 & 0xFF) as u32 * 100).filter(|&m| m > 0);
            let max_mhz = io.read_msr(TURBO_RATIO_LIMIT).map(|v| (v & 0xFF) as u32 * 100).filter(|&m| m > 0);
            let _ = tx.send(Rated { tjmax: tj0 as u32, base_mhz, max_mhz });
            let mut perf_prev: Vec<Option<(u64, u64)>> = vec![None; targets.len()];
            // Package MSRs read the same on every core. Units: watts = 1/2^bits 3:0, joules = 1/2^bits 12:8.
            let units = io.read_msr(RAPL_POWER_UNIT);
            let power_unit = units.map(|u| 1.0 / (1u64 << (u & 0xF)) as f32);
            let energy_unit = units.map(|u| 1.0 / (1u64 << (u >> 8 & 0x1F)) as f64);
            let mut energy_prev = io.read_msr(PKG_ENERGY_STATUS).map(|e| (e, Instant::now()));
            loop {
                let (mut cores, mut clocks, mut vid) = (Vec::new(), Vec::new(), None::<f32>);
                for (i, (&(g, m), &tj)) in targets.iter().zip(&tjmax).enumerate() {
                    pin(g, m);
                    cores.push(io.read_msr(IA32_THERM_STATUS).and_then(|v| decode(v, tj)));
                    let perf = io.read_msr(IA32_APERF).zip(io.read_msr(IA32_MPERF));
                    clocks.push(match (perf_prev[i], perf, base_mhz) {
                        (Some(p), Some(n), Some(b)) => busy_mhz(p, n, b as f32),
                        _ => None,
                    });
                    perf_prev[i] = perf;
                    // VID: bits 47:32 in 1/8192 V.
                    if let Some(v) = io.read_msr(IA32_PERF_STATUS).map(|v| (v >> 32 & 0xFFFF) as f32 / 8192.0) {
                        vid = Some(vid.map_or(v, |x| x.max(v)));
                    }
                }
                let status = io.read_msr(IA32_PACKAGE_THERM_STATUS);
                let energy = io.read_msr(PKG_ENERGY_STATUS).map(|e| (e, Instant::now()));
                let power = match (energy_prev, energy, energy_unit) {
                    (Some((e0, t0)), Some((e1, t1)), Some(unit)) => Some(watts(e0, e1, unit, (t1 - t0).as_secs_f64())),
                    _ => None,
                };
                energy_prev = energy;
                // Read every pass: PC makers' performance modes change the limits at run time.
                let (pl1, pl2) = match (io.read_msr(PKG_POWER_LIMIT), power_unit) {
                    (Some(v), Some(unit)) => (limit_watts(v, unit), limit_watts(v >> 32, unit)),
                    _ => (None, None),
                };
                let reading = Readings {
                    package: status.and_then(|v| decode(v, tj0)),
                    cores,
                    power,
                    pl1,
                    pl2,
                    limit: status.and_then(limit_reason),
                    clocks,
                    vid: vid.filter(|&v| v > 0.0),
                };
                if let Ok(mut t) = shared.lock() {
                    *t = reading;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        })
        .map_err(|e| format!("CPU sensors unavailable: {e}"))?;
    let rated = rx.recv().map_err(|_| "CPU sensors unavailable: reader stopped".to_string())?;
    Ok((latest, rated))
}

#[cfg(test)]
mod tests {
    use super::{busy_mhz, decode, limit_reason, limit_watts, watts};

    #[test]
    fn busy_clock_from_aperf_mperf() {
        // APERF ran 1.5x faster than MPERF on a 3400 MHz base: 5100 MHz.
        assert_eq!(busy_mhz((100, 100), (100 + 15_000, 100 + 10_000), 3400.0), Some(5100.0));
        assert_eq!(busy_mhz((5, 7), (5, 7), 3400.0), None);
    }

    #[test]
    fn decodes_power_and_limits() {
        // 2^-14 J units: 16384 units in 1 s is 1 W; the counter wraps at 32 bits.
        let unit = 1.0 / 16384.0;
        assert_eq!(watts(1_000, 1_000 + 125 * 16384, unit, 1.0), 125.0);
        assert_eq!(watts(0xFFFF_FFFF, 16383, unit, 1.0), 1.0);
        // PL1 125 W (1000 units of 1/8 W), enabled; PL2 253 W, enabled.
        let msr: u64 = (1 << 15 | 2024) << 32 | 1 << 15 | 1000;
        assert_eq!(limit_watts(msr, 0.125), Some(125.0));
        assert_eq!(limit_watts(msr >> 32, 0.125), Some(253.0));
        assert_eq!(limit_watts(1000, 0.125), None);
        assert_eq!(limit_reason(1 | 1 << 10), Some("heat"));
        assert_eq!(limit_reason(1 << 10 | 1 << 11), Some("power"));
        assert_eq!(limit_reason(1 << 11), None);
    }

    #[test]
    fn decodes_digital_readout_below_tjmax() {
        // Valid bit set, readout 45 degrees below a TjMax of 100.
        assert_eq!(decode(1 << 31 | 45 << 16, 100), Some(55.0));
        // Bits outside 22:16 are ignored.
        assert_eq!(decode(1 << 31 | 0x80 << 16 | 0xFFFF, 100), Some(100.0));
        assert_eq!(decode(45 << 16, 100), None);
    }
}
