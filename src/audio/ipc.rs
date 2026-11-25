//! Per-user named-pipe JSON interface for automation (PowerShell, AutoHotkey, Stream Deck).

use std::ffi::c_void;
use std::mem::size_of;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use winaudio::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED,
    GetLastError, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree, WAIT_OBJECT_0,
};
use winaudio::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use winaudio::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use winaudio::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use winaudio::Win32::System::IO::{CancelIo, GetOverlappedResult, OVERLAPPED};
use winaudio::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use winaudio::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, INFINITE, OpenProcessToken, ResetEvent, WaitForSingleObject,
};
use winaudio::core::{PCWSTR, PWSTR};

use crate::audio::model::{AppAction, AppState};
use crate::audio::win32::{self, WakeHandle, WM_APP_EXIT, WM_APP_MENU};

pub const PIPE_NAME: &str = r"\\.\pipe\settmetter-audio-ipc";
pub const QUIT_EVENT_NAME: &str = "Local\\SETTMetterAudioQuitEvent";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcRequest {
    GetState,
    ApplyAction(AppAction),
    Ping,
    OpenFlyout {
        anchor_x: Option<i32>,
        anchor_y: Option<i32>,
    },
    QuitDaemon,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcResponse {
    State(AppState),
    Ok,
    Error(String),
}

const PIPE_BUFFER_SIZE: u32 = 64 * 1024;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const IO_TIMEOUT_MS: u32 = 2_000;
const CLIENT_DRAIN_TIMEOUT_MS: u32 = 250;

pub struct IpcContext {
    pub wake: WakeHandle,
}

enum IpcAction {
    None,
    OpenMenu,
    Quit,
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() && self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

struct OwnedSecurityDescriptor {
    sd: PSECURITY_DESCRIPTOR,
}

impl Drop for OwnedSecurityDescriptor {
    fn drop(&mut self) {
        if !self.sd.0.is_null() {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.sd.0)));
            }
        }
    }
}

pub fn spawn_server(ctx: IpcContext) {
    let _ = thread::Builder::new()
        .name("ipc-server".into())
        .spawn(move || run_server(ctx));
}

pub fn spawn_quit_event_listener(wake: WakeHandle) {
    let _ = thread::Builder::new()
        .name("quit-event-watcher".into())
        .spawn(move || {
            let name = win32::to_wide(QUIT_EVENT_NAME);
            let Ok(handle) = (unsafe { CreateEventW(None, true, false, PCWSTR(name.as_ptr())) })
            else {
                return;
            };

            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                unsafe {
                    let _ = ResetEvent(handle);
                }
            }

            let wait_result = unsafe { WaitForSingleObject(handle, INFINITE) };
            if wait_result == WAIT_OBJECT_0 {
                wake.post(WM_APP_EXIT);
            }

            // Keep `handle` open for the process lifetime so the named event stays alive.
            let _ = handle;
        });
}

fn current_user_sid_string() -> Option<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
        let token = OwnedHandle(token);

        let mut needed = 0u32;
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut needed);
        if (needed as usize) < size_of::<TOKEN_USER>() {
            return None;
        }

        let mut buf = vec![0u8; needed as usize];
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            needed,
            &mut needed,
        )
        .ok()?;
        if (needed as usize) < size_of::<TOKEN_USER>() {
            return None;
        }

        let token_user = std::ptr::read_unaligned(buf.as_ptr().cast::<TOKEN_USER>());
        let mut sid_pwstr = PWSTR::null();
        ConvertSidToStringSidW(token_user.User.Sid, &mut sid_pwstr).ok()?;
        let sid_str = sid_pwstr.to_string().ok();
        if !sid_pwstr.0.is_null() {
            let _ = LocalFree(Some(HLOCAL(sid_pwstr.0.cast::<c_void>())));
        }
        sid_str
    }
}

fn build_pipe_security_descriptor() -> Option<OwnedSecurityDescriptor> {
    let user_sid = current_user_sid_string()?;
    let sddl = format!("D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{user_sid})");
    let sddl_wide = win32::to_wide(&sddl);
    let mut sd = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl_wide.as_ptr()),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
        .ok()?;
    }
    if sd.0.is_null() {
        return None;
    }
    Some(OwnedSecurityDescriptor { sd })
}

