//! System monitor data layer: CPU/RAM/GPU/VRAM usage and CPU/GPU/SSD temperatures, read from
//! PDH counters, DXGI + D3DKMT/NVML, the PawnIO driver (MSR/SMN) and the storage IOCTLs.

mod disk;
mod gpu;
mod pawnio;
mod pdh;

use disk::Disk;
use gpu::Gpu;
use pawnio::PawnIo;
use pdh::{PdhQuery, good, instance_name};

use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::GetSystemTimes;

// Sensor bits for the single-instance sensors (CPU usage/temperature, RAM usage, SSD
// temperature). GPU/VRAM usage and GPU temperature moved to per-adapter toggles (see
// `hidden_gpu_sensors` in settings.rs) since a system can have more than one discrete GPU.
pub const BIT_CPU: u32 = 1 << 0;
pub const BIT_CPU_TEMP: u32 = 1 << 1;
pub const BIT_RAM: u32 = 1 << 2;
pub const BIT_DISK_TEMP: u32 = 1 << 3;
pub const ALL_SENSORS: u32 = (1 << 4) - 1;
/// (label, bit) in menu order.
pub const SENSOR_NAMES: [(&str, u32); 4] = [
    ("CPU usage", BIT_CPU),
    ("CPU temperature", BIT_CPU_TEMP),
    ("RAM usage", BIT_RAM),
    ("SSD temperature", BIT_DISK_TEMP),
];

// Icon-glyph bits: one per group (CPU, GPU, RAM, VRAM, SSD), independent of the sensor
// show/hide bits above — an extra per-group toggle so each glyph can be turned off without
// hiding the numbers next to it.
pub const ICON_CPU: u32 = 1 << 0;
pub const ICON_GPU: u32 = 1 << 1;
pub const ICON_RAM: u32 = 1 << 2;
pub const ICON_VRAM: u32 = 1 << 3;
pub const ICON_DISK: u32 = 1 << 4;
pub const ALL_ICONS: u32 = (1 << 5) - 1;
/// (label, bit) in menu order.
pub const ICON_NAMES: [(&str, u32); 5] = [
    ("CPU icon", ICON_CPU),
    ("GPU icon", ICON_GPU),
    ("RAM icon", ICON_RAM),
    ("VRAM icon", ICON_VRAM),
    ("SSD icon", ICON_DISK),
];

/// One discrete GPU's readings. Systems with more than one discrete GPU (e.g. a notebook with
/// two dedicated adapters) get one `GpuReading` per adapter instead of a single merged value.
#[derive(Clone, Copy, Default, Debug)]
pub struct GpuReading {
    pub pct: Option<f64>,
    pub temp: Option<f64>,
    pub vram_pct: Option<f64>,
    pub vram_used_gb: f64,
    pub vram_total_gb: f64,
}

#[derive(Clone, Default, Debug)]
pub struct Snapshot {
    pub cpu_pct: Option<f64>,
    pub ram_pct: Option<f64>,
    pub ram_used_gb: f64,
    pub ram_total_gb: f64,
    pub gpus: Vec<GpuReading>,
    pub cpu_temp: Option<f64>,
    pub disk_temp: Option<f64>,
    /// (process name, % CPU time) of the busiest process this sample, for the tray tooltip.
    pub top_cpu_process: Option<(String, f64)>,
    /// (process name, private working set in MB) of the process using the most RAM.
    pub top_ram_process: Option<(String, f64)>,
    /// (process name, summed GPU-engine utilization %) of the process driving the GPU hardest.
    pub top_gpu_process: Option<(String, f64)>,
}

impl Snapshot {
    /// Clears every field whose bit is off in `mask`, or whose per-adapter id (`"gpu{i}"` /
    /// `"gputemp{i}"` / `"vram{i}"`) is in `hidden_gpu`, so hidden sensors never reach the tray
    /// icon or the taskbar label.
    pub fn apply(mut self, mask: u32, hidden_gpu: &[String]) -> Snapshot {
        if mask & BIT_CPU == 0 {
            self.cpu_pct = None;
        }
        if mask & BIT_CPU_TEMP == 0 {
            self.cpu_temp = None;
        }
        if mask & BIT_RAM == 0 {
            self.ram_pct = None;
        }
        for (i, g) in self.gpus.iter_mut().enumerate() {
            if hidden_gpu.iter().any(|h| h == &format!("gpu{i}")) {
                g.pct = None;
            }
            if hidden_gpu.iter().any(|h| h == &format!("gputemp{i}")) {
                g.temp = None;
            }
            if hidden_gpu.iter().any(|h| h == &format!("vram{i}")) {
                g.vram_pct = None;
            }
        }
        if mask & BIT_DISK_TEMP == 0 {
            self.disk_temp = None;
        }
        self
    }

