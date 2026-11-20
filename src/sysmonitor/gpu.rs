//! Discrete GPU detection (DXGI) and per-adapter temperature via D3DKMT or NVML.
//! Ported from TraySense's `init_gpu`/D3DKMT+NVML temperature path.

use std::ffi::c_void;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{HMODULE, LUID};
use windows_sys::Win32::System::LibraryLoader::*;

pub(crate) fn load_lib(name: &str) -> *mut c_void {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { LoadLibraryExW(wide.as_ptr(), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) }
}

pub(crate) fn proc_addr(module: *mut c_void, name: &str) -> *mut c_void {
    let cname = std::ffi::CString::new(name).unwrap();
    unsafe { GetProcAddress(module as HMODULE, cname.as_ptr() as *const u8).map_or(null_mut(), |f| f as *mut c_void) }
}

/// Loads `name` from `module` and casts it straight to the caller's `unsafe extern "system" fn`
/// type `T`, replacing the repeated `GetProcAddress` + `std::mem::transmute` pairs below. `T`
/// must be exactly pointer-sized (every call site here passes a bare `fn` type), checked at
/// runtime since `transmute` can't enforce that across a generic parameter.
pub(crate) unsafe fn load_fn<T: Copy>(module: *mut c_void, name: &str) -> Option<T> {
    debug_assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<usize>(), "load_fn::<T> must be a fn pointer");
    let addr = proc_addr(module, name);
    (!addr.is_null()).then(|| unsafe { std::mem::transmute_copy(&addr) })
}

const KMTQAITYPE_ADAPTERPERFDATA: u32 = 62;

/// Discrete/primary GPU chosen the same way as TraySense `init_gpu`: largest dedicated VRAM
/// among non-software, non-Microsoft-Basic-Render adapters.
#[allow(dead_code)] // `luid`/`name` kept for debugging/diagnostics, not read on the hot path.
pub(crate) struct Gpu {
    luid: LUID,
    /// `luid_0xHHHHHHHH_0xLLLLLLLL`, used to filter PDH `GPU Engine`/`GPU Adapter Memory`
    /// instance names down to this adapter only (same as TraySense `has_luid`).
    pub(crate) luid_str: String,
    name: String,
    pub(crate) vram_total: u64,
    kmt_adapter: u32,
    kmt_query: Option<unsafe extern "system" fn(*mut c_void) -> i32>,
    nvml: Option<Nvml>,
}

struct Nvml {
    get_temp: unsafe extern "system" fn(*mut c_void, i32, *mut u32) -> i32,
    device: *mut c_void,
}

#[repr(C)]
struct KmtOpen {
    luid: LUID,
    adapter: u32,
}
#[repr(C)]
struct KmtQai {
    adapter: u32,
    kind: u32,
    data: *mut c_void,
    size: u32,
}
#[repr(C)]
#[derive(Default)]
struct KmtPerf {
    phys_idx: u32,
    mem_freq: u64,
    max_mem_freq: u64,
    max_mem_freq_oc: u64,
    mem_bw: u64,
    pcie_bw: u64,
    fan_rpm: u32,
    power: u32,
    temperature: u32,
    power_state_override: u8,
}

impl Gpu {
    /// Every discrete (non-software, non-Microsoft-Basic-Render) adapter, in DXGI enumeration
    /// order. A notebook with one dedicated GPU returns a single-element vec (unchanged
    /// behavior); one with two (e.g. dual-GPU workstation/laptop) returns both so each can be
    /// shown as its own `GPU1`/`GPU2` line instead of being merged into one.
    pub(crate) fn detect_all() -> Vec<Gpu> {
        use windows::Win32::Graphics::Dxgi::{
            CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIAdapter1, IDXGIFactory1,
        };
        let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
            return Vec::new();
        };
        let mut found: Vec<(LUID, u64, String)> = Vec::new();
        let mut i = 0u32;
        loop {
            let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(i) } {
                Ok(a) => a,
                Err(_) => break,
            };
            i += 1;
            let desc = match unsafe { adapter.GetDesc1() } {
                Ok(d) => d,
                Err(_) => continue,
            };
            if desc.Flags & (DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 || desc.VendorId == 0x1414 {
                continue; // skip software/WARP and Microsoft Basic Render adapters
            }
            let name = String::from_utf16_lossy(&desc.Description).trim_end_matches('\0').to_string();
            // windows::Win32::Foundation::LUID has the same layout as windows_sys's.
            let luid = LUID { LowPart: desc.AdapterLuid.LowPart, HighPart: desc.AdapterLuid.HighPart };
            found.push((luid, desc.DedicatedVideoMemory as u64, name));
        }

