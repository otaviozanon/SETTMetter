//! Performance Data Helper (PDH) query for CPU thermal-zone, GPU engine and GPU memory
//! wildcard counters.

use windows_sys::Win32::System::Performance::*;

const PDH_FMT_NOCAP100: u32 = 0x0000_8000;

/// One PDH wildcard counter's formatted samples (`\GPU Engine(*)\...`, etc.), re-fetched into a
/// growable buffer, grown on `PDH_MORE_DATA`.
pub(crate) struct PdhWildcard {
    pub(crate) counter: isize,
    /// u64-backed so the buffer is 8-byte aligned (`PDH_FMT_COUNTERVALUE_ITEM_W` holds a pointer).
    buf: Vec<u64>,
}

impl PdhWildcard {
    fn byte_ptr(&mut self) -> *mut u8 {
        self.buf.as_mut_ptr() as *mut u8
    }

    pub(crate) fn items(&mut self) -> &[PDH_FMT_COUNTERVALUE_ITEM_W] {
        if self.counter == 0 {
            return &[];
        }
        let mut size = (self.buf.len() * 8) as u32;
        let mut count = 0u32;
        let fmt = PDH_FMT_DOUBLE | PDH_FMT_NOCAP100;
        let mut status =
            unsafe { PdhGetFormattedCounterArrayW(self.counter, fmt, &mut size, &mut count, self.byte_ptr() as *mut _) };
        if status == PDH_MORE_DATA {
            self.buf.resize(size.div_ceil(8) as usize, 0);
            status =
                unsafe { PdhGetFormattedCounterArrayW(self.counter, fmt, &mut size, &mut count, self.byte_ptr() as *mut _) };
        }
        if status != 0 {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(self.buf.as_ptr() as *const PDH_FMT_COUNTERVALUE_ITEM_W, count as usize) }
    }
}

pub(crate) fn good(item: &PDH_FMT_COUNTERVALUE_ITEM_W) -> bool {
    item.FmtValue.CStatus == PDH_CSTATUS_VALID_DATA || item.FmtValue.CStatus == PDH_CSTATUS_NEW_DATA
}

/// Reads a PDH counter's wide instance name as a lossy `String`, used to match an adapter's
/// LUID substring.
pub(crate) fn instance_name(item: &PDH_FMT_COUNTERVALUE_ITEM_W) -> String {
    if item.szName.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    unsafe {
        while *item.szName.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(item.szName, len))
    }
}

/// PDH query bundling the CPU-temperature thermal zone counter and, when a discrete GPU was
/// detected, the GPU usage/VRAM counters.
pub(crate) struct PdhQuery {
    query: isize,
    pub(crate) therm: PdhWildcard,
    pub(crate) eng: PdhWildcard,
    pub(crate) mem: PdhWildcard,
    /// Per-process counters for the "top process" tooltip line: CPU time, private working set
    /// and PID (the latter to turn a `\Process(name#n)\...` instance into a real PID, which is
    /// also how GPU-Engine instance names encode which process owns each engine).
    pub(crate) proc_cpu: PdhWildcard,
    pub(crate) proc_mem: PdhWildcard,
    pub(crate) proc_id: PdhWildcard,
}

fn pdh_add(query: isize, path: &str) -> isize {
    let wide = crate::wide(path);
    let mut counter = 0isize;
    if unsafe { PdhAddEnglishCounterW(query, wide.as_ptr(), 0, &mut counter) } == 0 {
        counter
    } else {
        0
    }
}

impl PdhQuery {
    pub(crate) fn build(want_cpu_temp: bool, has_gpu: bool) -> Option<PdhQuery> {
        let mut query = 0isize;
        if unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut query) } != 0 {
            return None;
        }
        let therm = if want_cpu_temp {
            pdh_add(query, r"\Thermal Zone Information(*)\Temperature")
        } else {
            0
        };
        let eng = if has_gpu { pdh_add(query, r"\GPU Engine(*)\Utilization Percentage") } else { 0 };
        let mem = if has_gpu { pdh_add(query, r"\GPU Adapter Memory(*)\Dedicated Usage") } else { 0 };
        let proc_cpu = pdh_add(query, r"\Process(*)\% Processor Time");
        let proc_mem = pdh_add(query, r"\Process(*)\Working Set - Private");
        let proc_id = pdh_add(query, r"\Process(*)\ID Process");
        unsafe { PdhCollectQueryData(query) };
        Some(PdhQuery {
            query,
            therm: PdhWildcard { counter: therm, buf: Vec::new() },
            eng: PdhWildcard { counter: eng, buf: Vec::new() },
            mem: PdhWildcard { counter: mem, buf: Vec::new() },
            proc_cpu: PdhWildcard { counter: proc_cpu, buf: Vec::new() },
            proc_mem: PdhWildcard { counter: proc_mem, buf: Vec::new() },
            proc_id: PdhWildcard { counter: proc_id, buf: Vec::new() },
        })
    }

    pub(crate) fn collect(&mut self) -> bool {
        unsafe { PdhCollectQueryData(self.query) == 0 }
    }
}

impl Drop for PdhQuery {
    fn drop(&mut self) {
        unsafe { PdhCloseQuery(self.query) };
    }
}