    /// True when every usage/temperature value is hidden: CPU, RAM, SSD, and every GPU/VRAM
    /// adapter. Used to show a fallback tray icon so the right-click menu stays reachable once
    /// the taskbar widget has nothing left to draw.
    pub fn all_hidden(&self) -> bool {
        self.cpu_pct.is_none()
            && self.cpu_temp.is_none()
            && self.ram_pct.is_none()
            && self.disk_temp.is_none()
            && self.gpus.iter().all(|g| g.pct.is_none() && g.temp.is_none() && g.vram_pct.is_none())
    }
}

fn ft(f: FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

/// Orchestrates every sensor: call [`SysMonitor::new`] once, then [`SysMonitor::sample`] on each
/// tick of the widget's poll timer.
pub struct SysMonitor {
    prev_idle: u64,
    prev_total: u64,
    pdh: Option<PdhQuery>,
    gpus: Vec<Gpu>,
    pio: Option<PawnIo>,
    disk: Option<Disk>,
    disk_tick: u32,
    last_disk_temp: Option<f64>,
}

impl SysMonitor {
    pub fn new() -> SysMonitor {
        let gpus = Gpu::detect_all();
        let pio = PawnIo::open();
        let pdh = PdhQuery::build(pio.is_none(), !gpus.is_empty());
        SysMonitor {
            prev_idle: 0,
            prev_total: 0,
            pdh,
            gpus,
            pio,
            disk: Disk::open(),
            disk_tick: 0,
            last_disk_temp: None,
        }
    }

    fn read_cpu(&mut self) -> Option<f64> {
        let (mut idle, mut kernel, mut user): (FILETIME, FILETIME, FILETIME) = unsafe { zeroed3() };
        if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
            return None;
        }
        let idle = ft(idle);
        let total = ft(kernel) + ft(user);
        let result = (self.prev_total != 0 && total > self.prev_total)
            .then(|| 100.0 * (1.0 - (idle - self.prev_idle) as f64 / (total - self.prev_total) as f64));
        self.prev_idle = idle;
        self.prev_total = total;
        result
    }

    fn read_ram() -> (Option<f64>, f64, f64) {
        let mut mem: MEMORYSTATUSEX = unsafe { zeroed() };
        mem.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if unsafe { GlobalMemoryStatusEx(&mut mem) } == 0 {
            return (None, 0.0, 0.0);
        }
        let total = mem.ullTotalPhys as f64 / 1_073_741_824.0;
        let used = (mem.ullTotalPhys - mem.ullAvailPhys) as f64 / 1_073_741_824.0;
        let pct = 100.0 * (mem.ullTotalPhys - mem.ullAvailPhys) as f64 / mem.ullTotalPhys as f64;
        (Some(pct), used, total)
    }

    /// `hidden_gpu` lets the caller skip the GPU-engine/VRAM PDH parsing and the
    /// D3DKMT/NVML temperature queries entirely when every adapter's usage, temperature and
    /// VRAM checkboxes are unchecked in the menu — avoids keeping DXGI/NVML "hot" every tick
    /// for data nobody sees.
    pub fn sample(&mut self, hidden_gpu: &[String]) -> Snapshot {
        let mut s = Snapshot { cpu_pct: self.read_cpu(), ..Default::default() };
        let (ram_pct, ram_used, ram_total) = Self::read_ram();
        s.ram_pct = ram_pct;
        s.ram_used_gb = ram_used;
        s.ram_total_gb = ram_total;

        let all_gpu_hidden = !self.gpus.is_empty()
            && (0..self.gpus.len()).all(|i| {
                hidden_gpu.iter().any(|h| h == &format!("gpu{i}"))
                    && hidden_gpu.iter().any(|h| h == &format!("gputemp{i}"))
                    && hidden_gpu.iter().any(|h| h == &format!("vram{i}"))
            });

        if let Some(pdh) = &mut self.pdh {
            if pdh.collect() {
                if pdh.therm.counter != 0 {
                    let max_kelvin = pdh
                        .therm
                        .items()
                        .iter()
                        .filter(|i| good(i))
                        .map(|i| unsafe { i.FmtValue.Anonymous.doubleValue })
                        .fold(0.0, f64::max);
                    if max_kelvin > 200.0 {
                        s.cpu_temp = Some(max_kelvin - 273.15);
                    }
                }
                if !all_gpu_hidden {
                s.gpus = self
                    .gpus
                    .iter()
                    .map(|gpu| {
                        let mut g = GpuReading::default();
                        if pdh.eng.counter != 0 {
                            let mut engines = [0.0f64; 64];
                            for item in pdh.eng.items().iter().filter(|i| good(i)) {
                                let name = instance_name(item);
                                if !name.to_ascii_lowercase().contains(&gpu.luid_str) {
                                    continue;
                                }
                                if let Some(pos) = name.find("_eng_") {
                                    if let Ok(k) = name[pos + 5..].split('_').next().unwrap_or("").parse::<usize>() {
                                        if k < engines.len() {
                                            engines[k] += unsafe { item.FmtValue.Anonymous.doubleValue };
                                        }
                                    }
                                }
                            }
                            g.pct = Some(engines.iter().cloned().fold(0.0, f64::max).min(100.0));
                        }
                        if pdh.mem.counter != 0 && gpu.vram_total != 0 {
                            let mut used = 0.0;
                            let mut found = false;
                            for item in pdh.mem.items().iter().filter(|i| good(i)) {
                                if instance_name(item).to_ascii_lowercase().contains(&gpu.luid_str) {
                                    used += unsafe { item.FmtValue.Anonymous.doubleValue };
                                    found = true;
                                }
                            }
                            if found {
                                g.vram_used_gb = used / 1_073_741_824.0;
                                g.vram_total_gb = gpu.vram_total as f64 / 1_073_741_824.0;
                                g.vram_pct = Some(100.0 * used / gpu.vram_total as f64);
                            }
                        }
                        g
                    })
                    .collect();
                }
            }
        }
        if s.gpus.is_empty() && !self.gpus.is_empty() {
            s.gpus = vec![GpuReading::default(); self.gpus.len()];
        }
        if !all_gpu_hidden {
            for (g, gpu) in s.gpus.iter_mut().zip(self.gpus.iter()) {
                g.temp = gpu.temp();
            }
        }
        s.cpu_temp = self.pio.as_ref().and_then(PawnIo::temp).or(s.cpu_temp);

        // SSD/HDD temperature changes slowly, so it's re-read every 5th sample.
        self.disk_tick = self.disk_tick.wrapping_add(1);
        if self.disk_tick.is_multiple_of(5) || self.last_disk_temp.is_none() {
            self.last_disk_temp = self.disk.as_ref().and_then(Disk::temp);
        }
        s.disk_temp = self.last_disk_temp;

        if let Some(pdh) = &mut self.pdh {
            let (top_cpu, top_ram, pid_names) = top_processes(pdh);
            s.top_cpu_process = top_cpu;
            s.top_ram_process = top_ram;
            if pdh.eng.counter != 0 {
                s.top_gpu_process = top_gpu_process(pdh, &pid_names);
            }
        }

        s
    }
}