        let gdi = unsafe { GetModuleHandleW(crate::wide("gdi32.dll").as_ptr()) };
        let open: Option<unsafe extern "system" fn(*mut c_void) -> i32> =
            unsafe { load_fn(gdi, "D3DKMTOpenAdapterFromLuid") };
        let query: Option<unsafe extern "system" fn(*mut c_void) -> i32> =
            unsafe { load_fn(gdi, "D3DKMTQueryAdapterInfo") };

        let mut nvidia_index = 0u32;
        found
            .into_iter()
            .map(|(luid, vram_total, name)| {
                let mut kmt_adapter = 0u32;
                if let Some(open) = open {
                    let mut o = KmtOpen { luid, adapter: 0 };
                    if unsafe { open(&mut o as *mut _ as *mut c_void) } == 0 {
                        kmt_adapter = o.adapter;
                    }
                }
                let luid_str = format!("luid_0x{:08x}_0x{:08x}", luid.HighPart as u32, luid.LowPart);
                let is_nvidia = name.to_ascii_lowercase().contains("nvidia");
                let mut gpu = Gpu { luid, luid_str, name, vram_total, kmt_adapter, kmt_query: query, nvml: None };
                if gpu.kmt_temp().is_none() && is_nvidia {
                    gpu.nvml = Nvml::init(nvidia_index);
                }
                if is_nvidia {
                    nvidia_index += 1;
                }
                gpu
            })
            .collect()
    }

    fn kmt_temp(&self) -> Option<f64> {
        let query = self.kmt_query?;
        if self.kmt_adapter == 0 {
            return None;
        }
        let mut perf = KmtPerf::default();
        let mut q = KmtQai {
            adapter: self.kmt_adapter,
            kind: KMTQAITYPE_ADAPTERPERFDATA,
            data: &mut perf as *mut _ as *mut c_void,
            size: std::mem::size_of::<KmtPerf>() as u32,
        };
        if unsafe { query(&mut q as *mut _ as *mut c_void) } != 0 || perf.temperature == 0 {
            return None;
        }
        Some(perf.temperature as f64 / 10.0)
    }

    pub(crate) fn temp(&self) -> Option<f64> {
        self.kmt_temp().or_else(|| self.nvml.as_ref().and_then(Nvml::temp))
    }
}

impl Nvml {
    fn init(index: u32) -> Option<Nvml> {
        let mut nv = load_lib("nvml.dll");
        if nv.is_null() {
            let wide: Vec<u16> = r"C:\Program Files\NVIDIA Corporation\NVSMI\nvml.dll"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            nv = unsafe { LoadLibraryExW(wide.as_ptr(), null_mut(), LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32) };
        }
        if nv.is_null() {
            return None;
        }
        let init: Option<unsafe extern "system" fn() -> i32> = unsafe { load_fn(nv, "nvmlInit_v2") };
        let handle_by_idx: Option<unsafe extern "system" fn(u32, *mut *mut c_void) -> i32> =
            unsafe { load_fn(nv, "nvmlDeviceGetHandleByIndex_v2") };
        let get_temp: Option<unsafe extern "system" fn(*mut c_void, i32, *mut u32) -> i32> =
            unsafe { load_fn(nv, "nvmlDeviceGetTemperature") };
        let (init, handle_by_idx, get_temp) = (init?, handle_by_idx?, get_temp?);
        if unsafe { init() } != 0 {
            return None;
        }
        let mut device = null_mut();
        if unsafe { handle_by_idx(index, &mut device) } != 0 {
            return None;
        }
        Some(Nvml { get_temp, device })
    }

    fn temp(&self) -> Option<f64> {
        let mut t = 0u32;
        (unsafe { (self.get_temp)(self.device, 0, &mut t) } == 0).then_some(t as f64)
    }
}
