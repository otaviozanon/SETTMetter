pub mod model;
mod ipc;
mod policy_config;
mod spatial;
mod store;
pub(crate) mod win32;


use std::sync::OnceLock;

static ENGINE: OnceLock<AudioEngine> = OnceLock::new();

/// Starts the audio worker. `controller` receives `WM_APP_AUDIO_REFRESH` (WM_USER + 1) posts
/// from the worker, which the widget's controller window handles.
pub fn start(controller: isize) {
    let state = std::sync::Arc::new(std::sync::Mutex::new(store::load_state()));
    let wake = win32::WakeHandle::new(controller);
    let engine = AudioEngine::start(state, wake);
    let _ = ENGINE.set(engine);
    ipc::spawn_server(ipc::IpcContext { wake });
    ipc::spawn_quit_event_listener(wake);
}

pub fn engine() -> Option<&'static AudioEngine> {
    ENGINE.get()
}

/// Restarts the Windows audio stack (AudioEndpointBuilder, which restarts Audiosrv and every
/// endpoint). Windows' own recovery from a crashed Audiosrv waits 60 s, which is the "audio dead
/// for a minute" users see; this is the manual equivalent. Runs hidden; the app is elevated.
pub fn restart_audio_service() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let command = "Restart-Service -Name AudioEndpointBuilder -Force -ErrorAction SilentlyContinue; \
         Start-Sleep -Milliseconds 1500; \
         if ((Get-Service Audiosrv).Status -ne 'Running') { Start-Service Audiosrv -ErrorAction SilentlyContinue }";
    let _ = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            command,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::audio::model::AppAction;
use crate::audio::model::{
    AppState, AudioDevice, DeviceFlow, EnhancementsState, LISTEN_TARGET_COMMS_OUTPUT,
    LISTEN_TARGET_DEFAULT_OUTPUT, ListenConfig, RoleAssignments,
};
use winaudio::Win32::Devices::FunctionDiscovery::{
    PKEY_Device_DeviceDesc, PKEY_Device_FriendlyName, PKEY_DeviceInterface_FriendlyName,
};
use winaudio::Win32::Foundation::{PROPERTYKEY, S_FALSE, S_OK};
use winaudio::Win32::Media::Audio::Endpoints::{
    IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl,
};
use winaudio::Win32::Media::Audio::{
    AUDIO_VOLUME_NOTIFICATION_DATA, DEVICE_STATE, DEVICE_STATE_ACTIVE, DEVICE_STATE_DISABLED,
    EDataFlow, ENDPOINT_SYSFX_DISABLED, ENDPOINT_SYSFX_ENABLED, ERole, IMMDevice,
    IMMDeviceEnumerator, IMMEndpoint, IMMNotificationClient, IMMNotificationClient_Impl,
    MMDeviceEnumerator, PKEY_AudioEndpoint_Disable_SysFx, eAll, eCapture, eCommunications,
    eConsole, eMultimedia, eRender,
};
use winaudio::Win32::System::Com::StructuredStorage::PROPVARIANT;
use winaudio::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, STGM_READ,
};
use winaudio::core::{GUID, Interface, PCWSTR, implement};

use crate::audio::policy_config::{
    PKEY_LISTEN_DEVICE, PKEY_LISTEN_ENABLED, PolicyConfig, propvariant_lpwstr,
    propvariant_to_string,
};
use win32::{WM_APP_AUDIO_REFRESH as WM_APP_REFRESH_TRAY, WakeHandle};

const APP_AUDIO_CONTEXT: GUID = GUID::from_u128(0x9f4a6b2c_8d1e_47c5_a3b0_7e12c5f941d8);
const LOCK_GUARD_COOLDOWN: Duration = Duration::from_millis(80);
const LOCK_FOLLOWUP_BUFFER: Duration = Duration::from_millis(20);
const VISIBILITY_POLL_INTERVAL: Duration = Duration::from_millis(15);
const VISIBILITY_WAIT_TIMEOUT: Duration = Duration::from_millis(500);
const AUTO_ENFORCE_SUPPRESS_WINDOW: Duration = Duration::from_millis(400);
const TOPOLOGY_COALESCE_DELAY: Duration = Duration::from_millis(50);
const SAVE_DEBOUNCE_DELAY: Duration = Duration::from_millis(250);
const TRIM_IDLE_DELAY: Duration = Duration::from_secs(3);

enum EngineMsg {
    ApplyAction {
        action: AppAction,
        reply: mpsc::SyncSender<AppState>,
    },
    ApplyActionAsync {
        action: AppAction,
    },
    ToggleActiveMicsMute {
        reply: mpsc::SyncSender<AppState>,
    },
    DefaultChanged {
        flow: DeviceFlow,
        role: ERole,
        device_id: String,
    },
    TopologyChanged,
    VolumeChanged {
        device_id: String,
        volume: u8,
        muted: bool,
    },
    PropertyChanged {
        device_id: String,
        key: PROPERTYKEY,
    },
    Shutdown,
}

