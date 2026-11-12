//! Gathers every source into one `Sample` per tick, plus the static `Info` sent once.

use crate::win::{clock::Clock, cpu_temp, gpu, memory, net, topology};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use sysinfo::{DiskKind, DiskRefreshKind, Disks, ProcessRefreshKind, ProcessesToUpdate, System};

/// What does not change while Corelume runs. Each `*_error` says why a source is missing.
#[derive(Serialize, Clone)]
pub struct Info {
    /// Filled in by main: the app version and whether it runs as administrator.
    pub version: String,
    pub elevated: bool,
    pub cpu_name: String,
    pub cores: Vec<topology::Core>,
    pub tjmax: Option<u32>,
    pub cpu_base_mhz: Option<u32>,
    pub cpu_max_mhz: Option<u32>,
    pub temp_error: Option<String>,
    pub gpu_name: Option<String>,
    pub vram_total: Option<u64>,
    pub gpu_rated: Option<gpu::GpuRated>,
    pub gpu_error: Option<String>,
    pub ram_total: u64,
}

#[derive(Serialize, Clone)]
pub struct Sample {
    /// Load in %, total and per logical processor.
    pub cpu: f32,
    pub threads: Vec<f32>,
    pub clock_ghz: Option<f32>,
    pub cpu_temp: Option<f32>,
    /// Per physical core, in `Info::cores` order; empty when temperatures are unavailable.
    pub core_temps: Vec<Option<f32>>,
    /// Per physical core: busy clock in MHz.
    pub core_clocks: Vec<Option<f32>>,
    pub cpu_vid: Option<f32>,
    /// Package watts, the enabled PL1 and PL2 limits, and "heat" or "power" while held back.
    pub cpu_power: Option<f32>,
    pub cpu_pl1: Option<f32>,
    pub cpu_pl2: Option<f32>,
    pub cpu_limit: Option<&'static str>,
    pub ram_used: u64,
    pub ram_commit: Option<u64>,
    pub ram_commit_limit: Option<u64>,
    pub ram_cached: Option<u64>,
    /// Bytes per second over physical adapters, and bytes since Corelume started.
    pub net_down: u64,
    pub net_up: u64,
    pub net_rx: u64,
    pub net_tx: u64,
    pub adapters: Vec<AdapterSample>,
    pub disks: Vec<DiskSample>,
    pub gpu: Option<gpu::GpuSample>,
    /// Extra data for the page open in the main window, absent from the history.
    pub detail: Option<Detail>,
}

#[derive(Serialize, Clone)]
pub struct AdapterSample {
    pub name: String,
    pub down: u64,
    pub up: u64,
    pub rx: u64,
    pub tx: u64,
}

#[derive(Serialize, Clone)]
pub struct DiskSample {
    /// "C:"
    pub mount: String,
    pub label: String,
    /// "SSD", "HDD" or "".
    pub kind: &'static str,
    pub fs: String,
    pub removable: bool,
    pub total: u64,
    pub free: u64,
    /// Bytes per second.
    pub read: u64,
    pub write: u64,
}

#[derive(Serialize, Clone, Default)]
pub struct Detail {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_cpu: Option<Vec<Proc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_ram: Option<Vec<Proc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modules: Option<Vec<memory::Module>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<memory::Breakdown>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapters: Option<Vec<net::Details>>,
}

/// Processes grouped by executable name, like Task Manager's app rows.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Proc {
    pub name: String,
    pub count: u32,
    /// % of the whole CPU.
    pub cpu: f32,
    /// Bytes: working set on the CPU page, private working set on the Memory page (as Task
    /// Manager's Processes tab shows it, so the list adds up to "Programs").
    pub ram: u64,
}

/// Rows in the busiest-programs list (CPU page) and the largest-programs list (Memory page,
/// a taller card).
const TOP_CPU: usize = 8;
const TOP_RAM: usize = 15;

pub struct Sampler {
    sys: System,
    disks: Option<Disks>,
    clock: Option<Clock>,
    readings: Option<Arc<Mutex<cpu_temp::Readings>>>,
    gpu: Option<gpu::Gpu>,
    net_prev: HashMap<u64, (String, u64, u64)>,
    net_totals: HashMap<u64, (u64, u64)>,
    at: Instant,
    net_rx: u64,
    net_tx: u64,
    modules: Option<Vec<memory::Module>>,
    /// Opened the first time the Memory page is shown.
    mem_counters: Option<memory::Counters>,
}