/// Strips the `#n` PDH appends to tell apart same-named instances (e.g. `chrome#3`), so
/// multiple processes with the same image name report under one display name.
fn strip_instance_suffix(name: &str) -> &str {
    name.rfind('#').map_or(name, |i| &name[..i])
}

/// Scans `\Process(*)\% Processor Time`, `...\Working Set - Private` and `...\ID Process` for
/// the busiest-CPU and busiest-RAM processes, and returns a PID -> display-name map (built from
/// the same sample) for `top_gpu_process` to reuse, since GPU-Engine instance names only carry
/// the PID, not the process name.
type TopProcess = Option<(String, f64)>;
type PidNames = std::collections::HashMap<u32, String>;

fn top_processes(pdh: &mut PdhQuery) -> (TopProcess, TopProcess, PidNames) {
    const SKIP: [&str; 2] = ["_total", "idle"];
    let mut pid_names = std::collections::HashMap::new();
    for item in pdh.proc_id.items().iter().filter(|i| good(i)) {
        let name = strip_instance_suffix(&instance_name(item)).to_string();
        if SKIP.contains(&name.to_ascii_lowercase().as_str()) {
            continue;
        }
        let pid = unsafe { item.FmtValue.Anonymous.doubleValue } as u32;
        pid_names.insert(pid, name);
    }
    let top_cpu = pdh
        .proc_cpu
        .items()
        .iter()
        .filter(|i| good(i))
        .map(|i| (strip_instance_suffix(&instance_name(i)).to_string(), unsafe { i.FmtValue.Anonymous.doubleValue }))
        .filter(|(name, _)| !SKIP.contains(&name.to_ascii_lowercase().as_str()))
        .fold(None, |best: Option<(String, f64)>, cur| {
            if best.as_ref().is_none_or(|b| cur.1 > b.1) { Some(cur) } else { best }
        });
    let top_ram = pdh
        .proc_mem
        .items()
        .iter()
        .filter(|i| good(i))
        .map(|i| (strip_instance_suffix(&instance_name(i)).to_string(), unsafe { i.FmtValue.Anonymous.doubleValue } / 1_048_576.0))
        .filter(|(name, _)| !SKIP.contains(&name.to_ascii_lowercase().as_str()))
        .fold(None, |best: Option<(String, f64)>, cur| {
            if best.as_ref().is_none_or(|b| cur.1 > b.1) { Some(cur) } else { best }
        });
    (top_cpu, top_ram, pid_names)
}