pub struct AudioEngine {
    tx: mpsc::Sender<EngineMsg>,
    state: Arc<Mutex<AppState>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl AudioEngine {
    pub fn start(state: Arc<Mutex<AppState>>, wake: WakeHandle) -> Self {
        let (tx, rx) = mpsc::channel::<EngineMsg>();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<()>(1);

        let worker_tx = tx.clone();
        let worker_state = Arc::clone(&state);
        let handle = thread::Builder::new()
            .name("audio-engine".into())
            .spawn(move || run_worker(worker_state, wake, worker_tx, rx, ready_tx))
            .ok();

        let _ = ready_rx.recv();

        Self {
            tx,
            state,
            worker: Mutex::new(handle),
        }
    }

    pub fn apply_action(&self, action: AppAction) -> AppState {        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        if self
            .tx
            .send(EngineMsg::ApplyAction {
                action: action.clone(),
                reply: reply_tx,
            })
            .is_ok()
            && let Ok(updated) = reply_rx.recv()
        {
            return updated;
        }

        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.apply_action(&action);
        let _ = store::save_state(&guard);
        guard.clone()
    }

    pub fn apply_action_async(&self, action: AppAction) {
        let is_volume = matches!(action, AppAction::SetVolume { .. });
        if is_volume {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard.apply_action(&action);
        }
        if self
            .tx
            .send(EngineMsg::ApplyActionAsync {
                action: action.clone(),
            })
            .is_err()
        {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if !is_volume {
                guard.apply_action(&action);
            }
            let _ = store::save_state(&guard);
        }
    }

    pub fn toggle_active_mics_mute(&self) -> AppState {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        if self
            .tx
            .send(EngineMsg::ToggleActiveMicsMute { reply: reply_tx })
            .is_ok()
            && let Ok(updated) = reply_rx.recv()
        {
            return updated;
        }

        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.toggle_active_mics_mute();
        let _ = store::save_state(&guard);
        guard.clone()
    }

    pub fn snapshot(&self) -> AppState {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(EngineMsg::Shutdown);
        if let Some(handle) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = handle.join();
        }
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct ComMtaGuard {
    should_uninit: bool,
}

impl ComMtaGuard {
    fn init() -> Self {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self {
            should_uninit: hr == S_OK || hr == S_FALSE,
        }
    }
}

impl Drop for ComMtaGuard {
    fn drop(&mut self) {
        if self.should_uninit {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

#[implement(IMMNotificationClient)]
struct DeviceNotificationClient {
    tx: mpsc::Sender<EngineMsg>,
}

impl IMMNotificationClient_Impl for DeviceNotificationClient_Impl {
    fn OnDeviceStateChanged(
        &self,
        _pwstrdeviceid: &PCWSTR,
        _dwnewstate: DEVICE_STATE,
    ) -> winaudio::core::Result<()> {
        let _ = self.tx.send(EngineMsg::TopologyChanged);
        Ok(())
    }

    fn OnDeviceAdded(&self, _pwstrdeviceid: &PCWSTR) -> winaudio::core::Result<()> {
        let _ = self.tx.send(EngineMsg::TopologyChanged);
        Ok(())
    }

    fn OnDeviceRemoved(&self, _pwstrdeviceid: &PCWSTR) -> winaudio::core::Result<()> {
        let _ = self.tx.send(EngineMsg::TopologyChanged);
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        pwstrdefaultdeviceid: &PCWSTR,
    ) -> winaudio::core::Result<()> {
        let Some(flow) = flow_from_win32(flow) else {
            return Ok(());
        };
        let device_id = if pwstrdefaultdeviceid.is_null() {
            String::new()
        } else {
            unsafe { pwstrdefaultdeviceid.to_string().unwrap_or_default() }
        };
        let _ = self.tx.send(EngineMsg::DefaultChanged {
            flow,
            role,
            device_id,
        });
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        pwstrdeviceid: &PCWSTR,
        key: &PROPERTYKEY,
    ) -> winaudio::core::Result<()> {
        if pwstrdeviceid.is_null() || !is_watched_property(key) {
            return Ok(());
        }
        let device_id = unsafe { pwstrdeviceid.to_string().unwrap_or_default() };
        if !device_id.is_empty() {
            let _ = self.tx.send(EngineMsg::PropertyChanged {
                device_id,
                key: *key,
            });
        }
        Ok(())
    }
}

#[implement(IAudioEndpointVolumeCallback)]
struct EndpointVolumeCallback {
    device_id: String,
    tx: mpsc::Sender<EngineMsg>,
}

impl IAudioEndpointVolumeCallback_Impl for EndpointVolumeCallback_Impl {
    fn OnNotify(
        &self,
        pnotify: *mut AUDIO_VOLUME_NOTIFICATION_DATA,
    ) -> winaudio::core::Result<()> {
        if pnotify.is_null() {
            return Ok(());
        }
        let data = unsafe { &*pnotify };
        if data.guidEventContext == APP_AUDIO_CONTEXT {
            return Ok(());
        }
        let volume = (data.fMasterVolume.clamp(0.0, 1.0) * 100.0).round() as u8;
        let muted = data.bMuted.as_bool();
        let _ = self.tx.send(EngineMsg::VolumeChanged {
            device_id: self.device_id.clone(),
            volume,
            muted,
        });
        Ok(())
    }
}

struct VolumeSubscription {
    endpoint: IAudioEndpointVolume,
    callback: IAudioEndpointVolumeCallback,
}

impl VolumeSubscription {
    fn unregister(&self) {
        unsafe {
            let _ = self.endpoint.UnregisterControlChangeNotify(&self.callback);
        }
    }
}

#[derive(Default)]
struct EnforceResult {
    enabled_any: bool,
    disabled_ids: HashSet<String>,
    needs_reconcile: bool,
}

struct AudioWorker {
    state: Arc<Mutex<AppState>>,
    wake: WakeHandle,
    tx: mpsc::Sender<EngineMsg>,
    enumerator: IMMDeviceEnumerator,
    policy: PolicyConfig,
    notification_client: Option<IMMNotificationClient>,
    volumes: HashMap<String, VolumeSubscription>,
    last_enforced: HashMap<(DeviceFlow, i32, String), Instant>,
    suppress_auto_enforce_until: Option<Instant>,
    topology_due: Option<Instant>,
    enforce_due: Option<Instant>,
    save_due: Option<Instant>,
    trim_due: Option<Instant>,
}

fn run_worker(
    state: Arc<Mutex<AppState>>,
    wake: WakeHandle,
    tx: mpsc::Sender<EngineMsg>,
    rx: mpsc::Receiver<EngineMsg>,
    ready_tx: mpsc::SyncSender<()>,
) {
    let _com = ComMtaGuard::init();

    let Ok(mut worker) = AudioWorker::new(state, wake, tx) else {
        let _ = ready_tx.send(());
        return;
    };

    worker.initialize();
    let _ = ready_tx.send(());

    let mut pending_msg: Option<EngineMsg> = None;
    loop {
        let msg = if let Some(queued) = pending_msg.take() {
            Some(queued)
        } else {
            match worker.next_deadline() {
                None => match rx.recv() {
                    Ok(msg) => Some(msg),
                    Err(_) => break,
                },
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        match rx.try_recv() {
                            Ok(msg) => Some(msg),
                            Err(mpsc::TryRecvError::Empty) => None,
                            Err(mpsc::TryRecvError::Disconnected) => break,
                        }
                    } else {
                        match rx.recv_timeout(deadline - now) {
                            Ok(msg) => Some(msg),
                            Err(mpsc::RecvTimeoutError::Timeout) => None,
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                    }
                }
            }
        };

        if let Some(msg) = msg {
            match msg {
                EngineMsg::ApplyAction { action, reply } => {
                    let updated = worker.handle_apply_action(action);
                    let _ = reply.send(updated);
                }
                EngineMsg::ApplyActionAsync { mut action } => {
                    if let AppAction::SetVolume { device_id, .. } = &action {
                        let target_id = device_id.clone();
                        loop {
                            match rx.try_recv() {
                                Ok(EngineMsg::ApplyActionAsync {
                                    action:
                                        AppAction::SetVolume {
                                            device_id: next_id,
                                            volume: next_vol,
                                        },
                                }) if next_id == target_id => {
                                    action = AppAction::SetVolume {
                                        device_id: next_id,
                                        volume: next_vol,
                                    };
                                }
                                Ok(other) => {
                                    pending_msg = Some(other);
                                    break;
                                }
                                Err(_) => break,
                            }
                        }
                    }
                    let _ = worker.handle_apply_action(action);
                }
                EngineMsg::ToggleActiveMicsMute { reply } => {
                    let updated = worker.handle_toggle_active_mics_mute();
                    let _ = reply.send(updated);
                }
                EngineMsg::DefaultChanged {
                    flow,
                    role,
                    device_id,
                } => {
                    worker.handle_default_changed(flow, role, device_id);
                }
                EngineMsg::TopologyChanged => {
                    worker.schedule_topology_refresh();
                }
                EngineMsg::VolumeChanged {
                    device_id,
                    volume,
                    muted,
                } => {
                    worker.handle_volume_changed(device_id, volume, muted);
                }
                EngineMsg::PropertyChanged { device_id, key } => {
                    worker.handle_property_changed(device_id, key);
                }
                EngineMsg::Shutdown => break,
            }
        }

        worker.process_due_timers();
    }

    worker.flush_save();
    worker.cleanup();
}

impl AudioWorker {
    fn new(
        state: Arc<Mutex<AppState>>,
        wake: WakeHandle,
        tx: mpsc::Sender<EngineMsg>,
    ) -> winaudio::core::Result<Self> {
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        let policy = PolicyConfig::new()?;
        Ok(Self {
            state,
            wake,
            tx,
            enumerator,
            policy,
            notification_client: None,
            volumes: HashMap::new(),
            last_enforced: HashMap::new(),
            suppress_auto_enforce_until: None,
            topology_due: None,
            enforce_due: None,
            save_due: None,
            trim_due: None,
        })
    }

    fn next_deadline(&self) -> Option<Instant> {
        earliest_deadline([
            self.topology_due,
            self.enforce_due,
            self.save_due,
            self.trim_due,
        ])
    }

    fn schedule_topology_refresh(&mut self) {
        self.topology_due = Some(Instant::now() + TOPOLOGY_COALESCE_DELAY);
    }

    fn schedule_enforce(&mut self, when: Instant) {
        self.enforce_due = Some(match self.enforce_due {
            Some(existing) => existing.max(when),
            None => when,
        });
    }

    fn schedule_save(&mut self) {
        self.save_due = Some(Instant::now() + SAVE_DEBOUNCE_DELAY);
    }

    fn flush_save(&mut self) {
        if self.save_due.take().is_some() {
            let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let _ = store::save_state(&guard);
        }
    }

    fn schedule_trim(&mut self) {
        self.trim_due = Some(Instant::now() + TRIM_IDLE_DELAY);
    }

    fn process_due_timers(&mut self) {
        if self.topology_due.is_some_and(|due| Instant::now() >= due) {
            self.topology_due = None;
            self.handle_topology_changed();
        }
        if self.enforce_due.is_some_and(|due| Instant::now() >= due) {
            self.enforce_due = None;
            self.handle_enforce_due();
        }
        if self.save_due.is_some_and(|due| Instant::now() >= due) {
            self.flush_save();
        }
        if self.trim_due.is_some_and(|due| Instant::now() >= due) {
            self.trim_due = None;
            win32::trim_working_set();
        }
    }

    fn handle_enforce_due(&mut self) {
        if let Some(until) = self.suppress_auto_enforce_until
            && Instant::now() < until
        {
            self.schedule_enforce(until + LOCK_FOLLOWUP_BUFFER);
            return;
        }
        let snapshot = {
            let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard.clone()
        };
        if snapshot.lock_enabled && self.enforce_locked_roles(&snapshot) {
            self.schedule_trim();
        }
    }

    fn initialize(&mut self) {
        let client: IMMNotificationClient = DeviceNotificationClient {
            tx: self.tx.clone(),
        }
        .into();
        if unsafe { self.enumerator.RegisterEndpointNotificationCallback(&client) }.is_ok() {
            self.notification_client = Some(client);
        }

        let initial_state = {
            let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard.clone()
        };
        self.ensure_locked_roles_enabled(&initial_state);

        let (live_devices, live_roles) = self.enumerate_and_subscribe(&HashSet::new());

        let mut snapshot = {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard.reconcile_live_devices(live_devices, live_roles);
            if guard.auto_disable_enhancements {
                guard.set_auto_disable_enhancements(true);
            }
            let _ = store::save_state(&guard);
            guard.clone()
        };

        if snapshot.auto_disable_unused_devices {
            let enforced = self.enforce_unused_devices(&snapshot);
            if enforced.enabled_any || enforced.needs_reconcile {
                snapshot = self.refresh_live_state_excluding_pending(&enforced.disabled_ids);
            } else if !enforced.disabled_ids.is_empty() {
                snapshot = self.mark_devices_disabled(&enforced.disabled_ids);
            }
        }
        if snapshot.auto_disable_enhancements {
            self.disable_all_active_enhancements(&snapshot);
        }
        if snapshot.lock_enabled {
            if self.ensure_locked_roles_enabled(&snapshot) {
                snapshot = self.refresh_live_state();
                if snapshot.auto_disable_enhancements {
                    self.disable_all_active_enhancements(&snapshot);
                }
            }
            self.enforce_locked_roles(&snapshot);
        }
        win32::trim_working_set();
    }

    fn suppress_auto_enforce(&mut self) {
        self.suppress_auto_enforce_until = Some(Instant::now() + AUTO_ENFORCE_SUPPRESS_WINDOW);
    }

    fn is_auto_enforce_suppressed(&self) -> bool {
        self.suppress_auto_enforce_until
            .is_some_and(|until| Instant::now() < until)
    }

    fn device_state(&self, device_id: &str) -> Option<DEVICE_STATE> {
        let wide = win32::to_wide(device_id);
        let device = unsafe { self.enumerator.GetDevice(PCWSTR(wide.as_ptr())).ok()? };
        unsafe { device.GetState().ok() }
    }

    fn wait_for_device_state(&self, device_id: &str, target: DEVICE_STATE) -> bool {
        let deadline = Instant::now() + VISIBILITY_WAIT_TIMEOUT;
        loop {
            if self.device_state(device_id) == Some(target) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(VISIBILITY_POLL_INTERVAL);
        }
    }

    fn ensure_device_enabled(&self, device_id: &str) -> bool {
        if self.device_state(device_id) == Some(DEVICE_STATE_ACTIVE) {
            return true;
        }
        if self.policy.set_endpoint_visibility(device_id, true).is_err() {
            return false;
        }
        self.wait_for_device_state(device_id, DEVICE_STATE_ACTIVE)
    }

    fn enable_devices_batch(&self, ids: &[String]) -> Vec<String> {
        let mut seen = HashSet::new();
        let to_enable: Vec<&String> = ids
            .iter()
            .filter(|id| {
                seen.insert(id.as_str()) && self.device_state(id) != Some(DEVICE_STATE_ACTIVE)
            })
            .collect();
        if to_enable.is_empty() {
            return Vec::new();
        }

        let mut pending: Vec<String> = to_enable
            .into_iter()
            .filter(|id| self.policy.set_endpoint_visibility(id, true).is_ok())
            .cloned()
            .collect();
        if pending.is_empty() {
            return Vec::new();
        }

        let mut enabled = Vec::with_capacity(pending.len());
        let deadline = Instant::now() + VISIBILITY_WAIT_TIMEOUT;
        loop {
            let mut still_pending = Vec::new();
            for id in pending {
                if self.device_state(&id) == Some(DEVICE_STATE_ACTIVE) {
                    enabled.push(id);
                } else {
                    still_pending.push(id);
                }
            }
            pending = still_pending;
            if pending.is_empty() || Instant::now() >= deadline {
                break;
            }
            thread::sleep(VISIBILITY_POLL_INTERVAL);
        }
        enabled
    }

    fn refresh_live_state(&mut self) -> AppState {
        self.topology_due = None;
        self.refresh_live_state_excluding_pending(&HashSet::new())
    }

    fn refresh_live_state_excluding_pending(
        &mut self,
        pending_disabled: &HashSet<String>,
    ) -> AppState {
        if pending_disabled.is_empty() {
            self.topology_due = None;
        }
        let (live_devices, live_roles) = self.enumerate_and_subscribe(pending_disabled);
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.reconcile_live_devices(live_devices, live_roles);
        if guard.auto_disable_enhancements {
            guard.set_auto_disable_enhancements(true);
        }
        self.save_due = None;
        let _ = store::save_state(&guard);
        guard.clone()
    }

    fn mark_devices_disabled(&mut self, disabled_ids: &HashSet<String>) -> AppState {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut changed = false;
        for dev in &mut guard.devices {
            if disabled_ids.contains(&dev.id) && !dev.disabled {
                dev.disabled = true;
                changed = true;
            }
        }
        if changed {
            self.save_due = None;
            let _ = store::save_state(&guard);
        }
        guard.clone()
    }

    fn enforce_unused_devices(&mut self, state: &AppState) -> EnforceResult {
        if !state.auto_disable_unused_devices {
            return EnforceResult::default();
        }

        let in_use_ids = in_use_device_ids(state);
        let mut result = EnforceResult::default();

        let to_enable: Vec<String> = state
            .devices
            .iter()
            .filter(|dev| {
                in_use_ids.contains(&dev.id)
                    && self.device_state(&dev.id) != Some(DEVICE_STATE_ACTIVE)
            })
            .map(|dev| dev.id.clone())
            .collect();

        if !to_enable.is_empty() {
            self.suppress_auto_enforce();
            let enabled = self.enable_devices_batch(&to_enable);
            if !enabled.is_empty() {
                result.enabled_any = true;
                if state.auto_disable_enhancements {
                    for id in &enabled {
                        if let Some(dev) = state.device(id) {
                            self.disable_device_enhancements(id, dev.flow, false);
                        }
                    }
                }
            }
        }

        if state.lock_enabled {
            self.enforce_locked_roles(state);
        }

        let active_ids: HashSet<String> = state
            .devices
            .iter()
            .filter(|dev| self.device_state(&dev.id) == Some(DEVICE_STATE_ACTIVE))
            .map(|dev| dev.id.clone())
            .collect();
        let mut active_by_flow = count_active_by_flow(&state.devices, &active_ids);

        let mut to_disable = Vec::new();
        for dev in &state.devices {
            if in_use_ids.contains(&dev.id) || !active_ids.contains(&dev.id) {
                continue;
            }
            let idx = flow_index(dev.flow);
            if active_by_flow[idx] <= 1 {
                result.needs_reconcile = true;
                continue;
            }
            active_by_flow[idx] -= 1;
            to_disable.push(dev.id.clone());
        }

        if !to_disable.is_empty() {
            self.suppress_auto_enforce();
            for id in to_disable {
                if self.policy.set_endpoint_visibility(&id, false).is_ok() {
                    result.disabled_ids.insert(id);
                } else {
                    result.needs_reconcile = true;
                }
            }
        }

        result
    }

    fn cleanup(&mut self) {
        self.clear_volume_subscriptions();
        if let Some(client) = self.notification_client.take() {
            unsafe {
                let _ = self.enumerator.UnregisterEndpointNotificationCallback(&client);
            }
        }
    }

    fn clear_volume_subscriptions(&mut self) {
        for sub in self.volumes.values() {
            sub.unregister();
        }
        self.volumes.clear();
    }

    fn enumerate_and_subscribe(
        &mut self,
        pending_disabled: &HashSet<String>,
    ) -> (Vec<AudioDevice>, RoleAssignments) {
        let mut devices = Vec::new();
        if let Ok(collection) = unsafe {
            self.enumerator.EnumAudioEndpoints(
                eAll,
                DEVICE_STATE(DEVICE_STATE_ACTIVE.0 | DEVICE_STATE_DISABLED.0),
            )
        } {
            let mut active_ids = HashSet::new();
            let count = unsafe { collection.GetCount().unwrap_or(0) };
            for index in 0..count {
                let Ok(device) = (unsafe { collection.Item(index) }) else {
                    continue;
                };
                if let Some(info) = self.inspect_and_subscribe_device(&device, pending_disabled) {
                    if !info.disabled {
                        active_ids.insert(info.id.clone());
                    }
                    devices.push(info);
                }
            }
            self.volumes.retain(|id, sub| {
                if active_ids.contains(id) {
                    true
                } else {
                    sub.unregister();
                    false
                }
            });
        }

        let roles = self.read_live_roles();
        (devices, roles)
    }

    fn inspect_and_subscribe_device(
        &mut self,
        device: &IMMDevice,
        pending_disabled: &HashSet<String>,
    ) -> Option<AudioDevice> {
        let id = read_device_id(device)?;
        let endpoint: IMMEndpoint = device.cast().ok()?;
        let flow = flow_from_win32(unsafe { endpoint.GetDataFlow().ok()? })?;
        let disabled = unsafe { device.GetState().ok()? } == DEVICE_STATE_DISABLED
            || pending_disabled.contains(&id);
        let (name, subtitle) = read_device_names(device);

        // A disabled endpoint has no live volume, enhancement or listen state worth reading,
        // and those property calls are slow on virtual/hung drivers.
        let (volume, muted, enhancements, listen) = if disabled {
            (
                0,
                false,
                EnhancementsState::default(),
                (flow == DeviceFlow::Input).then(ListenConfig::default),
            )
        } else {
            let (volume, muted) = self.read_and_ensure_volume_subscription(&id, device);
            (
                volume,
                muted,
                self.read_enhancements(&id, flow, false),
                (flow == DeviceFlow::Input).then(|| self.read_listen(&id)),
            )
        };

        Some(AudioDevice {
            id,
            name,
            subtitle,
            flow,
            volume,
            muted,
            pinned: false,
            disabled,
            enhancements,
            previous_enhancements: None,
            listen,
        })
    }

    fn read_and_ensure_volume_subscription(&mut self, id: &str, device: &IMMDevice) -> (u8, bool) {
        if let Some(sub) = self.volumes.get(id)
            && let Some(values) = read_endpoint_volume(&sub.endpoint)
        {
            return values;
        }
        if let Some(stale) = self.volumes.remove(id) {
            stale.unregister();
        }

        match unsafe { device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) } {
            Ok(vol) => {
                let scalar = unsafe { vol.GetMasterVolumeLevelScalar().unwrap_or(1.0) };
                let vol_pct = (scalar.clamp(0.0, 1.0) * 100.0).round() as u8;
                let is_muted = unsafe { vol.GetMute().map(|b| b.as_bool()).unwrap_or(false) };

                let callback: IAudioEndpointVolumeCallback = EndpointVolumeCallback {
                    device_id: id.to_string(),
                    tx: self.tx.clone(),
                }
                .into();
                if unsafe { vol.RegisterControlChangeNotify(&callback) }.is_ok() {
                    self.volumes.insert(
                        id.to_string(),
                        VolumeSubscription {
                            endpoint: vol,
                            callback,
                        },
                    );
                }
                (vol_pct, is_muted)
            }
            Err(_) => (100, false),
        }
    }

    fn read_live_roles(&self) -> RoleAssignments {
        RoleAssignments {
            default_output_id: self.get_default_endpoint_id(eRender, eConsole),
            comms_output_id: self.get_default_endpoint_id(eRender, eCommunications),
            default_input_id: self.get_default_endpoint_id(eCapture, eConsole),
            comms_input_id: self.get_default_endpoint_id(eCapture, eCommunications),
        }
    }

    fn get_default_endpoint_id(&self, flow: EDataFlow, role: ERole) -> String {
        unsafe { self.enumerator.GetDefaultAudioEndpoint(flow, role) }
            .ok()
            .and_then(|dev| read_device_id(&dev))
            .unwrap_or_default()
    }

    fn set_default_endpoint_if_needed(&self, flow: DeviceFlow, role: ERole, device_id: &str) {
        if self.get_default_endpoint_id(flow_to_win32(flow), role) != device_id {
            let _ = self.policy.set_default_endpoint(device_id, role);
        }
    }

    fn read_sysfx_value(&self, device_id: &str) -> Option<u32> {
        self.policy
            .get_property_value(device_id, true, &PKEY_AudioEndpoint_Disable_SysFx)
            .ok()
            .and_then(|pv| u32::try_from(&pv).ok())
    }

    fn read_enhancements(
        &self,
        device_id: &str,
        flow: DeviceFlow,
        disabled: bool,
    ) -> EnhancementsState {
        let sys_fx_enabled = self
            .policy
            .get_property_value(device_id, true, &PKEY_AudioEndpoint_Disable_SysFx)
            .ok()
            .map(|pv| u32::try_from(&pv).ok() != Some(ENDPOINT_SYSFX_DISABLED))
            .unwrap_or(false);
        let spatial_sound_enabled = if !disabled && flow == DeviceFlow::Output {
            spatial::is_spatial_enabled(device_id)
        } else {
            false
        };
        EnhancementsState {
            sys_fx_enabled,
            advanced_enabled: false,
            spatial_sound_enabled,
            previous_spatial_format: None,
        }
    }

    fn read_listen(&self, device_id: &str) -> ListenConfig {
        let enabled = self
            .policy
            .get_property_value(device_id, false, &PKEY_LISTEN_ENABLED)
            .ok()
            .and_then(|pv| bool::try_from(&pv).ok())
            .unwrap_or(false);

        let raw_target = self
            .policy
            .get_property_value(device_id, false, &PKEY_LISTEN_DEVICE)
            .ok()
            .and_then(|pv| {
                if pv.is_empty() {
                    None
                } else {
                    propvariant_to_string(&pv).filter(|s| !s.is_empty())
                }
            });

        let target_device_id = match raw_target {
            Some(target) => Some(target),
            None if enabled => Some(LISTEN_TARGET_DEFAULT_OUTPUT.to_string()),
            None => None,
        };

        ListenConfig {
            enabled,
            target_device_id,
        }
    }

    fn handle_topology_changed(&mut self) {
        let was_suppressed = self.is_auto_enforce_suppressed();
        let prior_state = {
            let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard.clone()
        };
        let reenabled_locked = self.ensure_locked_roles_enabled(&prior_state);

        let mut snapshot = self.refresh_live_state();

        if snapshot.auto_disable_unused_devices && (!was_suppressed || reenabled_locked) {
            let enforced = self.enforce_unused_devices(&snapshot);
            if enforced.enabled_any || enforced.needs_reconcile {
                snapshot = self.refresh_live_state_excluding_pending(&enforced.disabled_ids);
            } else if !enforced.disabled_ids.is_empty() {
                snapshot = self.mark_devices_disabled(&enforced.disabled_ids);
            }
        }
        if snapshot.auto_disable_enhancements {
            self.disable_all_active_enhancements(&snapshot);
        }
        if snapshot.lock_enabled {
            if self.ensure_locked_roles_enabled(&snapshot) {
                snapshot = self.refresh_live_state();
                if snapshot.auto_disable_enhancements {
                    self.disable_all_active_enhancements(&snapshot);
                }
            }
            self.enforce_locked_roles(&snapshot);
        }
        self.resync_comms_listen_targets(&snapshot);

        self.wake.post(WM_APP_REFRESH_TRAY);
        self.schedule_trim();
    }

    fn handle_default_changed(&mut self, flow: DeviceFlow, role: ERole, device_id: String) {
        let (lock_enabled, expected, should_enforce) = {
            let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if guard.lock_enabled {
                let expected = expected_role_device(&guard, flow, role).to_string();
                let valid = !expected.is_empty()
                    && device_id != expected
                    && guard.has_device_with_flow(flow, &expected)
                    && guard.device(&expected).is_some_and(|d| !d.disabled);
                (true, expected, valid)
            } else {
                (false, String::new(), false)
            }
        };

        if lock_enabled {
            if should_enforce {
                let now = Instant::now();
                if let Some(until) = self.suppress_auto_enforce_until
                    && now < until
                {
                    self.schedule_enforce(until + LOCK_FOLLOWUP_BUFFER);
                } else if self.device_state(&expected) == Some(DEVICE_STATE_ACTIVE) {
                    let key = (flow, role.0, expected.clone());
                    if let Some(&at) = self.last_enforced.get(&key)
                        && now < at + LOCK_GUARD_COOLDOWN
                    {
                        self.schedule_enforce(at + LOCK_GUARD_COOLDOWN + LOCK_FOLLOWUP_BUFFER);
                    } else {
                        self.last_enforced.insert(key, now);
                        let _ = self.policy.set_default_endpoint(&expected, role);
                        self.schedule_trim();
                    }
                }
            }
            return;
        }

        if device_id.is_empty() {
            return;
        }

        let (changed, comms_output_changed, auto_disable_unused, resync_snapshot) = {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if !guard.has_device_with_flow(flow, &device_id) {
                return;
            }
            let old_roles = guard.roles.clone();
            let old_comms_output = guard.roles.comms_output_id.clone();

            match (flow, role) {
                (DeviceFlow::Output, r) if r == eConsole => {
                    guard.roles.default_output_id = device_id.clone();
                    if !guard.separate_comms_role {
                        guard.roles.comms_output_id = device_id;
                    }
                }
                (DeviceFlow::Output, r) if r == eCommunications && guard.separate_comms_role => {
                    guard.roles.comms_output_id = device_id;
                }
                (DeviceFlow::Input, r) if r == eConsole => {
                    guard.roles.default_input_id = device_id.clone();
                    if !guard.separate_comms_role {
                        guard.roles.comms_input_id = device_id;
                    }
                }
                (DeviceFlow::Input, r) if r == eCommunications && guard.separate_comms_role => {
                    guard.roles.comms_input_id = device_id;
                }
                _ => {}
            }

            let changed = guard.roles != old_roles;
            let comms_output_changed = guard.roles.comms_output_id != old_comms_output;
            if changed {
                self.save_due = None;
                let _ = store::save_state(&guard);
            }
            let auto_disable_unused = guard.auto_disable_unused_devices;
            let snapshot =
                (comms_output_changed || (changed && auto_disable_unused)).then(|| guard.clone());
            (changed, comms_output_changed, auto_disable_unused, snapshot)
        };

        if let Some(snapshot) = resync_snapshot {
            if comms_output_changed {
                self.resync_comms_listen_targets(&snapshot);
            }
            if changed && auto_disable_unused && !self.is_auto_enforce_suppressed() {
                let enforced = self.enforce_unused_devices(&snapshot);
                if enforced.enabled_any || enforced.needs_reconcile {
                    let _ = self.refresh_live_state_excluding_pending(&enforced.disabled_ids);
                } else if !enforced.disabled_ids.is_empty() {
                    let _ = self.mark_devices_disabled(&enforced.disabled_ids);
                }
            }
        }
        if changed {
            self.wake.post(WM_APP_REFRESH_TRAY);
            self.schedule_trim();
        }
    }

    fn handle_volume_changed(&mut self, device_id: String, volume: u8, muted: bool) {
        let changed = {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard.set_device_audio_state(&device_id, volume, muted)
        };
        if !changed {
            return;
        }

        self.schedule_save();
        self.schedule_trim();
        self.wake.post(WM_APP_REFRESH_TRAY);
    }

    fn handle_property_changed(&mut self, device_id: String, key: PROPERTYKEY) {
        let device_info = {
            let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard.device(&device_id).map(|d| (d.flow, d.disabled))
        };
        let Some((flow, disabled)) = device_info else {
            return;
        };

        if key == PKEY_AudioEndpoint_Disable_SysFx {
            let enhancements = self.read_enhancements(&device_id, flow, disabled);
            let auto_disable = {
                let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
                guard.auto_disable_enhancements
            };

            let updated = if auto_disable && enhancements.any_enabled() {
                self.disable_device_enhancements(&device_id, flow, disabled);
                let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
                guard.set_device_enhancements(&device_id, EnhancementsState::all_disabled())
            } else {
                let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
                guard.set_device_enhancements(&device_id, enhancements)
            };
            if updated {
                self.schedule_save();
                self.schedule_trim();
                self.wake.post(WM_APP_REFRESH_TRAY);
            }
        } else if key == PKEY_LISTEN_ENABLED || key == PKEY_LISTEN_DEVICE {
            if flow != DeviceFlow::Input {
                return;
            }
            let live_listen = self.read_listen(&device_id);
            let updated = {
                let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let reconciled = reconcile_live_listen(
                    guard.device(&device_id).and_then(|d| d.listen.as_ref()),
                    live_listen,
                    &guard.roles.comms_output_id,
                );
                guard.set_device_listen(&device_id, reconciled)
            };
            if updated {
                self.schedule_save();
                self.schedule_trim();
                self.wake.post(WM_APP_REFRESH_TRAY);
            }
        } else if key == PKEY_Device_DeviceDesc
            || key == PKEY_DeviceInterface_FriendlyName
            || key == PKEY_Device_FriendlyName
        {
            let wide = win32::to_wide(&device_id);
            let Ok(device) = (unsafe { self.enumerator.GetDevice(PCWSTR(wide.as_ptr())) }) else {
                return;
            };
            let (name, subtitle) = read_device_names(&device);
            let changed = {
                let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(dev) = guard.devices.iter_mut().find(|d| d.id == device_id)
                    && (dev.name != name || dev.subtitle != subtitle)
                {
                    dev.name = name;
                    dev.subtitle = subtitle;
                    true
                } else {
                    false
                }
            };
            if changed {
                self.schedule_save();
                self.schedule_trim();
                self.wake.post(WM_APP_REFRESH_TRAY);
            }
        }
    }

    fn handle_apply_action(&mut self, action: AppAction) -> AppState {
        let defer_save = matches!(action, AppAction::SetVolume { .. });
        let mut previous_roles: Option<RoleAssignments> = None;
        let mut disable_accepted = true;
        let mut spatial_formats_to_restore: Vec<(String, String)> = Vec::new();
        let mut snapshot = {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            match &action {
                AppAction::SetEnhancementsDisabled {
                    device_id,
                    disabled: false,
                } => {
                    if let Some(dev) = guard.device(device_id)
                        && !dev.enhancements.any_enabled()
                        && !dev.disabled
                        && dev.flow == DeviceFlow::Output
                        && let Some(fmt) = saved_spatial_format(dev)
                    {
                        spatial_formats_to_restore.push((device_id.clone(), fmt.to_string()));
                    }
                }
                AppAction::ReenableAllEnhancements => {
                    for dev in &guard.devices {
                        if !dev.enhancements.any_enabled()
                            && !dev.disabled
                            && dev.flow == DeviceFlow::Output
                            && let Some(fmt) = saved_spatial_format(dev)
                        {
                            spatial_formats_to_restore.push((dev.id.clone(), fmt.to_string()));
                        }
                    }
                }
                _ => {}
            }
            match &action {
                AppAction::SetDeviceDisabled {
                    device_id,
                    disabled,
                } => {
                    if guard.auto_disable_unused_devices {
                        disable_accepted = false;
                    } else {
                        previous_roles = Some(guard.roles.clone());
                        disable_accepted = guard.set_device_disabled(device_id, *disabled);
                    }
                }
                _ => {
                    guard.apply_action(&action);
                    if matches!(action, AppAction::ApplyProfile { .. })
                        && guard.auto_disable_enhancements
                    {
                        guard.set_auto_disable_enhancements(true);
                    }
                }
            }
            if !defer_save && disable_accepted {
                self.save_due = None;
                let _ = store::save_state(&guard);
            }
            guard.clone()
        };
        if defer_save {
            self.schedule_save();
        }

        match action {
            AppAction::SetLockEnabled(true) => {
                self.enforce_locked_roles(&snapshot);
            }
            AppAction::SetSeparateCommsRole(false) => {
                for (flow, target_id) in [
                    (DeviceFlow::Output, snapshot.roles.comms_output_id.as_str()),
                    (DeviceFlow::Input, snapshot.roles.comms_input_id.as_str()),
                ] {
                    if !target_id.is_empty()
                        && snapshot.has_device_with_flow(flow, target_id)
                        && self.device_state(target_id) == Some(DEVICE_STATE_ACTIVE)
                    {
                        if snapshot.lock_enabled {
                            self.last_enforced.insert(
                                (flow, eCommunications.0, target_id.to_string()),
                                Instant::now(),
                            );
                        }
                        self.set_default_endpoint_if_needed(flow, eCommunications, target_id);
                    }
                }
                if snapshot.lock_enabled {
                    self.enforce_locked_roles(&snapshot);
                }
                self.resync_comms_listen_targets(&snapshot);
                if snapshot.auto_disable_unused_devices {
                    let enforced = self.enforce_unused_devices(&snapshot);
                    if enforced.enabled_any || enforced.needs_reconcile {
                        snapshot = self.refresh_live_state_excluding_pending(&enforced.disabled_ids);
                    } else if !enforced.disabled_ids.is_empty() {
                        snapshot = self.mark_devices_disabled(&enforced.disabled_ids);
                    }
                }
            }
            AppAction::SetDefault { flow, device_id } => {
                if snapshot.has_device_with_flow(flow, &device_id) {
                    let was_disabled =
                        self.device_state(&device_id) != Some(DEVICE_STATE_ACTIVE);
                    if was_disabled || snapshot.auto_disable_unused_devices {
                        self.suppress_auto_enforce();
                    }
                    let mut spatial_recorded = false;
                    if self.ensure_device_enabled(&device_id) {
                        self.set_default_endpoint_if_needed(flow, eConsole, &device_id);
                        self.set_default_endpoint_if_needed(flow, eMultimedia, &device_id);
                        if !snapshot.separate_comms_role {
                            self.set_default_endpoint_if_needed(flow, eCommunications, &device_id);
                            if flow == DeviceFlow::Output {
                                self.resync_comms_listen_targets(&snapshot);
                            }
                        }
                        if snapshot.auto_disable_enhancements {
                            spatial_recorded =
                                self.disable_device_enhancements(&device_id, flow, false);
                        }
                    }
                    let enforced = if snapshot.auto_disable_unused_devices {
                        self.enforce_unused_devices(&snapshot)
                    } else {
                        EnforceResult::default()
                    };
                    if was_disabled || enforced.enabled_any || enforced.needs_reconcile {
                        snapshot = self.refresh_live_state_excluding_pending(&enforced.disabled_ids);
                    } else if !enforced.disabled_ids.is_empty() {
                        snapshot = self.mark_devices_disabled(&enforced.disabled_ids);
                    } else if spatial_recorded {
                        snapshot = self.state.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    }
                }
            }
            AppAction::SetComms { flow, device_id } => {
                if snapshot.separate_comms_role && snapshot.has_device_with_flow(flow, &device_id) {
                    let was_disabled =
                        self.device_state(&device_id) != Some(DEVICE_STATE_ACTIVE);
                    if was_disabled || snapshot.auto_disable_unused_devices {
                        self.suppress_auto_enforce();
                    }
                    let mut spatial_recorded = false;
                    if self.ensure_device_enabled(&device_id) {
                        self.set_default_endpoint_if_needed(flow, eCommunications, &device_id);
                        if flow == DeviceFlow::Output {
                            self.resync_comms_listen_targets(&snapshot);
                        }
                        if snapshot.auto_disable_enhancements {
                            spatial_recorded =
                                self.disable_device_enhancements(&device_id, flow, false);
                        }
                    }
                    let enforced = if snapshot.auto_disable_unused_devices {
                        self.enforce_unused_devices(&snapshot)
                    } else {
                        EnforceResult::default()
                    };
                    if was_disabled || enforced.enabled_any || enforced.needs_reconcile {
                        snapshot = self.refresh_live_state_excluding_pending(&enforced.disabled_ids);
                    } else if !enforced.disabled_ids.is_empty() {
                        snapshot = self.mark_devices_disabled(&enforced.disabled_ids);
                    } else if spatial_recorded {
                        snapshot = self.state.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    }
                }
            }
            AppAction::SetVolume { device_id, volume } => {
                self.set_endpoint_volume(&device_id, volume);
            }
            AppAction::ToggleMute { device_id } => {
                if let Some(dev) = snapshot.device(&device_id) {
                    self.set_endpoint_mute(&device_id, dev.muted);
                }
            }
            AppAction::SetMute { device_id, muted } => {
                if snapshot.device(&device_id).is_some() {
                    self.set_endpoint_mute(&device_id, muted);
                }
            }
            AppAction::SetEnhancementsDisabled {
                device_id,
                disabled,
            } => {
                if let Some((flow, dev_disabled, enable_sysfx)) = snapshot
                    .device(&device_id)
                    .map(|dev| {
                        (
                            dev.flow,
                            dev.disabled,
                            dev.enhancements.sys_fx_enabled || dev.enhancements.advanced_enabled,
                        )
                    })
                {
                    if disabled {
                        if self.disable_device_enhancements(&device_id, flow, dev_disabled) {
                            snapshot = self.state.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        }
                    } else {
                        if enable_sysfx {
                            self.enable_device_sysfx(&device_id);
                        }
                        for (id, fmt) in spatial_formats_to_restore {
                            let _ = spatial::set_spatial_format(&id, &fmt);
                        }
                    }
                }
            }
            AppAction::SetAutoDisableEnhancements(true) => {
                if self.disable_all_active_enhancements(&snapshot) {
                    snapshot = self.state.lock().unwrap_or_else(|e| e.into_inner()).clone();
                }
            }
            AppAction::ReenableAllEnhancements => {
                for dev in &snapshot.devices {
                    if !dev.disabled
                        && (dev.enhancements.sys_fx_enabled || dev.enhancements.advanced_enabled)
                    {
                        self.enable_device_sysfx(&dev.id);
                    }
                }
                for (id, fmt) in spatial_formats_to_restore {
                    let _ = spatial::set_spatial_format(&id, &fmt);
                }
            }
            AppAction::ToggleDevicePin { .. } | AppAction::SetDevicePinned { .. } => {
                if snapshot.auto_disable_unused_devices {
                    let enforced = self.enforce_unused_devices(&snapshot);
                    if enforced.enabled_any || enforced.needs_reconcile {
                        snapshot = self.refresh_live_state_excluding_pending(&enforced.disabled_ids);
                    } else if !enforced.disabled_ids.is_empty() {
                        snapshot = self.mark_devices_disabled(&enforced.disabled_ids);
                    }
                }
            }
            AppAction::SetAutoDisableUnusedDevices(false) => {
                // Turning the feature off must undo its effect: leaving endpoints disabled here
                // breaks audio for apps that still select them, with no obvious way back.
                snapshot = self.reenable_all_devices(snapshot);
            }
            AppAction::SetAutoDisableUnusedDevices(true) => {
                let enforced = self.enforce_unused_devices(&snapshot);
                if enforced.enabled_any || enforced.needs_reconcile {
                    snapshot = self.refresh_live_state_excluding_pending(&enforced.disabled_ids);
                } else if !enforced.disabled_ids.is_empty() {
                    snapshot = self.mark_devices_disabled(&enforced.disabled_ids);
                }
            }
            AppAction::ReenableAllDevices => {
                snapshot = self.reenable_all_devices(snapshot);
            }
            AppAction::SetDeviceDisabled {
                device_id,
                disabled,
            } => {
                if disable_accepted
                    && !snapshot.auto_disable_unused_devices
                    && let Some(dev) = snapshot.device(&device_id)
                {
                    let flow = dev.flow;
                    if disabled {
                        let has_other_active = snapshot.devices.iter().any(|other| {
                            other.flow == flow
                                && other.id != device_id
                                && self.device_state(&other.id) == Some(DEVICE_STATE_ACTIVE)
                        });
                        if !has_other_active {
                            snapshot = self.refresh_live_state();
                        } else {
                            self.suppress_auto_enforce();
                            if let Some(prev) = previous_roles.as_ref()
                                && *prev != snapshot.roles
                            {
                                self.apply_reassigned_roles(prev, &snapshot);
                            }
                            if self.device_state(&device_id) != Some(DEVICE_STATE_DISABLED)
                                && self.policy.set_endpoint_visibility(&device_id, false).is_err()
                            {
                                snapshot = self.refresh_live_state();
                            }
                        }
                    } else {
                        self.suppress_auto_enforce();
                        if self.ensure_device_enabled(&device_id) {
                            if snapshot.auto_disable_enhancements {
                                self.disable_device_enhancements(&device_id, flow, false);
                            }
                            if snapshot.lock_enabled {
                                self.enforce_locked_roles(&snapshot);
                            }
                        }
                        snapshot = self.refresh_live_state();
                    }
                }
            }
            AppAction::SetListenEnabled { device_id, .. }
            | AppAction::SetListenTarget { device_id, .. } => {
                if let Some(dev) = snapshot.device(&device_id)
                    && dev.flow == DeviceFlow::Input
                    && let Some(listen) = dev.listen.as_ref()
                {
                    let mut enabled_target = false;
                    if !dev.disabled
                        && let Some(target_id) = resolve_listen_target_id(listen, &snapshot)
                        && self.device_state(target_id) != Some(DEVICE_STATE_ACTIVE)
                    {
                        self.suppress_auto_enforce();
                        if self.ensure_device_enabled(target_id) {
                            enabled_target = true;
                            if snapshot.auto_disable_enhancements {
                                self.disable_device_enhancements(
                                    target_id,
                                    DeviceFlow::Output,
                                    false,
                                );
                            }
                        }
                    }
                    self.apply_listen_config(&device_id, listen, &snapshot);
                    let enforced = if snapshot.auto_disable_unused_devices {
                        self.enforce_unused_devices(&snapshot)
                    } else {
                        EnforceResult::default()
                    };
                    if enabled_target || enforced.enabled_any || enforced.needs_reconcile {
                        snapshot = self.refresh_live_state_excluding_pending(&enforced.disabled_ids);
                    } else if !enforced.disabled_ids.is_empty() {
                        snapshot = self.mark_devices_disabled(&enforced.disabled_ids);
                    }
                }
            }
            AppAction::ApplyProfile { profile_id } => {
                if let Some(profile) = snapshot.profiles.iter().find(|p| p.id == profile_id) {
                    let pending_disabled = self.apply_profile_to_windows(&snapshot, profile);
                    snapshot = self.refresh_live_state_excluding_pending(&pending_disabled);
                }
            }
            _ => {}
        }

        self.wake.post(WM_APP_REFRESH_TRAY);
        self.schedule_trim();
        snapshot
    }

    fn handle_toggle_active_mics_mute(&mut self) -> AppState {
        let (target_muted, mic_ids, snapshot) = {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let target_muted = guard.toggle_active_mics_mute();
            let mut ids = Vec::new();
            for candidate in [
                guard.roles.default_input_id.as_str(),
                guard.roles.comms_input_id.as_str(),
            ] {
                if !candidate.is_empty()
                    && guard.has_device_with_flow(DeviceFlow::Input, candidate)
                    && !ids.iter().any(|existing: &String| existing == candidate)
                {
                    ids.push(candidate.to_string());
                }
            }
            self.save_due = None;
            let _ = store::save_state(&guard);
            (target_muted, ids, guard.clone())
        };

        for id in mic_ids {
            self.set_endpoint_mute(&id, target_muted);
        }

        self.wake.post(WM_APP_REFRESH_TRAY);
        self.schedule_trim();
        snapshot
    }

    /// Enables every endpoint that isn't active and returns the refreshed state. Shared by the
    /// explicit "re-enable all" action and by turning auto-disable off.
    fn reenable_all_devices(&mut self, snapshot: AppState) -> AppState {
        let ids: Vec<String> = snapshot
            .devices
            .iter()
            .filter(|dev| self.device_state(&dev.id) != Some(DEVICE_STATE_ACTIVE))
            .map(|dev| dev.id.clone())
            .collect();
        if !ids.is_empty() {
            self.suppress_auto_enforce();
            let enabled = self.enable_devices_batch(&ids);
            if snapshot.auto_disable_enhancements {
                for id in &enabled {
                    if let Some(dev) = snapshot.device(id) {
                        self.disable_device_enhancements(id, dev.flow, false);
                    }
                }
            }
        }
        self.refresh_live_state()
    }

    fn ensure_locked_roles_enabled(&mut self, state: &AppState) -> bool {        if !state.lock_enabled {
            return false;
        }
        let mut enabled_any = false;
        for id in locked_role_ids(state) {
            if self.device_state(id) == Some(DEVICE_STATE_DISABLED) {
                self.suppress_auto_enforce();
                if self.ensure_device_enabled(id) {
                    enabled_any = true;
                }
            }
        }
        enabled_any
    }

    fn apply_reassigned_roles(&mut self, previous: &RoleAssignments, current: &AppState) {
        let mut comms_output_changed = false;
        let comms_out = if current.separate_comms_role {
            current.roles.comms_output_id.as_str()
        } else {
            current.roles.default_output_id.as_str()
        };
        let comms_in = if current.separate_comms_role {
            current.roles.comms_input_id.as_str()
        } else {
            current.roles.default_input_id.as_str()
        };

        let role_updates = [
            (
                previous.default_output_id != current.roles.default_output_id,
                DeviceFlow::Output,
                eConsole,
                current.roles.default_output_id.as_str(),
            ),
            (
                previous.default_output_id != current.roles.default_output_id,
                DeviceFlow::Output,
                eMultimedia,
                current.roles.default_output_id.as_str(),
            ),
            (
                previous.comms_output_id != current.roles.comms_output_id
                    || (!current.separate_comms_role
                        && previous.default_output_id != current.roles.default_output_id),
                DeviceFlow::Output,
                eCommunications,
                comms_out,
            ),
            (
                previous.default_input_id != current.roles.default_input_id,
                DeviceFlow::Input,
                eConsole,
                current.roles.default_input_id.as_str(),
            ),
            (
                previous.default_input_id != current.roles.default_input_id,
                DeviceFlow::Input,
                eMultimedia,
                current.roles.default_input_id.as_str(),
            ),
            (
                previous.comms_input_id != current.roles.comms_input_id
                    || (!current.separate_comms_role
                        && previous.default_input_id != current.roles.default_input_id),
                DeviceFlow::Input,
                eCommunications,
                comms_in,
            ),
        ];

        for (changed, flow, role, target_id) in role_updates {
            if !changed
                || target_id.is_empty()
                || self.device_state(target_id) != Some(DEVICE_STATE_ACTIVE)
            {
                continue;
            }
            if current.lock_enabled {
                self.last_enforced
                    .insert((flow, role.0, target_id.to_string()), Instant::now());
            }
            self.set_default_endpoint_if_needed(flow, role, target_id);
            if flow == DeviceFlow::Output && role == eCommunications {
                comms_output_changed = true;
            }
        }

        if comms_output_changed {
            self.resync_comms_listen_targets(current);
        }
    }

    fn enforce_locked_roles(&mut self, state: &AppState) -> bool {
        let comms_out = if state.separate_comms_role {
            state.roles.comms_output_id.as_str()
        } else {
            state.roles.default_output_id.as_str()
        };
        let comms_in = if state.separate_comms_role {
            state.roles.comms_input_id.as_str()
        } else {
            state.roles.default_input_id.as_str()
        };

        let targets = [
            (
                DeviceFlow::Output,
                eRender,
                eConsole,
                state.roles.default_output_id.as_str(),
            ),
            (
                DeviceFlow::Output,
                eRender,
                eMultimedia,
                state.roles.default_output_id.as_str(),
            ),
            (DeviceFlow::Output, eRender, eCommunications, comms_out),
            (
                DeviceFlow::Input,
                eCapture,
                eConsole,
                state.roles.default_input_id.as_str(),
            ),
            (
                DeviceFlow::Input,
                eCapture,
                eMultimedia,
                state.roles.default_input_id.as_str(),
            ),
            (DeviceFlow::Input, eCapture, eCommunications, comms_in),
        ];

        let mut enforced_any = false;
        for (flow, win_flow, role, target_id) in targets {
            if target_id.is_empty()
                || !state.has_device_with_flow(flow, target_id)
                || self.device_state(target_id) != Some(DEVICE_STATE_ACTIVE)
            {
                continue;
            }
            let current = self.get_default_endpoint_id(win_flow, role);
            if current != target_id {
                self.last_enforced
                    .insert((flow, role.0, target_id.to_string()), Instant::now());
                let _ = self.policy.set_default_endpoint(target_id, role);
                enforced_any = true;
            }
        }
        enforced_any
    }

    fn apply_profile_to_windows(
        &mut self,
        state: &AppState,
        profile: &crate::audio::model::AudioProfile,
    ) -> HashSet<String> {
        let targets = profile_enable_targets(state);
        let target_ids: Vec<String> = targets
            .iter()
            .filter(|(id, _)| self.device_state(id) != Some(DEVICE_STATE_ACTIVE))
            .map(|(id, _)| id.clone())
            .collect();
        let enabled_ids: HashSet<String> = if target_ids.is_empty() {
            HashSet::new()
        } else {
            self.suppress_auto_enforce();
            let enabled = self.enable_devices_batch(&target_ids);
            if state.auto_disable_enhancements {
                for id in &enabled {
                    if let Some((_, flow)) = targets.iter().find(|(tid, _)| tid == id) {
                        self.disable_device_enhancements(id, *flow, false);
                    }
                }
            }
            enabled.into_iter().collect()
        };

        self.enforce_locked_roles(state);

        for setting in &profile.device_settings {
            let Some(device) = state.device(&setting.device_id) else {
                continue;
            };
            let active = self.device_state(&device.id) == Some(DEVICE_STATE_ACTIVE);
            if active {
                self.sync_endpoint_audio(&device.id, device.volume, device.muted);
            }

            if state.auto_disable_enhancements || !device.enhancements.any_enabled() {
                if !enabled_ids.contains(&device.id) || !state.auto_disable_enhancements {
                    self.disable_device_enhancements(&device.id, device.flow, !active);
                }
            } else {
                if device.enhancements.sys_fx_enabled || device.enhancements.advanced_enabled {
                    self.enable_device_sysfx(&device.id);
                } else {
                    self.disable_device_sysfx(&device.id);
                }
                if active && device.flow == DeviceFlow::Output {
                    if device.enhancements.spatial_sound_enabled {
                        if !device.disabled
                            && let Some(fmt) = device.enhancements.previous_spatial_format.as_deref()
                            && spatial::set_spatial_format(&device.id, fmt)
                        {
                            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
                            if guard.clear_previous_spatial_format(&device.id) {
                                self.save_due = None;
                                let _ = store::save_state(&guard);
                            }
                        }
                    } else {
                        self.disable_device_spatial(&device.id);
                    }
                }
            }

            if device.flow == DeviceFlow::Input
                && let Some(listen) = device.listen.as_ref()
            {
                self.apply_listen_config(&device.id, listen, state);
            }
        }

        self.resync_comms_listen_targets(state);

        if state.auto_disable_unused_devices {
            self.enforce_unused_devices(state).disabled_ids
        } else {
            HashSet::new()
        }
    }

    fn sync_endpoint_audio(&self, device_id: &str, volume: u8, muted: bool) {
        let activated;
        let endpoint = if let Some(sub) = self.volumes.get(device_id) {
            &sub.endpoint
        } else if let Some(vol) = self.activate_endpoint_volume(device_id) {
            activated = vol;
            &activated
        } else {
            return;
        };

        let target_volume = volume.min(100);
        let current_volume = unsafe { endpoint.GetMasterVolumeLevelScalar().ok() }
            .map(|s| (s.clamp(0.0, 1.0) * 100.0).round() as u8);
        if current_volume != Some(target_volume) {
            let scalar = (target_volume as f32) / 100.0;
            unsafe {
                let _ = endpoint.SetMasterVolumeLevelScalar(scalar, &APP_AUDIO_CONTEXT);
            }
        }

        let current_muted = unsafe { endpoint.GetMute().ok().map(|b| b.as_bool()) };
        if current_muted != Some(muted) {
            unsafe {
                let _ = endpoint.SetMute(muted, &APP_AUDIO_CONTEXT);
            }
        }
    }

    fn set_endpoint_volume(&self, device_id: &str, volume: u8) {
        let scalar = (volume.min(100) as f32) / 100.0;
        if let Some(sub) = self.volumes.get(device_id) {
            unsafe {
                let _ = sub
                    .endpoint
                    .SetMasterVolumeLevelScalar(scalar, &APP_AUDIO_CONTEXT);
            }
            return;
        }
        if let Some(vol) = self.activate_endpoint_volume(device_id) {
            unsafe {
                let _ = vol.SetMasterVolumeLevelScalar(scalar, &APP_AUDIO_CONTEXT);
            }
        }
    }

    fn set_endpoint_mute(&self, device_id: &str, muted: bool) {
        if let Some(sub) = self.volumes.get(device_id) {
            unsafe {
                let _ = sub.endpoint.SetMute(muted, &APP_AUDIO_CONTEXT);
            }
            return;
        }
        if let Some(vol) = self.activate_endpoint_volume(device_id) {
            unsafe {
                let _ = vol.SetMute(muted, &APP_AUDIO_CONTEXT);
            }
        }
    }

    fn activate_endpoint_volume(&self, device_id: &str) -> Option<IAudioEndpointVolume> {
        let wide = win32::to_wide(device_id);
        let device = unsafe { self.enumerator.GetDevice(PCWSTR(wide.as_ptr())).ok()? };
        unsafe { device.Activate(CLSCTX_ALL, None).ok() }
    }

    fn disable_device_sysfx(&self, device_id: &str) {
        if self.read_sysfx_value(device_id) != Some(ENDPOINT_SYSFX_DISABLED) {
            let _ = self.policy.set_property_value(
                device_id,
                true,
                &PKEY_AudioEndpoint_Disable_SysFx,
                &PROPVARIANT::from(ENDPOINT_SYSFX_DISABLED),
            );
        }
    }

    fn disable_device_spatial(&mut self, device_id: &str) -> bool {
        let Some(prev_format) = spatial::disable_spatial(device_id) else {
            return false;
        };
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if guard.record_previous_spatial_format(device_id, prev_format) {
            self.save_due = None;
            let _ = store::save_state(&guard);
            return true;
        }
        false
    }

    fn disable_device_enhancements(
        &mut self,
        device_id: &str,
        flow: DeviceFlow,
        disabled: bool,
    ) -> bool {
        self.disable_device_sysfx(device_id);
        if !disabled && flow == DeviceFlow::Output {
            self.disable_device_spatial(device_id)
        } else {
            false
        }
    }

    fn enable_device_sysfx(&self, device_id: &str) {
        if self.read_sysfx_value(device_id) == Some(ENDPOINT_SYSFX_DISABLED) {
            let _ = self.policy.set_property_value(
                device_id,
                true,
                &PKEY_AudioEndpoint_Disable_SysFx,
                &PROPVARIANT::from(ENDPOINT_SYSFX_ENABLED),
            );
        }
    }

    fn disable_all_active_enhancements(&mut self, state: &AppState) -> bool {
        let mut recorded_any = false;
        for dev in &state.devices {
            if self.disable_device_enhancements(&dev.id, dev.flow, dev.disabled) {
                recorded_any = true;
            }
        }
        recorded_any
    }

    fn apply_listen_config(&self, device_id: &str, listen: &ListenConfig, state: &AppState) {
        let (active, desired_target) = desired_effective_listen(listen, state);
        let live = self.read_listen(device_id);
        if live.enabled == active && live.target_device_id.as_deref() == desired_target {
            return;
        }

        let target_pv = match desired_target {
            Some(LISTEN_TARGET_DEFAULT_OUTPUT) | None => PROPVARIANT::default(),
            Some(target_id) => propvariant_lpwstr(target_id),
        };

        if active {
            let _ =
                self.policy
                    .set_property_value(device_id, false, &PKEY_LISTEN_DEVICE, &target_pv);
            let _ = self.policy.set_property_value(
                device_id,
                false,
                &PKEY_LISTEN_ENABLED,
                &PROPVARIANT::from(true),
            );
        } else {
            let _ = self.policy.set_property_value(
                device_id,
                false,
                &PKEY_LISTEN_ENABLED,
                &PROPVARIANT::from(false),
            );
            let _ =
                self.policy
                    .set_property_value(device_id, false, &PKEY_LISTEN_DEVICE, &target_pv);
        }
    }

    fn resync_comms_listen_targets(&self, state: &AppState) {
        for dev in &state.devices {
            if dev.flow == DeviceFlow::Input
                && let Some(listen) = dev.listen.as_ref()
                && listen.enabled
                && listen.target_device_id.as_deref() == Some(LISTEN_TARGET_COMMS_OUTPUT)
            {
                self.apply_listen_config(&dev.id, listen, state);
            }
        }
    }
}

fn saved_spatial_format(device: &AudioDevice) -> Option<&str> {
    device
        .enhancements
        .previous_spatial_format
        .as_deref()
        .or_else(|| {
            device
                .previous_enhancements
                .as_ref()
                .and_then(|prev| prev.previous_spatial_format.as_deref())
        })
}

fn earliest_deadline<const N: usize>(deadlines: [Option<Instant>; N]) -> Option<Instant> {
    deadlines.into_iter().flatten().min()
}

fn read_endpoint_volume(endpoint: &IAudioEndpointVolume) -> Option<(u8, bool)> {
    let scalar = unsafe { endpoint.GetMasterVolumeLevelScalar().ok()? };
    let muted = unsafe { endpoint.GetMute().ok()?.as_bool() };
    let vol_pct = (scalar.clamp(0.0, 1.0) * 100.0).round() as u8;
    Some((vol_pct, muted))
}

fn reconcile_live_listen(
    existing: Option<&ListenConfig>,
    mut live: ListenConfig,
    comms_output_id: &str,
) -> ListenConfig {
    if let Some(existing_listen) = existing {
        if live.enabled {
            if existing_listen.target_device_id.as_deref() == Some(LISTEN_TARGET_COMMS_OUTPUT)
                && !comms_output_id.is_empty()
                && live.target_device_id.as_deref() == Some(comms_output_id)
            {
                live.target_device_id = Some(LISTEN_TARGET_COMMS_OUTPUT.to_string());
            } else if existing_listen.target_device_id.as_deref()
                == Some(LISTEN_TARGET_DEFAULT_OUTPUT)
                && live.target_device_id.is_none()
            {
                live.target_device_id = Some(LISTEN_TARGET_DEFAULT_OUTPUT.to_string());
            }
        } else if matches!(
            existing_listen.target_device_id.as_deref(),
            Some(LISTEN_TARGET_DEFAULT_OUTPUT | LISTEN_TARGET_COMMS_OUTPUT)
        ) {
            live.target_device_id = existing_listen.target_device_id.clone();
        } else if existing_listen.enabled && existing_listen.target_device_id.is_none() {
            live.target_device_id = None;
        }
    }
    live
}

fn desired_effective_listen<'a>(
    listen: &'a ListenConfig,
    state: &'a AppState,
) -> (bool, Option<&'a str>) {
    let target_id = match listen.target_device_id.as_deref() {
        Some(LISTEN_TARGET_DEFAULT_OUTPUT) => None,
        Some(LISTEN_TARGET_COMMS_OUTPUT) => {
            if !state.roles.comms_output_id.is_empty()
                && state.has_device_with_flow(DeviceFlow::Output, &state.roles.comms_output_id)
            {
                Some(state.roles.comms_output_id.as_str())
            } else {
                None
            }
        }
        Some(target_id) if state.has_device_with_flow(DeviceFlow::Output, target_id) => {
            Some(target_id)
        }
        _ => return (false, None),
    };

    let active = listen.enabled;
    let normalized_target = match target_id {
        Some(id) => Some(id),
        None if active => Some(LISTEN_TARGET_DEFAULT_OUTPUT),
        None => None,
    };
    (active, normalized_target)
}

fn locked_role_ids(state: &AppState) -> Vec<&str> {
    let mut ids = Vec::with_capacity(4);
    let candidates = if state.separate_comms_role {
        [
            state.roles.default_output_id.as_str(),
            state.roles.comms_output_id.as_str(),
            state.roles.default_input_id.as_str(),
            state.roles.comms_input_id.as_str(),
        ]
    } else {
        [
            state.roles.default_output_id.as_str(),
            "",
            state.roles.default_input_id.as_str(),
            "",
        ]
    };
    for id in candidates {
        if !id.is_empty() && !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

fn expected_role_device(state: &AppState, flow: DeviceFlow, role: ERole) -> &str {
    match (flow, role) {
        (DeviceFlow::Output, r) if r == eCommunications && state.separate_comms_role => {
            &state.roles.comms_output_id
        }
        (DeviceFlow::Output, _) => &state.roles.default_output_id,
        (DeviceFlow::Input, r) if r == eCommunications && state.separate_comms_role => {
            &state.roles.comms_input_id
        }
        (DeviceFlow::Input, _) => &state.roles.default_input_id,
    }
}

fn flow_from_win32(flow: EDataFlow) -> Option<DeviceFlow> {
    if flow == eRender {
        Some(DeviceFlow::Output)
    } else if flow == eCapture {
        Some(DeviceFlow::Input)
    } else {
        None
    }
}

fn flow_to_win32(flow: DeviceFlow) -> EDataFlow {
    match flow {
        DeviceFlow::Output => eRender,
        DeviceFlow::Input => eCapture,
    }
}

fn is_watched_property(key: &PROPERTYKEY) -> bool {
    *key == PKEY_AudioEndpoint_Disable_SysFx
        || *key == PKEY_LISTEN_ENABLED
        || *key == PKEY_LISTEN_DEVICE
        || *key == PKEY_Device_DeviceDesc
        || *key == PKEY_DeviceInterface_FriendlyName
        || *key == PKEY_Device_FriendlyName
}

fn read_device_id(device: &IMMDevice) -> Option<String> {
    unsafe {
        let raw = device.GetId().ok()?;
        let id = raw.to_string().ok();
        CoTaskMemFree(Some(raw.0.cast()));
        id.filter(|s| !s.is_empty())
    }
}

fn read_device_names(device: &IMMDevice) -> (String, String) {
    let Ok(store) = (unsafe { device.OpenPropertyStore(STGM_READ) }) else {
        return ("Audio Device".to_string(), String::new());
    };

    let read_prop = |key: &PROPERTYKEY| -> Option<String> {
        let pv = unsafe { store.GetValue(key).ok()? };
        propvariant_to_string(&pv)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    let desc = read_prop(&PKEY_Device_DeviceDesc);
    let iface = read_prop(&PKEY_DeviceInterface_FriendlyName);
    let friendly = read_prop(&PKEY_Device_FriendlyName);

    let name = desc
        .or_else(|| friendly.clone())
        .or_else(|| iface.clone())
        .unwrap_or_else(|| "Audio Device".to_string());

    let subtitle = iface
        .filter(|s| s != &name)
        .or_else(|| friendly.filter(|s| s != &name))
        .unwrap_or_default();

    (name, subtitle)
}

fn flow_index(flow: DeviceFlow) -> usize {
    match flow {
        DeviceFlow::Output => 0,
        DeviceFlow::Input => 1,
    }
}

fn count_active_by_flow(devices: &[AudioDevice], active_ids: &HashSet<String>) -> [usize; 2] {
    let mut counts = [0usize; 2];
    for dev in devices {
        if active_ids.contains(&dev.id) {
            counts[flow_index(dev.flow)] += 1;
        }
    }
    counts
}

fn in_use_device_ids(state: &AppState) -> HashSet<String> {
    let mut planned = state.clone();
    planned.auto_disable_unused_devices = true;
    planned.sync_auto_disabled_flags();
    planned
        .devices
        .iter()
        .filter(|dev| planned.is_device_in_use(dev))
        .map(|dev| dev.id.clone())
        .collect()
}

fn resolve_listen_target_id<'a>(listen: &'a ListenConfig, state: &'a AppState) -> Option<&'a str> {
    if !listen.enabled {
        return None;
    }
    match listen.target_device_id.as_deref()? {
        LISTEN_TARGET_DEFAULT_OUTPUT => (!state.roles.default_output_id.is_empty()
            && state.has_device_with_flow(DeviceFlow::Output, &state.roles.default_output_id))
        .then_some(state.roles.default_output_id.as_str()),
        LISTEN_TARGET_COMMS_OUTPUT => (!state.roles.comms_output_id.is_empty()
            && state.has_device_with_flow(DeviceFlow::Output, &state.roles.comms_output_id))
        .then_some(state.roles.comms_output_id.as_str()),
        id => state
            .has_device_with_flow(DeviceFlow::Output, id)
            .then_some(id),
    }
}

fn profile_enable_targets(state: &AppState) -> Vec<(String, DeviceFlow)> {
    let mut targets = Vec::new();
    let mut seen = HashSet::new();

    for dev in &state.devices {
        if state.is_device_in_use(dev) && !dev.id.is_empty() && seen.insert(dev.id.clone()) {
            targets.push((dev.id.clone(), dev.flow));
        }
    }

    for dev in &state.devices {
        if dev.flow == DeviceFlow::Input
            && (seen.contains(&dev.id) || !dev.disabled)
            && let Some(listen) = dev.listen.as_ref()
            && let Some(target_id) = resolve_listen_target_id(listen, state)
            && !target_id.is_empty()
            && seen.insert(target_id.to_string())
        {
            targets.push((target_id.to_string(), DeviceFlow::Output));
        }
    }

    targets
}