impl Sampler {
    pub fn new() -> Result<(Self, Info), String> {
        let mut sys = System::new().map_err(|e| format!("system counters unavailable: {e}"))?;
        sys.refresh_cpu_all();
        sys.refresh_memory();
        let cores = topology::cores();
        let (readings, rated, temp_error) = match cpu_temp::start(&cores) {
            Ok((r, rated)) => (Some(r), Some(rated), None),
            Err(e) => (None, None, Some(e)),
        };
        let (gpu, gpu_name, vram_total, gpu_rated, gpu_error) = match gpu::Gpu::open() {
            Ok((g, name, vram, rated)) => (Some(g), Some(name), Some(vram), Some(rated), None),
            Err(e) => (None, None, None, None, Some(e)),
        };
        let info = Info {
            version: String::new(),
            elevated: false,
            cpu_name: clean_cpu_name(sys.cpus().first().map_or("", |c| c.brand())),
            cores,
            tjmax: rated.as_ref().map(|r| r.tjmax),
            cpu_base_mhz: rated.as_ref().and_then(|r| r.base_mhz),
            cpu_max_mhz: rated.as_ref().and_then(|r| r.max_mhz),
            temp_error,
            gpu_name,
            vram_total,
            gpu_rated,
            gpu_error,
            ram_total: sys.total_memory(),
        };
        let sampler = Self {
            sys,
            disks: Disks::new_with_refreshed_list().ok(),
            clock: Clock::new(),
            readings,
            gpu,
            net_prev: net::counters(),
            net_totals: HashMap::new(),
            at: Instant::now(),
            net_rx: 0,
            net_tx: 0,
            modules: None,
            mem_counters: None,
        };
        Ok((sampler, info))
    }

    /// `page` is the page open in the main window ("" when it is closed); it decides the `detail`.
    pub fn tick(&mut self, page: &str) -> Sample {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        let secs = self.at.elapsed().as_secs_f64().max(0.001);
        self.at = Instant::now();
        let per_sec = |bytes: u64| (bytes as f64 / secs) as u64;

        let now = net::counters();
        let mut adapters = Vec::new();
        for (luid, (name, i, o)) in &now {
            if let Some((_, pi, po)) = self.net_prev.get(luid) {
                let (rx, tx) = (i.saturating_sub(*pi), o.saturating_sub(*po));
                let total = self.net_totals.entry(*luid).or_default();
                *total = (total.0 + rx, total.1 + tx);
                adapters.push(AdapterSample { name: name.clone(), down: per_sec(rx), up: per_sec(tx), rx: total.0, tx: total.1 });
            }
        }
        adapters.sort_by(|a, b| a.name.cmp(&b.name));
        let (rx, tx) = net_delta(&self.net_prev, &now);
        let luids: Vec<u64> = now.keys().copied().collect();
        self.net_prev = now;
        self.net_rx += rx;
        self.net_tx += tx;

        let disks = self.disks.as_mut().map_or_else(Vec::new, |d| {
            d.refresh_specifics(true, DiskRefreshKind::nothing().with_storage().with_io_usage());
            d
                .iter()
                .map(|d| {
                    let usage = d.usage();
                    DiskSample {
                        mount: d.mount_point().to_string_lossy().trim_end_matches('\\').to_string(),
                        label: d.name().to_string_lossy().into_owned(),
                        kind: match d.kind() {
                            DiskKind::SSD => "SSD",
                            DiskKind::HDD => "HDD",
                            _ => "",
                        },
                        fs: d.file_system().to_string_lossy().into_owned(),
                        removable: d.is_removable(),
                        total: d.total_space(),
                        free: d.available_space(),
                        read: per_sec(usage.read_bytes),
                        write: per_sec(usage.written_bytes),
                    }
                })
                .collect::<Vec<_>>()
        });
        let mut disks = disks;
        disks.sort_by(|a, b| a.mount.cmp(&b.mount));

        let detail = match page {
            "cpu" => Some(Detail { top_cpu: Some(top(self.processes(), TOP_CPU, |p| p.cpu)), ..Detail::default() }),
            "memory" => {
                if self.mem_counters.is_none() {
                    self.mem_counters = memory::Counters::new();
                }
                let total = self.sys.total_memory();
                let read = self.mem_counters.as_mut().and_then(|c| c.read(total));
                let top_ram = read.as_ref().map(|(_, sets)| {
                    let procs = sets
                        .iter()
                        .filter(|(name, _)| name.as_str() != "Memory Compression")
                        .map(|(name, &(ram, count))| Proc { name: name.clone(), count, cpu: 0.0, ram })
                        .collect();
                    top(procs, TOP_RAM, |p| p.ram as f32)
                });
                Some(Detail {
                    top_ram,
                    memory: read.map(|(b, _)| b),
                    modules: Some(self.modules.get_or_insert_with(memory::modules).clone()),
                    ..Detail::default()
                })
            }
            "network" => Some(Detail { adapters: Some(net::details(&luids)), ..Detail::default() }),
            _ => None,
        };

        let r = self.readings.as_ref().and_then(|t| t.lock().ok().map(|t| t.clone())).unwrap_or_default();
        let commit = memory::commit_and_cache();
        Sample {
            cpu: self.sys.global_cpu_usage(),
            threads: self.sys.cpus().iter().map(|c| c.usage()).collect(),
            clock_ghz: self.clock.as_mut().and_then(Clock::ghz),
            cpu_temp: r.package,
            core_temps: r.cores,
            core_clocks: r.clocks,
            cpu_vid: r.vid,
            cpu_power: r.power,
            cpu_pl1: r.pl1,
            cpu_pl2: r.pl2,
            cpu_limit: r.limit,
            ram_used: self.sys.used_memory(),
            ram_commit: commit.map(|c| c.0),
            ram_commit_limit: commit.map(|c| c.1),
            ram_cached: commit.map(|c| c.2),
            net_down: per_sec(rx),
            net_up: per_sec(tx),
            net_rx: self.net_rx,
            net_tx: self.net_tx,
            adapters,
            disks,
            gpu: self.gpu.as_ref().and_then(gpu::Gpu::read),
            detail,
        }
    }