/// Sums `\GPU Engine(pid_<pid>_luid_...)\Utilization Percentage` per PID (across every adapter
/// and engine type) and reports the process using the GPU the hardest, from the PID Windows puts
/// in the instance name.
fn top_gpu_process(pdh: &mut PdhQuery, pid_names: &PidNames) -> TopProcess {
    let mut by_pid: std::collections::HashMap<u32, f64> = std::collections::HashMap::new();
    for item in pdh.eng.items().iter().filter(|i| good(i)) {
        let name = instance_name(item);
        let Some(rest) = name.strip_prefix("pid_") else { continue };
        let Some(pid) = rest.split('_').next().and_then(|s| s.parse::<u32>().ok()) else { continue };
        *by_pid.entry(pid).or_insert(0.0) += unsafe { item.FmtValue.Anonymous.doubleValue };
    }
    by_pid
        .into_iter()
        .filter(|(_, v)| *v > 0.5)
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(pid, v)| (pid_names.get(&pid).cloned().unwrap_or_else(|| format!("pid {pid}")), v.min(100.0)))
}

impl Default for SysMonitor {
    fn default() -> SysMonitor {
        SysMonitor::new()
    }
}

unsafe fn zeroed3() -> (FILETIME, FILETIME, FILETIME) {
    (zeroed(), zeroed(), zeroed())
}

unsafe fn zeroed<T>() -> T {
    std::mem::zeroed()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(pct: f64, temp: f64, vram: f64) -> GpuReading {
        GpuReading { pct: Some(pct), temp: Some(temp), vram_pct: Some(vram), vram_used_gb: 0.0, vram_total_gb: 0.0 }
    }

    fn full_snapshot() -> Snapshot {
        Snapshot {
            cpu_pct: Some(10.0),
            ram_pct: Some(20.0),
            ram_used_gb: 0.0,
            ram_total_gb: 0.0,
            gpus: vec![gpu(30.0, 60.0, 40.0), gpu(31.0, 61.0, 41.0)],
            cpu_temp: Some(50.0),
            disk_temp: Some(35.0),
            top_cpu_process: None,
            top_ram_process: None,
            top_gpu_process: None,
        }
    }

    #[test]
    fn mask_hides_only_the_unset_bits() {
        let snap = full_snapshot().apply(BIT_CPU | BIT_RAM, &[]);
        assert_eq!(snap.cpu_pct, Some(10.0));
        assert_eq!(snap.ram_pct, Some(20.0));
        assert_eq!(snap.cpu_temp, None);
        assert_eq!(snap.disk_temp, None);
    }

    #[test]
    fn all_sensors_mask_keeps_everything() {
        let snap = full_snapshot().apply(ALL_SENSORS, &[]);
        assert_eq!(snap.cpu_pct, Some(10.0));
        assert_eq!(snap.cpu_temp, Some(50.0));
        assert_eq!(snap.ram_pct, Some(20.0));
        assert_eq!(snap.disk_temp, Some(35.0));
    }

    #[test]
    fn hidden_gpu_ids_only_affect_their_own_adapter_and_field() {
        // Hide only adapter 0's usage and adapter 1's temperature; everything else survives.
        let hidden = vec!["gpu0".to_string(), "gputemp1".to_string()];
        let snap = full_snapshot().apply(ALL_SENSORS, &hidden);
        assert_eq!(snap.gpus[0].pct, None);
        assert_eq!(snap.gpus[0].temp, Some(60.0));
        assert_eq!(snap.gpus[0].vram_pct, Some(40.0));
        assert_eq!(snap.gpus[1].pct, Some(31.0));
        assert_eq!(snap.gpus[1].temp, None);
        assert_eq!(snap.gpus[1].vram_pct, Some(41.0));
    }

    #[test]
    fn all_hidden_true_only_when_every_value_is_none() {
        let mostly_hidden = full_snapshot().apply(0, &["gpu0".into(), "gputemp0".into(), "vram0".into(), "gpu1".into(), "gputemp1".into(), "vram1".into()]);
        assert!(mostly_hidden.all_hidden());

        let one_left = full_snapshot().apply(BIT_CPU, &["gpu0".into(), "gputemp0".into(), "vram0".into(), "gpu1".into(), "gputemp1".into(), "vram1".into()]);
        assert!(!one_left.all_hidden());
    }
}
