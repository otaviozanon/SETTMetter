use std::mem::{ManuallyDrop, size_of};
use std::ptr;

use winaudio::Win32::Foundation::PROPERTYKEY;
use winaudio::Win32::Media::Audio::ERole;
use winaudio::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use winaudio::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemAlloc};
use winaudio::Win32::System::Variant::{VT_BSTR, VT_LPWSTR};
use winaudio::core::{BOOL, GUID, PCWSTR, PWSTR};

use crate::audio::win32;

pub use com::IPolicyConfig;

pub const CLSID_POLICY_CONFIG_CLIENT: GUID =
    GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

pub const PKEY_LISTEN_ENABLED: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x24dbb0fc_9311_4b3d_9cf0_18ff155639d4),
    pid: 1,
};

pub const PKEY_LISTEN_DEVICE: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x24dbb0fc_9311_4b3d_9cf0_18ff155639d4),
    pid: 0,
};

#[allow(non_snake_case, unsafe_op_in_unsafe_fn, clippy::too_many_arguments)]
mod com {
    use std::ffi::c_void;

    use winaudio::Win32::Foundation::PROPERTYKEY;
    use winaudio::Win32::Media::Audio::ERole;
    use winaudio::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use winaudio::core::{BOOL, HRESULT, IUnknown, IUnknown_Vtbl, PCWSTR, interface};

    #[interface("f8679f50-850a-41cf-9c72-430f290290c8")]
    pub unsafe trait IPolicyConfig: IUnknown {
        pub fn GetMixFormat(&self, device_id: PCWSTR, format: *mut *mut c_void) -> HRESULT;
        pub fn GetDeviceFormat(
            &self,
            device_id: PCWSTR,
            default_format: i32,
            format: *mut *mut c_void,
        ) -> HRESULT;
        pub fn ResetDeviceFormat(&self, device_id: PCWSTR) -> HRESULT;
        pub fn SetDeviceFormat(
            &self,
            device_id: PCWSTR,
            endpoint_format: *const c_void,
            mix_format: *const c_void,
        ) -> HRESULT;
        pub fn GetProcessingPeriod(
            &self,
            device_id: PCWSTR,
            default_period: i32,
            default_period_out: *mut i64,
            minimum_period_out: *mut i64,
        ) -> HRESULT;
        pub fn SetProcessingPeriod(&self, device_id: PCWSTR, period: *const i64) -> HRESULT;
        pub fn GetShareMode(&self, device_id: PCWSTR, mode: *mut c_void) -> HRESULT;
        pub fn SetShareMode(&self, device_id: PCWSTR, mode: *const c_void) -> HRESULT;
        pub fn GetPropertyValue(
            &self,
            device_id: PCWSTR,
            fx_store: BOOL,
            key: *const PROPERTYKEY,
            value: *mut PROPVARIANT,
        ) -> HRESULT;
        pub fn SetPropertyValue(
            &self,
            device_id: PCWSTR,
            fx_store: BOOL,
            key: *const PROPERTYKEY,
            value: *const PROPVARIANT,
        ) -> HRESULT;
        pub fn SetDefaultEndpoint(&self, device_id: PCWSTR, role: ERole) -> HRESULT;
        pub fn SetEndpointVisibility(&self, device_id: PCWSTR, visible: BOOL) -> HRESULT;
    }
}

pub struct PolicyConfig {
    inner: IPolicyConfig,
}

impl PolicyConfig {
    pub fn new() -> winaudio::core::Result<Self> {
        let inner: IPolicyConfig =
            unsafe { CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL)? };
        Ok(Self { inner })
    }

    pub fn set_default_endpoint(&self, device_id: &str, role: ERole) -> winaudio::core::Result<()> {
        let wide = win32::to_wide(device_id);
        unsafe { self.inner.SetDefaultEndpoint(PCWSTR(wide.as_ptr()), role).ok() }
    }

    pub fn set_endpoint_visibility(
        &self,
        device_id: &str,
        visible: bool,
    ) -> winaudio::core::Result<()> {
        let wide = win32::to_wide(device_id);
        unsafe {
            self.inner
                .SetEndpointVisibility(PCWSTR(wide.as_ptr()), BOOL::from(visible))
                .ok()
        }
    }

    pub fn get_property_value(
        &self,
        device_id: &str,
        fx_store: bool,
        key: &PROPERTYKEY,
    ) -> winaudio::core::Result<PROPVARIANT> {
        let wide = win32::to_wide(device_id);
        let mut value = PROPVARIANT::default();
        unsafe {
            self.inner
                .GetPropertyValue(PCWSTR(wide.as_ptr()), fx_store.into(), key, &mut value)
                .ok()?;
        }
        Ok(value)
    }

    pub fn set_property_value(
        &self,
        device_id: &str,
        fx_store: bool,
        key: &PROPERTYKEY,
        value: &PROPVARIANT,
    ) -> winaudio::core::Result<()> {
        let wide = win32::to_wide(device_id);
        unsafe {
            self.inner
                .SetPropertyValue(PCWSTR(wide.as_ptr()), fx_store.into(), key, value)
                .ok()
        }
    }
}

pub fn propvariant_lpwstr(s: &str) -> PROPVARIANT {
    let wide = win32::to_wide(s);
    let byte_len = wide.len() * size_of::<u16>();
    let ptr = unsafe { CoTaskMemAlloc(byte_len) }.cast::<u16>();
    if ptr.is_null() {
        return PROPVARIANT::default();
    }
    unsafe {
        ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
    }
    PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_LPWSTR,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    pwszVal: PWSTR(ptr),
                },
            }),
        },
    }
}

pub fn propvariant_to_string(pv: &PROPVARIANT) -> Option<String> {
    let vt = pv.vt();
    unsafe {
        if vt == VT_LPWSTR {
            let ptr = pv.Anonymous.Anonymous.Anonymous.pwszVal;
            if ptr.is_null() {
                None
            } else {
                ptr.to_string().ok()
            }
        } else if vt == VT_BSTR {
            Some((*pv.Anonymous.Anonymous.Anonymous.bstrVal).to_string())
        } else {
            None
        }
    }
}