    /// Refreshing every process costs a few milliseconds, so only the CPU and Memory pages ask.
    fn processes(&mut self) -> Vec<Proc> {
        self.sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_cpu().with_memory());
        let threads = self.sys.cpus().len().max(1) as f32;
        let mut by_name: HashMap<String, Proc> = HashMap::new();
        for p in self.sys.processes().values() {
            let name = p.name().to_string_lossy().into_owned();
            let e = by_name.entry(name.clone()).or_insert(Proc { name, count: 0, cpu: 0.0, ram: 0 });
            e.count += 1;
            e.cpu += p.cpu_usage() / threads;
            e.ram += p.memory();
        }
        by_name.into_values().collect()
    }
}

/// The `n` largest by `key`, largest first; idle "System Idle Process" never makes the list.
fn top(mut procs: Vec<Proc>, n: usize, key: impl Fn(&Proc) -> f32) -> Vec<Proc> {
    procs.retain(|p| p.name != "System Idle Process");
    procs.sort_by(|a, b| key(b).total_cmp(&key(a)));
    procs.truncate(n);
    procs
}

/// Bytes moved between two counter snapshots. Only adapters present in both count, and a
/// counter that went backwards (adapter reset) counts as zero, so a Wi-Fi link coming up
/// never shows its whole since-boot total as one giant spike.
fn net_delta(prev: &HashMap<u64, (String, u64, u64)>, now: &HashMap<u64, (String, u64, u64)>) -> (u64, u64) {
    now.iter()
        .filter_map(|(id, (_, i, o))| prev.get(id).map(|(_, pi, po)| (i.saturating_sub(*pi), o.saturating_sub(*po))))
        .fold((0, 0), |(a, b), (i, o)| (a + i, b + o))
}

