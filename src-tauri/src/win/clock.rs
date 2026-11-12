//! Effective CPU clock, as Task Manager computes it: nominal frequency × % processor performance.

use std::ptr::null_mut;
use windows_sys::Win32::System::Performance::{
    PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PdhAddEnglishCounterW,
    PdhCollectQueryData, PdhGetFormattedCounterValue, PdhOpenQueryW,
};

pub struct Clock {
    query: PDH_HQUERY,
    nominal: PDH_HCOUNTER,
    performance: PDH_HCOUNTER,
}

// PDH handles are plain process-wide handles; the sampler thread owns this.
unsafe impl Send for Clock {}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

impl Clock {
    pub fn new() -> Option<Self> {
        let (mut query, mut nominal, mut performance) = (null_mut(), null_mut(), null_mut());
        unsafe {
            if PdhOpenQueryW(std::ptr::null(), 0, &mut query) != 0
                || PdhAddEnglishCounterW(query, wide(r"\Processor Information(_Total)\Processor Frequency").as_ptr(), 0, &mut nominal) != 0
                || PdhAddEnglishCounterW(query, wide(r"\Processor Information(_Total)\% Processor Performance").as_ptr(), 0, &mut performance) != 0
            {
                return None;
            }
            // "% Processor Performance" is a rate: it needs a first sample to diff against.
            PdhCollectQueryData(query);
        }
        Some(Self { query, nominal, performance })
    }

    pub fn ghz(&mut self) -> Option<f32> {
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return None;
            }
        }
        let read = |counter: PDH_HCOUNTER| {
            let mut v: PDH_FMT_COUNTERVALUE = unsafe { std::mem::zeroed() };
            let ok = unsafe { PdhGetFormattedCounterValue(counter, PDH_FMT_DOUBLE, std::ptr::null_mut(), &mut v) } == 0;
            (ok && v.CStatus == 0).then(|| unsafe { v.Anonymous.doubleValue })
        };
        Some((read(self.nominal)? * read(self.performance)? / 100.0 / 1000.0) as f32)
    }
}
