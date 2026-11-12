//! NVIDIA GPU metrics through NVML, which ships with the NVIDIA driver. No admin needed.

use nvml_wrapper::Nvml;
use nvml_wrapper::bitmasks::device::ThrottleReasons;
use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
use serde::Serialize;

#[derive(Serialize, Clone)]
pub struct GpuSample {
    pub load: Option<u32>,
    pub temp: Option<u32>,
    pub vram_used: Option<u64>,
    pub power_w: Option<f32>,
    pub power_limit_w: Option<f32>,
    pub clock_mhz: Option<u32>,
    pub mem_clock_mhz: Option<u32>,
    pub fan: Option<u32>,
    /// Video encoder (NVENC) and decoder load in %.
    pub encoder: Option<u32>,
    pub decoder: Option<u32>,
    /// Current PCIe link, e.g. "Gen 4 ×16"; it drops at idle to save power.
    pub pcie: Option<String>,
    /// Why clocks are held below their maximum right now, in plain words.
    pub held_by: Vec<&'static str>,
}

/// What does not change while the driver stays loaded.
#[derive(Serialize, Clone)]
pub struct GpuRated {
    pub driver: Option<String>,
    pub max_clock_mhz: Option<u32>,
    pub max_mem_clock_mhz: Option<u32>,
    pub default_power_limit_w: Option<f32>,
    pub pcie_max: Option<String>,
}

pub struct Gpu {
    nvml: Nvml,
}

fn pcie(generation: Option<u32>, width: Option<u32>) -> Option<String> {
    Some(format!("Gen {} ×{}", generation?, width?))
}

/// Idle and display clocks are normal operation, so they are not listed.
fn held_by(reasons: ThrottleReasons) -> Vec<&'static str> {
    [
        (ThrottleReasons::SW_POWER_CAP, "power limit"),
        (ThrottleReasons::HW_POWER_BRAKE_SLOWDOWN, "power brake"),
        (ThrottleReasons::SW_THERMAL_SLOWDOWN, "heat"),
        (ThrottleReasons::HW_THERMAL_SLOWDOWN, "heat (hardware)"),
        (ThrottleReasons::HW_SLOWDOWN, "hardware slowdown"),
        (ThrottleReasons::APPLICATIONS_CLOCKS_SETTING, "clock setting"),
        (ThrottleReasons::SYNC_BOOST, "sync boost"),
    ]
    .into_iter()
    .filter(|(flag, _)| reasons.contains(*flag))
    .map(|(_, name)| name)
    .collect()
}

impl Gpu {
    /// Returns the GPU, its name, VRAM size and rated values, or a sentence saying why there is none.
    // shortcut: first NVIDIA GPU only; other vendors get the generic Windows GPU counters in v1.1.
    pub fn open() -> Result<(Self, String, u64, GpuRated), String> {
        let path = super::paths::system32().join("nvml.dll");
        let nvml = Nvml::builder()
            .lib_path(path.as_os_str())
            .init()
            .map_err(|_| "No NVIDIA GPU found".to_string())?;
        let device = nvml.device_by_index(0).map_err(|_| "No NVIDIA GPU found".to_string())?;
        let name = device.name().unwrap_or_else(|_| "NVIDIA GPU".into());
        let vram = device.memory_info().map(|m| m.total).unwrap_or(0);
        let rated = GpuRated {
            driver: nvml.sys_driver_version().ok(),
            max_clock_mhz: device.max_clock_info(Clock::Graphics).ok(),
            max_mem_clock_mhz: device.max_clock_info(Clock::Memory).ok(),
            default_power_limit_w: device.power_management_limit_default().ok().map(|mw| mw as f32 / 1000.0),
            pcie_max: pcie(device.max_pcie_link_gen().ok(), device.max_pcie_link_width().ok()),
        };
        drop(device);
        Ok((Self { nvml }, name, vram, rated))
    }

    pub fn read(&self) -> Option<GpuSample> {
        let d = self.nvml.device_by_index(0).ok()?;
        Some(GpuSample {
            load: d.utilization_rates().ok().map(|u| u.gpu),
            temp: d.temperature(TemperatureSensor::Gpu).ok(),
            vram_used: d.memory_info().ok().map(|m| m.used),
            power_w: d.power_usage().ok().map(|mw| mw as f32 / 1000.0),
            power_limit_w: d.enforced_power_limit().ok().map(|mw| mw as f32 / 1000.0),
            clock_mhz: d.clock_info(Clock::Graphics).ok(),
            mem_clock_mhz: d.clock_info(Clock::Memory).ok(),
            fan: d.fan_speed(0).ok(),
            encoder: d.encoder_utilization().ok().map(|u| u.utilization),
            decoder: d.decoder_utilization().ok().map(|u| u.utilization),
            pcie: pcie(d.current_pcie_link_gen().ok(), d.current_pcie_link_width().ok()),
            held_by: d.current_throttle_reasons().map(held_by).unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_only_the_reasons_that_matter() {
        assert!(held_by(ThrottleReasons::GPU_IDLE | ThrottleReasons::DISPLAY_CLOCK_SETTING).is_empty());
        assert_eq!(held_by(ThrottleReasons::SW_POWER_CAP | ThrottleReasons::GPU_IDLE), ["power limit"]);
        assert_eq!(pcie(Some(4), Some(16)).as_deref(), Some("Gen 4 ×16"));
        assert_eq!(pcie(Some(4), None), None);
    }
}