/// "Intel(R) Core(TM) i7-8700K CPU @ 3.70GHz" → "Intel Core i7-8700K".
fn clean_cpu_name(brand: &str) -> String {
    let brand = brand.replace("(R)", "").replace("(TM)", "");
    let brand = brand.split(" @ ").next().unwrap_or_default();
    brand.split_whitespace().filter(|w| *w != "CPU" && *w != "Processor").collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(rows: &[(u64, u64, u64)]) -> HashMap<u64, (String, u64, u64)> {
        rows.iter().map(|&(id, i, o)| (id, (format!("if{id}"), i, o))).collect()
    }

    #[test]
    fn net_delta_ignores_new_and_reset_adapters() {
        let prev = snap(&[(1, 1_000, 500), (2, 9_000, 9_000)]);
        // Adapter 1 moved 300/100 bytes, adapter 2 was reset, adapter 3 just appeared.
        let now = snap(&[(1, 1_300, 600), (2, 10, 10), (3, 5_000_000_000, 1)]);
        assert_eq!(net_delta(&prev, &now), (300, 100));
        assert_eq!(net_delta(&HashMap::new(), &now), (0, 0));
    }

    #[test]
    fn top_sorts_and_skips_idle() {
        let p = |name: &str, cpu: f32| Proc { name: name.into(), count: 1, cpu, ram: 0 };
        let list = top(vec![p("a", 1.0), p("System Idle Process", 90.0), p("b", 5.0), p("c", 3.0)], 2, |p| p.cpu);
        assert_eq!(list.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["b", "c"]);
    }

    #[test]
    #[ignore = "reads this machine's sensors: cargo test -- --ignored --nocapture"]
    fn live_sources() {
        let (mut sampler, info) = Sampler::new().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1500));
        sampler.tick("memory");
        std::thread::sleep(std::time::Duration::from_millis(1000));
        let s = sampler.tick("memory");
        let kinds: String = info.cores.iter().map(|c| c.kind).collect();
        let clusters: Vec<u32> = info.cores.iter().map(|c| c.cluster).collect();
        println!("cpu {:?}, cores {kinds}, clusters {clusters:?}", info.cpu_name);
        println!("temps: tjmax {:?}, error {:?}, package {:?}", info.tjmax, info.temp_error, s.cpu_temp);
        println!("power {:?} W, PL1 {:?}, PL2 {:?}, limit {:?}, vid {:?}", s.cpu_power, s.cpu_pl1, s.cpu_pl2, s.cpu_limit, s.cpu_vid);
        println!("gpu {:?} vram {:?} error {:?}", info.gpu_name, info.vram_total, info.gpu_error);
        if let Some(g) = &s.gpu {
            println!("gpu load {:?} temp {:?} power {:?}/{:?} clock {:?}/{:?} enc {:?} pcie {:?} held {:?}", g.load, g.temp, g.power_w, g.power_limit_w, g.clock_mhz, g.mem_clock_mhz, g.encoder, g.pcie, g.held_by);
        }
        println!("cpu {:.1}% clock {:?} GHz, ram {} / {} MB, commit {:?}, cached {:?}", s.cpu, s.clock_ghz, s.ram_used >> 20, info.ram_total >> 20, s.ram_commit.map(|b| b >> 20), s.ram_cached.map(|b| b >> 20));
        for d in &s.disks {
            println!("disk {} {:?} {} {} free {} / {} GB, r {} w {} B/s", d.mount, d.label, d.kind, d.fs, d.free >> 30, d.total >> 30, d.read, d.write);
        }
        for a in &s.adapters {
            println!("net {:?} down {} up {}", a.name, a.down, a.up);
        }
        let detail = s.detail.unwrap();
        println!("modules {:?}", detail.modules);
        println!("breakdown {:?}", detail.memory);
        println!("top ram {:?}", detail.top_ram.unwrap().iter().map(|p| (&p.name, p.count, p.ram >> 20)).collect::<Vec<_>>());
        let net = sampler.tick("network").detail.unwrap().adapters.unwrap();
        println!("adapters {:?}", net.iter().map(|a| (&a.name, &a.description, a.link_bps, &a.ips)).collect::<Vec<_>>());
        assert!(!info.cores.is_empty());
        assert_eq!(s.threads.len(), info.cores.iter().map(|c| c.threads.len()).sum::<usize>());
    }

    #[test]
    fn cpu_names_are_cleaned() {
        assert_eq!(clean_cpu_name("Intel(R) Core(TM) i5-12600K"), "Intel Core i5-12600K");
        assert_eq!(clean_cpu_name("Intel(R) Core(TM) i7-8700K CPU @ 3.70GHz"), "Intel Core i7-8700K");
        assert_eq!(clean_cpu_name("AMD Ryzen 7 7800X3D 8-Core Processor"), "AMD Ryzen 7 7800X3D 8-Core");
    }
}