fn create_pipe_instance(
    pipe_name: &[u16],
    pipe_sd: Option<&OwnedSecurityDescriptor>,
) -> Option<OwnedHandle> {
    let security_attrs = pipe_sd.map(|owned| SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: owned.sd.0,
        ..Default::default()
    });
    let sa_ptr = security_attrs.as_ref().map(std::ptr::from_ref);

    let pipe = unsafe {
        CreateNamedPipeW(
            PCWSTR(pipe_name.as_ptr()),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            PIPE_BUFFER_SIZE,
            PIPE_BUFFER_SIZE,
            0,
            sa_ptr,
        )
    };

    if pipe.is_invalid() || pipe == INVALID_HANDLE_VALUE {
        None
    } else {
        Some(OwnedHandle(pipe))
    }
}

fn dispatch_action(action: IpcAction, ctx: &IpcContext) {
    match action {
        IpcAction::None => {}
        IpcAction::OpenMenu => ctx.wake.post(WM_APP_MENU),
        IpcAction::Quit => ctx.wake.post(WM_APP_EXIT),
    }
}

fn wait_for_client_eof(pipe: HANDLE, event: HANDLE, timeout_ms: u32) {
    let mut byte = [0u8; 1];
    let _ = read_overlapped(pipe, event, &mut byte, timeout_ms);
}

fn run_server(ctx: IpcContext) {
    let pipe_name = win32::to_wide(PIPE_NAME);
    let pipe_sd = build_pipe_security_descriptor();
    let Ok(raw_event) = (unsafe { CreateEventW(None, true, false, PCWSTR::null()) }) else {
        return;
    };
    let io_event = OwnedHandle(raw_event);
    let mut pipe: Option<OwnedHandle> = None;

    loop {
        let pipe_handle = match pipe.as_ref() {
            Some(handle) => handle.0,
            None => {
                let Some(created) = create_pipe_instance(&pipe_name, pipe_sd.as_ref()) else {
                    thread::sleep(Duration::from_millis(100));
                    continue;
                };
                let raw = created.0;
                pipe = Some(created);
                raw
            }
        };

        if !connect_overlapped(pipe_handle, io_event.0) {
            pipe = None;
            thread::sleep(Duration::from_millis(50));
            continue;
        }

        let (action, wrote_ok) = handle_client(pipe_handle, io_event.0, &ctx);
        dispatch_action(action, &ctx);

        if wrote_ok {
            wait_for_client_eof(pipe_handle, io_event.0, CLIENT_DRAIN_TIMEOUT_MS);
        }

        if unsafe { DisconnectNamedPipe(pipe_handle) }.is_err() {
            pipe = None;
        }
    }
}

fn connect_overlapped(pipe: HANDLE, event: HANDLE) -> bool {
    unsafe {
        let _ = ResetEvent(event);
        let mut overlapped = OVERLAPPED {
            hEvent: event,
            ..Default::default()
        };
        match ConnectNamedPipe(pipe, Some(&mut overlapped)) {
            Ok(()) => true,
            Err(err) if err.code() == ERROR_PIPE_CONNECTED.to_hresult() => true,
            Err(err) if err.code() == ERROR_IO_PENDING.to_hresult() => {
                if WaitForSingleObject(event, INFINITE) != WAIT_OBJECT_0 {
                    let _ = CancelIo(pipe);
                    let mut ignored = 0u32;
                    let _ = GetOverlappedResult(pipe, &overlapped, &mut ignored, true);
                    return false;
                }
                let mut transferred = 0u32;
                GetOverlappedResult(pipe, &overlapped, &mut transferred, false).is_ok()
            }
            Err(_) => false,
        }
    }
}

fn handle_client(pipe: HANDLE, event: HANDLE, ctx: &IpcContext) -> (IpcAction, bool) {
    let Some(line) = read_line(pipe, event) else {
        return (IpcAction::None, false);
    };

    let mut action = IpcAction::None;
    let response = match serde_json::from_slice::<IpcRequest>(&line) {
        Ok(IpcRequest::GetState) => IpcResponse::State(snapshot()),
        Ok(IpcRequest::ApplyAction(app_action)) => match crate::audio::engine() {
            Some(engine) => IpcResponse::State(engine.apply_action(app_action)),
            None => IpcResponse::Error("audio engine not running".into()),
        },
        Ok(IpcRequest::OpenFlyout { .. }) => {
            action = IpcAction::OpenMenu;
            IpcResponse::Ok
        }
        Ok(IpcRequest::QuitDaemon) => {
            action = IpcAction::Quit;
            IpcResponse::Ok
        }
        Ok(IpcRequest::Ping) => IpcResponse::Ok,
        Err(err) => IpcResponse::Error(format!("Invalid IPC request: {err}")),
    };
    let _ = ctx;

    let wrote_ok = if let Ok(mut payload) = serde_json::to_vec(&response) {
        payload.push(b'\n');
        write_all(pipe, event, &payload)
    } else {
        false
    };

    (action, wrote_ok)
}

fn snapshot() -> AppState {
    crate::audio::engine()
        .map(|e| e.snapshot())
        .unwrap_or_default()
}

fn read_overlapped(pipe: HANDLE, event: HANDLE, buf: &mut [u8], timeout_ms: u32) -> Option<usize> {
    unsafe {
        let _ = ResetEvent(event);
        let mut overlapped = OVERLAPPED {
            hEvent: event,
            ..Default::default()
        };
        match ReadFile(pipe, Some(buf), None, Some(&mut overlapped)) {
            Ok(()) => {
                let mut transferred = 0u32;
                if GetOverlappedResult(pipe, &overlapped, &mut transferred, false).is_ok() {
                    Some(transferred as usize)
                } else {
                    None
                }
            }
            Err(err) if err.code() == ERROR_IO_PENDING.to_hresult() => {
                if WaitForSingleObject(event, timeout_ms) == WAIT_OBJECT_0 {
                    let mut transferred = 0u32;
                    match GetOverlappedResult(pipe, &overlapped, &mut transferred, false) {
                        Ok(()) => Some(transferred as usize),
                        Err(e) if e.code() == ERROR_BROKEN_PIPE.to_hresult() => Some(0),
                        Err(_) => None,
                    }
                } else {
                    let _ = CancelIo(pipe);
                    let mut ignored = 0u32;
                    let _ = GetOverlappedResult(pipe, &overlapped, &mut ignored, true);
                    None
                }
            }
            Err(err) if err.code() == ERROR_BROKEN_PIPE.to_hresult() => Some(0),
            Err(_) => None,
        }
    }
}

fn write_overlapped(pipe: HANDLE, event: HANDLE, data: &[u8], timeout_ms: u32) -> Option<usize> {
    unsafe {
        let _ = ResetEvent(event);
        let mut overlapped = OVERLAPPED {
            hEvent: event,
            ..Default::default()
        };
        match WriteFile(pipe, Some(data), None, Some(&mut overlapped)) {
            Ok(()) => {
                let mut transferred = 0u32;
                if GetOverlappedResult(pipe, &overlapped, &mut transferred, false).is_ok() {
                    Some(transferred as usize)
                } else {
                    None
                }
            }
            Err(err) if err.code() == ERROR_IO_PENDING.to_hresult() => {
                if WaitForSingleObject(event, timeout_ms) == WAIT_OBJECT_0 {
                    let mut transferred = 0u32;
                    if GetOverlappedResult(pipe, &overlapped, &mut transferred, false).is_ok() {
                        Some(transferred as usize)
                    } else {
                        None
                    }
                } else {
                    let _ = CancelIo(pipe);
                    let mut ignored = 0u32;
                    let _ = GetOverlappedResult(pipe, &overlapped, &mut ignored, true);
                    None
                }
            }
            Err(_) => None,
        }
    }
}

fn remaining_timeout_ms(deadline: Instant) -> Option<u32> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return None;
    }
    Some(remaining.as_millis().clamp(1, u128::from(u32::MAX)) as u32)
}

fn read_line(pipe: HANDLE, event: HANDLE) -> Option<Vec<u8>> {
    let deadline = Instant::now() + Duration::from_millis(u64::from(IO_TIMEOUT_MS));
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        let timeout_ms = remaining_timeout_ms(deadline)?;
        let bytes_read = read_overlapped(pipe, event, &mut chunk, timeout_ms)?;
        if bytes_read == 0 {
            break;
        }

        let slice = &chunk[..bytes_read];
        if let Some(pos) = slice.iter().position(|&b| b == b'\n') {
            if buffer.len() + pos > MAX_REQUEST_BYTES {
                return None;
            }
            buffer.extend_from_slice(&slice[..pos]);
            break;
        }
        if buffer.len() + slice.len() > MAX_REQUEST_BYTES {
            return None;
        }
        buffer.extend_from_slice(slice);
    }

    if buffer.last() == Some(&b'\r') {
        buffer.pop();
    }

    if buffer.is_empty() {
        None
    } else {
        Some(buffer)
    }
}

fn write_all(pipe: HANDLE, event: HANDLE, mut data: &[u8]) -> bool {
    let deadline = Instant::now() + Duration::from_millis(u64::from(IO_TIMEOUT_MS));
    while !data.is_empty() {
        let Some(timeout_ms) = remaining_timeout_ms(deadline) else {
            return false;
        };
        let Some(written) = write_overlapped(pipe, event, data, timeout_ms) else {
            return false;
        };
        if written == 0 {
            return false;
        }
        data = &data[written..];
    }
    true
}
