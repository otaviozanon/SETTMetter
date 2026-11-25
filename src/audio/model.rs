use serde::{Deserialize, Serialize};


pub const LISTEN_TARGET_DEFAULT_OUTPUT: &str = "__default_output__";
pub const LISTEN_TARGET_COMMS_OUTPUT: &str = "__comms_output__";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeviceFlow {
    Output,
    Input,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppAction {
    SetLockEnabled(bool),
    SetSeparateCommsRole(bool),
    SetAutoDisableEnhancements(bool),
    ReenableAllEnhancements,
    SetAutoDisableUnusedDevices(bool),
    SetDeviceDisabled {
        device_id: String,
        disabled: bool,
    },
    ReenableAllDevices,
    SetDefault {
        flow: DeviceFlow,
        device_id: String,
    },
    SetComms {
        flow: DeviceFlow,
        device_id: String,
    },
    SetVolume {
        device_id: String,
        volume: u8,
    },
    ToggleMute {
        device_id: String,
    },
    SetMute {
        device_id: String,
        muted: bool,
    },
    SetEnhancementsDisabled {
        device_id: String,
        disabled: bool,
    },
    ToggleDevicePin {
        device_id: String,
    },
    SetDevicePinned {
        device_id: String,
        pinned: bool,
    },
    SetListenEnabled {
        device_id: String,
        enabled: bool,
    },
    SetListenTarget {
        device_id: String,
        target_device_id: Option<String>,
    },
    ApplyProfile {
        profile_id: String,
    },
    SaveProfile {
        name: String,
    },
    DeleteProfile {
        profile_id: String,
    },
    /// Drag-and-drop order of one flow's device cards, top to bottom.
    ReorderDevices {
        flow: DeviceFlow,
        device_ids: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EnhancementsState {
    pub sys_fx_enabled: bool,
    pub advanced_enabled: bool,
    pub spatial_sound_enabled: bool,
    #[serde(default)]
    pub previous_spatial_format: Option<String>,
}

impl EnhancementsState {
    pub fn any_enabled(&self) -> bool {
        self.sys_fx_enabled || self.advanced_enabled || self.spatial_sound_enabled
    }

    pub fn disable_all(&mut self) {
        self.sys_fx_enabled = false;
        self.advanced_enabled = false;
        self.spatial_sound_enabled = false;
    }

    pub fn all_disabled() -> Self {
        Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ListenConfig {
    pub enabled: bool,
    pub target_device_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
    pub subtitle: String,
    pub flow: DeviceFlow,
    pub volume: u8,
    pub muted: bool,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub disabled: bool,
    pub enhancements: EnhancementsState,
    #[serde(default)]
    pub previous_enhancements: Option<EnhancementsState>,
    pub listen: Option<ListenConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceSnapshot {
    pub device_id: String,
    pub volume: u8,
    pub muted: bool,
    pub enhancements: EnhancementsState,
    pub listen: Option<ListenConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RoleAssignments {
    pub default_output_id: String,
    pub comms_output_id: String,
    pub default_input_id: String,
    pub comms_input_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioProfile {
    pub id: String,
    pub name: String,
    pub roles: RoleAssignments,
    pub device_settings: Vec<DeviceSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppState {
    pub lock_enabled: bool,
    #[serde(default)]
    pub separate_comms_role: bool,
    #[serde(default)]
    pub auto_disable_enhancements: bool,
    #[serde(default)]
    pub auto_disable_unused_devices: bool,
    pub active_profile_id: Option<String>,
    pub roles: RoleAssignments,
    pub devices: Vec<AudioDevice>,
    /// Device ids in the order the user dragged them; enumeration is re-sorted to match.
    #[serde(default)]
    pub device_order: Vec<String>,
    pub profiles: Vec<AudioProfile>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            lock_enabled: true,
            separate_comms_role: false,
            auto_disable_enhancements: false,
            auto_disable_unused_devices: false,
            active_profile_id: None,
            roles: RoleAssignments::default(),
            devices: Vec::new(),
            device_order: Vec::new(),
            profiles: Vec::new(),
        }
    }
}

impl AppState {
    pub fn apply_action(&mut self, action: &AppAction) {
        match action {
            AppAction::SetLockEnabled(enabled) => self.lock_enabled = *enabled,
            AppAction::SetSeparateCommsRole(enabled) => {
                self.separate_comms_role = *enabled;
                if !*enabled {
                    self.unify_comms_roles();
                    self.sync_auto_disabled_flags();
                }
            }
            AppAction::SetAutoDisableEnhancements(enabled) => {
                self.set_auto_disable_enhancements(*enabled);
            }
            AppAction::ReenableAllEnhancements => self.reenable_all_enhancements(),
            AppAction::SetAutoDisableUnusedDevices(enabled) => {
                self.set_auto_disable_unused_devices(*enabled);
            }
            AppAction::SetDeviceDisabled {
                device_id,
                disabled,
            } => {
                if !self.auto_disable_unused_devices {
                    self.set_device_disabled(device_id, *disabled);
                }
            }
            AppAction::ReenableAllDevices => self.reenable_all_devices(),
            AppAction::SetDefault { flow, device_id } => self.set_default(*flow, device_id),
            AppAction::SetComms { flow, device_id } => self.set_comms(*flow, device_id),
            AppAction::SetVolume { device_id, volume } => self.set_volume(device_id, *volume),
            AppAction::ToggleMute { device_id } => self.toggle_mute(device_id),
            AppAction::SetMute { device_id, muted } => self.set_mute(device_id, *muted),
            AppAction::SetEnhancementsDisabled {
                device_id,
                disabled,
            } => self.set_enhancements_disabled(device_id, *disabled),
            AppAction::ToggleDevicePin { device_id } => self.toggle_device_pin(device_id),
            AppAction::SetDevicePinned { device_id, pinned } => {
                self.set_device_pinned(device_id, *pinned);
            }
            AppAction::ReorderDevices { flow, device_ids } => {
                self.set_device_order(*flow, device_ids);
            }
            AppAction::SetListenEnabled { device_id, enabled } => {
                self.set_listen_enabled(device_id, *enabled);
            }
            AppAction::SetListenTarget {
                device_id,
                target_device_id,
            } => self.set_listen_target(device_id, target_device_id.clone()),
            AppAction::ApplyProfile { profile_id } => {
                self.apply_profile(profile_id);
            }
            AppAction::SaveProfile { name } => {
                self.save_current_as_profile(name);
            }
            AppAction::DeleteProfile { profile_id } => {
                self.delete_profile(profile_id);
            }
        }
    }

    pub fn reconcile_live_devices(
        &mut self,
        mut live_devices: Vec<AudioDevice>,
        live_roles: RoleAssignments,
    ) {
        for device in &mut live_devices {
            if let Some(existing) = self.device(&device.id) {
                device.pinned = existing.pinned;
                device.previous_enhancements = existing.previous_enhancements.clone();
                if device.enhancements.previous_spatial_format.is_none() {
                    device.enhancements.previous_spatial_format =
                        existing.enhancements.previous_spatial_format.clone();
                }
                if device.disabled {
                    device.volume = existing.volume;
                    device.muted = existing.muted;
                }
                if let (Some(existing_listen), Some(live_listen)) =
                    (existing.listen.as_ref(), device.listen.as_mut())
                {
                    if live_listen.enabled {
                        if existing_listen.target_device_id.as_deref()
                            == Some(LISTEN_TARGET_COMMS_OUTPUT)
                            && !self.roles.comms_output_id.is_empty()
                            && live_listen.target_device_id.as_deref()
                                == Some(self.roles.comms_output_id.as_str())
                        {
                            live_listen.target_device_id =
                                Some(LISTEN_TARGET_COMMS_OUTPUT.to_string());
                        }
                    } else if matches!(
                        existing_listen.target_device_id.as_deref(),
                        Some(LISTEN_TARGET_DEFAULT_OUTPUT | LISTEN_TARGET_COMMS_OUTPUT)
                    ) {
                        live_listen.target_device_id = existing_listen.target_device_id.clone();
                    } else if existing_listen.enabled && existing_listen.target_device_id.is_none()
                    {
                        live_listen.target_device_id = None;
                    }
                }
            }
        }

        self.devices = live_devices;
        self.apply_device_order();
        let has_real_devices = self.devices.iter().any(|d| d.id.starts_with('{'));

        self.roles = RoleAssignments {
            default_output_id: self.reconcile_role(
                DeviceFlow::Output,
                &self.roles.default_output_id,
                &live_roles.default_output_id,
                has_real_devices,
            ),
            comms_output_id: self.reconcile_role(
                DeviceFlow::Output,
                &self.roles.comms_output_id,
                &live_roles.comms_output_id,
                has_real_devices,
            ),
            default_input_id: self.reconcile_role(
                DeviceFlow::Input,
                &self.roles.default_input_id,
                &live_roles.default_input_id,
                has_real_devices,
            ),
            comms_input_id: self.reconcile_role(
                DeviceFlow::Input,
                &self.roles.comms_input_id,
                &live_roles.comms_input_id,
                has_real_devices,
            ),
        };
        if !self.separate_comms_role {
            self.unify_comms_roles();
        }

        if has_real_devices
            && !self.profiles.is_empty()
            && self.profiles.iter().all(is_mock_only_profile)
        {
            self.profiles.clear();
            self.active_profile_id = None;
        }
    }

    pub fn set_device_audio_state(&mut self, device_id: &str, volume: u8, muted: bool) -> bool {
        let clamped = volume.min(100);
        let Some(device) = self.device_mut(device_id) else {
            return false;
        };
        if device.volume == clamped && device.muted == muted {
            return false;
        }
        device.volume = clamped;
        device.muted = muted;
        true
    }

    pub fn set_device_enhancements(
        &mut self,
        device_id: &str,
        mut enhancements: EnhancementsState,
    ) -> bool {
        let Some(device) = self.device_mut(device_id) else {
            return false;
        };
        if enhancements.previous_spatial_format.is_none() {
            enhancements.previous_spatial_format =
                device.enhancements.previous_spatial_format.clone();
        }
        if device.enhancements == enhancements {
            return false;
        }
        device.enhancements = enhancements;
        true
    }

    pub fn record_previous_spatial_format(&mut self, device_id: &str, format: String) -> bool {
        let Some(device) = self.device_mut(device_id) else {
            return false;
        };
        if device.enhancements.previous_spatial_format.is_some() {
            return false;
        }
        device.enhancements.previous_spatial_format = Some(format);
        true
    }

    pub fn clear_previous_spatial_format(&mut self, device_id: &str) -> bool {
        let Some(device) = self.device_mut(device_id) else {
            return false;
        };
        if device.enhancements.previous_spatial_format.is_none() {
            return false;
        }
        device.enhancements.previous_spatial_format = None;
        true
    }

    pub fn set_device_listen(&mut self, device_id: &str, listen: ListenConfig) -> bool {
        let Some(device) = self.device_mut(device_id) else {
            return false;
        };
        if device.flow != DeviceFlow::Input || device.listen.as_ref() == Some(&listen) {
            return false;
        }
        device.listen = Some(listen);
        true
    }

    pub fn device(&self, device_id: &str) -> Option<&AudioDevice> {
        self.devices.iter().find(|d| d.id == device_id)
    }

    pub fn has_device_with_flow(&self, flow: DeviceFlow, device_id: &str) -> bool {
        self.devices
            .iter()
            .any(|d| d.id == device_id && d.flow == flow)
    }

    pub fn set_default(&mut self, flow: DeviceFlow, device_id: &str) {
        if !self.has_device_with_flow(flow, device_id) {
            return;
        }
        match flow {
            DeviceFlow::Output => {
                self.roles.default_output_id = device_id.to_string();
                if !self.separate_comms_role {
                    self.roles.comms_output_id = device_id.to_string();
                }
            }
            DeviceFlow::Input => {
                self.roles.default_input_id = device_id.to_string();
                if !self.separate_comms_role {
                    self.roles.comms_input_id = device_id.to_string();
                }
            }
        }
        if let Some(device) = self.device_mut(device_id) {
            device.disabled = false;
        }
        if self.auto_disable_unused_devices {
            self.sync_auto_disabled_flags();
        }
    }

    pub fn set_comms(&mut self, flow: DeviceFlow, device_id: &str) {
        if !self.separate_comms_role || !self.has_device_with_flow(flow, device_id) {
            return;
        }
        match flow {
            DeviceFlow::Output => self.roles.comms_output_id = device_id.to_string(),
            DeviceFlow::Input => self.roles.comms_input_id = device_id.to_string(),
        }
        if let Some(device) = self.device_mut(device_id) {
            device.disabled = false;
        }
        if self.auto_disable_unused_devices {
            self.sync_auto_disabled_flags();
        }
    }

    pub fn set_volume(&mut self, device_id: &str, volume: u8) {
        if let Some(device) = self.device_mut(device_id) {
            device.volume = volume.min(100);
        }
    }

    pub fn toggle_mute(&mut self, device_id: &str) {
        if let Some(device) = self.device_mut(device_id) {
            device.muted = !device.muted;
        }
    }

    pub fn set_mute(&mut self, device_id: &str, muted: bool) {
        if let Some(device) = self.device_mut(device_id) {
            device.muted = muted;
        }
    }

    pub fn are_active_mics_muted(&self) -> bool {
        let mut active = self.devices.iter().filter(|d| {
            d.flow == DeviceFlow::Input
                && (d.id == self.roles.default_input_id || d.id == self.roles.comms_input_id)
        });
        let Some(first) = active.next() else {
            return false;
        };
        first.muted && active.all(|d| d.muted)
    }

    pub fn toggle_active_mics_mute(&mut self) -> bool {
        let target_muted = !self.are_active_mics_muted();
        for device in &mut self.devices {
            if device.flow == DeviceFlow::Input
                && (device.id == self.roles.default_input_id
                    || device.id == self.roles.comms_input_id)
            {
                device.muted = target_muted;
            }
        }
        target_muted
    }

    pub fn toggle_device_pin(&mut self, device_id: &str) {
        if let Some(device) = self.device_mut(device_id) {
            device.pinned = !device.pinned;
        }
        if self.auto_disable_unused_devices {
            self.sync_auto_disabled_flags();
        }
    }

    pub fn set_device_pinned(&mut self, device_id: &str, pinned: bool) {
        if let Some(device) = self.device_mut(device_id) {
            device.pinned = pinned;
        }
        if self.auto_disable_unused_devices {
            self.sync_auto_disabled_flags();
        }
    }

    pub fn is_device_in_use(&self, device: &AudioDevice) -> bool {
        if device.pinned || self.is_default(device) || self.is_comms(device) {
            return true;
        }
        if device.flow == DeviceFlow::Output {
            return self.devices.iter().any(|input| {
                input.flow == DeviceFlow::Input
                    && !input.disabled
                    && input.listen.as_ref().is_some_and(|listen| {
                        listen.enabled
                            && listen.target_device_id.as_deref() == Some(device.id.as_str())
                    })
            });
        }
        false
    }

    pub fn sync_auto_disabled_flags(&mut self) {
        if !self.auto_disable_unused_devices {
            return;
        }

        let active_listen_targets: Vec<String> = self
            .devices
            .iter()
            .filter(|device| {
                device.flow == DeviceFlow::Input
                    && (device.pinned || self.is_default(device) || self.is_comms(device))
            })
            .filter_map(|device| device.listen.as_ref())
            .filter(|listen| listen.enabled)
            .filter_map(|listen| listen.target_device_id.clone())
            .collect();

        let in_use_flags: Vec<bool> = self
            .devices
            .iter()
            .map(|device| {
                device.pinned
                    || self.is_default(device)
                    || self.is_comms(device)
                    || (device.flow == DeviceFlow::Output
                        && active_listen_targets.iter().any(|id| id == &device.id))
            })
            .collect();

        for (device, in_use) in self.devices.iter_mut().zip(in_use_flags) {
            device.disabled = !in_use;
        }
    }

    pub fn set_device_disabled(&mut self, device_id: &str, disabled: bool) -> bool {
        let Some(flow) = self.device(device_id).map(|d| d.flow) else {
            return false;
        };
        if disabled {
            let Some(replacement_id) = self
                .devices
                .iter()
                .find(|d| d.flow == flow && d.id != device_id && !d.disabled)
                .map(|d| d.id.clone())
            else {
                return false;
            };
            if let Some(device) = self.device_mut(device_id) {
                device.disabled = true;
            }
            match flow {
                DeviceFlow::Output => {
                    if self.roles.default_output_id == device_id {
                        self.roles.default_output_id = replacement_id.clone();
                    }
                    if self.roles.comms_output_id == device_id {
                        self.roles.comms_output_id = replacement_id;
                    }
                }
                DeviceFlow::Input => {
                    if self.roles.default_input_id == device_id {
                        self.roles.default_input_id = replacement_id.clone();
                    }
                    if self.roles.comms_input_id == device_id {
                        self.roles.comms_input_id = replacement_id;
                    }
                }
            }
        } else if let Some(device) = self.device_mut(device_id) {
            device.disabled = false;
        }
        true
    }

    pub fn set_auto_disable_unused_devices(&mut self, enabled: bool) {
        self.auto_disable_unused_devices = enabled;
        if enabled {
            self.sync_auto_disabled_flags();
        }
    }

    pub fn reenable_all_devices(&mut self) {
        for device in &mut self.devices {
            device.disabled = false;
        }
    }

    pub fn set_enhancements_disabled(&mut self, device_id: &str, disabled: bool) {
        if let Some(device) = self.device_mut(device_id) {
            if disabled {
                disable_enhancements_preserving_undo(device);
            } else {
                reenable_enhancements_from_undo(device);
            }
        }
    }

    pub fn set_auto_disable_enhancements(&mut self, enabled: bool) {
        self.auto_disable_enhancements = enabled;
        if enabled {
            for device in &mut self.devices {
                disable_enhancements_preserving_undo(device);
            }
        }
    }

    pub fn reenable_all_enhancements(&mut self) {
        for device in &mut self.devices {
            if device.disabled {
                continue;
            }
            reenable_enhancements_from_undo(device);
        }
    }

    pub fn are_enhancements_disabled(&self, device: &AudioDevice) -> bool {
        !device.enhancements.any_enabled()
    }

    pub fn set_listen_enabled(&mut self, device_id: &str, enabled: bool) {
        if let Some(device) = self.device_mut(device_id)
            && device.flow == DeviceFlow::Input
        {
            let listen = device.listen.get_or_insert_with(ListenConfig::default);
            listen.enabled = enabled;
        }
        if self.auto_disable_unused_devices {
            self.sync_auto_disabled_flags();
        }
    }

    pub fn set_listen_target(&mut self, device_id: &str, target_device_id: Option<String>) {
        if let Some(device) = self.device_mut(device_id)
            && device.flow == DeviceFlow::Input
        {
            let listen = device.listen.get_or_insert_with(ListenConfig::default);
            listen.target_device_id = target_device_id;
        }
        if self.auto_disable_unused_devices {
            self.sync_auto_disabled_flags();
        }
    }

    pub fn listen_target_label(&self, target_id: Option<&str>) -> String {
        match target_id {
            None => "None".to_string(),
            Some(LISTEN_TARGET_DEFAULT_OUTPUT) => "Default Playback Device".to_string(),
            Some(LISTEN_TARGET_COMMS_OUTPUT) => "Default Communications Device".to_string(),
            Some(id) => self
                .devices
                .iter()
                .find(|d| d.id == id)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| "None".to_string()),
        }
    }

    pub fn apply_profile(&mut self, profile_id: &str) -> bool {
        let Some(profile) = self.profiles.iter().find(|p| p.id == profile_id).cloned() else {
            return false;
        };

        self.roles = profile.roles;
        if !self.separate_comms_role {
            self.unify_comms_roles();
        }
        for role_id in [
            self.roles.default_output_id.clone(),
            self.roles.comms_output_id.clone(),
            self.roles.default_input_id.clone(),
            self.roles.comms_input_id.clone(),
        ] {
            if !role_id.is_empty()
                && let Some(device) = self.device_mut(&role_id)
            {
                device.disabled = false;
            }
        }
        for snapshot in profile.device_settings {
            if let Some(device) = self.device_mut(&snapshot.device_id) {
                device.volume = snapshot.volume.min(100);
                device.muted = snapshot.muted;
                let saved_spatial = device.enhancements.previous_spatial_format.clone();
                device.enhancements = snapshot.enhancements;
                if device.enhancements.previous_spatial_format.is_none() {
                    device.enhancements.previous_spatial_format = saved_spatial;
                }
                if device.flow == DeviceFlow::Input {
                    device.listen = Some(snapshot.listen.unwrap_or_default());
                } else {
                    device.listen = None;
                }
            }
        }
        self.active_profile_id = Some(profile.id);
        if self.auto_disable_unused_devices {
            self.sync_auto_disabled_flags();
        }
        true
    }

    pub fn save_current_as_profile(&mut self, name: &str) -> String {
        let trimmed = name.trim();
        let display_name = if trimmed.is_empty() {
            format!("Profile {}", self.profiles.len() + 1)
        } else {
            trimmed.to_string()
        };

        let snapshots = self.capture_snapshots();

        if let Some(existing) = self
            .profiles
            .iter_mut()
            .find(|p| p.name.eq_ignore_ascii_case(&display_name))
        {
            existing.name = display_name;
            existing.roles = self.roles.clone();
            existing.device_settings = snapshots;
            let id = existing.id.clone();
            self.active_profile_id = Some(id.clone());
            return id;
        }

        let id = self.unique_profile_id(&display_name);
        self.profiles.push(AudioProfile {
            id: id.clone(),
            name: display_name,
            roles: self.roles.clone(),
            device_settings: snapshots,
        });
        self.active_profile_id = Some(id.clone());
        id
    }

    pub fn delete_profile(&mut self, profile_id: &str) -> bool {
        let before = self.profiles.len();
        self.profiles.retain(|p| p.id != profile_id);
        let removed = self.profiles.len() < before;
        if removed && self.active_profile_id.as_deref() == Some(profile_id) {
            self.active_profile_id = None;
        }
        removed
    }

    pub fn devices_for(&self, flow: DeviceFlow) -> impl Iterator<Item = &AudioDevice> {
        self.devices.iter().filter(move |d| d.flow == flow)
    }

    pub fn filtered_devices(&self, flow: DeviceFlow, query: &str) -> Vec<&AudioDevice> {
        let needle = query.trim().to_ascii_lowercase();
        let mut devices: Vec<&AudioDevice> = self
            .devices_for(flow)
            .filter(|device| {
                needle.is_empty()
                    || device.name.to_ascii_lowercase().contains(&needle)
                    || device.subtitle.to_ascii_lowercase().contains(&needle)
            })
            .collect();
        // Pinned devices float to the top; the stable sort keeps the manual (drag) order inside
        // each block, because `self.devices` is already sorted by `device_order`.
        devices.sort_by_key(|device| !device.pinned);
        devices
    }

    /// Applies the user's drag-and-drop order. Devices missing from `device_order` (new or not
    /// dragged yet) keep their enumeration order after the ordered ones.
    pub fn apply_device_order(&mut self) {
        let order = &self.device_order;
        self.devices.sort_by_key(|d| {
            order
                .iter()
                .position(|id| id == &d.id)
                .unwrap_or(usize::MAX)
        });
    }

    /// Replaces one flow's order with `ids` (top to bottom), leaving the other flow untouched.
    pub fn set_device_order(&mut self, flow: DeviceFlow, ids: &[String]) {
        let flow_ids: Vec<String> = self.devices_for(flow).map(|d| d.id.clone()).collect();
        if flow_ids.is_empty() {
            return;
        }
        let mut wanted: Vec<String> = ids
            .iter()
            .filter(|id| flow_ids.contains(id))
            .cloned()
            .collect();
        for id in &flow_ids {
            if !wanted.contains(id) {
                wanted.push(id.clone());
            }
        }
        let insert_at = self
            .device_order
            .iter()
            .position(|id| flow_ids.contains(id))
            .unwrap_or(self.device_order.len());
        self.device_order.retain(|id| !flow_ids.contains(id));
        for (offset, id) in wanted.into_iter().enumerate() {
            let at = (insert_at + offset).min(self.device_order.len());
            self.device_order.insert(at, id);
        }
        self.apply_device_order();
    }

    pub fn is_default(&self, device: &AudioDevice) -> bool {
        match device.flow {
            DeviceFlow::Output => self.roles.default_output_id == device.id,
            DeviceFlow::Input => self.roles.default_input_id == device.id,
        }
    }

    pub fn is_comms(&self, device: &AudioDevice) -> bool {
        match device.flow {
            DeviceFlow::Output => self.roles.comms_output_id == device.id,
            DeviceFlow::Input => self.roles.comms_input_id == device.id,
        }
    }

    pub fn active_profile(&self) -> Option<&AudioProfile> {
        let id = self.active_profile_id.as_deref()?;
        self.profiles.iter().find(|p| p.id == id)
    }

    fn unify_comms_roles(&mut self) {
        self.roles.comms_output_id = self.roles.default_output_id.clone();
        self.roles.comms_input_id = self.roles.default_input_id.clone();
    }

    fn reconcile_role(
        &self,
        flow: DeviceFlow,
        stored_id: &str,
        live_id: &str,
        has_real_devices: bool,
    ) -> String {
        if self.lock_enabled && !stored_id.is_empty() {
            let is_legacy_mock = !stored_id.starts_with('{') && has_real_devices;
            if !is_legacy_mock {
                match self
                    .devices
                    .iter()
                    .find(|d| d.id == stored_id && d.flow == flow)
                {
                    Some(device) if !device.disabled => return stored_id.to_string(),
                    Some(_) => {}
                    None if stored_id.starts_with('{') => return stored_id.to_string(),
                    None => {}
                }
            }
        }
        if !live_id.is_empty() {
            return live_id.to_string();
        }
        self.devices_for(flow)
            .find(|d| !d.disabled)
            .or_else(|| self.devices_for(flow).next())
            .map(|d| d.id.clone())
            .unwrap_or_default()
    }

    fn device_mut(&mut self, device_id: &str) -> Option<&mut AudioDevice> {
        self.devices.iter_mut().find(|d| d.id == device_id)
    }

    fn capture_snapshots(&self) -> Vec<DeviceSnapshot> {
        self.devices
            .iter()
            .map(|d| DeviceSnapshot {
                device_id: d.id.clone(),
                volume: d.volume,
                muted: d.muted,
                enhancements: d.enhancements.clone(),
                listen: d.listen.clone(),
            })
            .collect()
    }

    fn unique_profile_id(&self, name: &str) -> String {
        let mut base: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        while base.contains("--") {
            base = base.replace("--", "-");
        }
        let base = base.trim_matches('-');
        let base = if base.is_empty() { "profile" } else { base };

        if !self.profiles.iter().any(|p| p.id == base) {
            return base.to_string();
        }
        let mut suffix = 2usize;
        loop {
            let candidate = format!("{base}-{suffix}");
            if !self.profiles.iter().any(|p| p.id == candidate) {
                return candidate;
            }
            suffix += 1;
        }
    }
}

fn disable_enhancements_preserving_undo(device: &mut AudioDevice) {
    if device.enhancements.any_enabled() {
        device.previous_enhancements = Some(device.enhancements.clone());
        device.enhancements.disable_all();
    }
}

fn reenable_enhancements_from_undo(device: &mut AudioDevice) {
    if !device.enhancements.any_enabled() {
        let had_spatial_format = device.enhancements.previous_spatial_format.is_some();
        let mut restored = device
            .previous_enhancements
            .take()
            .filter(|prev| prev.any_enabled())
            .unwrap_or(EnhancementsState {
                sys_fx_enabled: true,
                advanced_enabled: true,
                spatial_sound_enabled: false,
                previous_spatial_format: None,
            });
        if had_spatial_format {
            restored.spatial_sound_enabled = true;
        }
        restored.previous_spatial_format = None;
        device.enhancements = restored;
    }
}

fn is_mock_device_id(id: &str) -> bool {
    !id.starts_with('{') && (id.starts_with("out-") || id.starts_with("in-"))
}

fn is_mock_only_profile(profile: &AudioProfile) -> bool {
    let role_ids = [
        profile.roles.default_output_id.as_str(),
        profile.roles.comms_output_id.as_str(),
        profile.roles.default_input_id.as_str(),
        profile.roles.comms_input_id.as_str(),
    ];
    let all_ids: Vec<&str> = role_ids
        .into_iter()
        .chain(profile.device_settings.iter().map(|s| s.device_id.as_str()))
        .filter(|id| !id.is_empty())
        .collect();
    !all_ids.is_empty() && all_ids.into_iter().all(is_mock_device_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, flow: DeviceFlow) -> AudioDevice {
        AudioDevice {
            id: id.to_string(),
            name: id.to_string(),
            subtitle: String::new(),
            flow,
            volume: 100,
            muted: false,
            pinned: false,
            disabled: false,
            enhancements: EnhancementsState::default(),
            previous_enhancements: None,
            listen: None,
        }
    }

    fn ids(state: &AppState) -> Vec<&str> {
        state.devices.iter().map(|d| d.id.as_str()).collect()
    }

    #[test]
    fn manual_order_survives_reenumeration() {
        let mut state = AppState {
            devices: vec![
                device("out1", DeviceFlow::Output),
                device("out2", DeviceFlow::Output),
                device("in1", DeviceFlow::Input),
            ],
            ..AppState::default()
        };
        state.set_device_order(DeviceFlow::Output, &["out2".to_string(), "out1".to_string()]);
        assert_eq!(ids(&state), ["out2", "out1", "in1"]);

        // The engine hands back freshly enumerated devices on every refresh, in its own order.
        state.reconcile_live_devices(
            vec![
                device("out1", DeviceFlow::Output),
                device("out2", DeviceFlow::Output),
                device("in1", DeviceFlow::Input),
            ],
            RoleAssignments::default(),
        );
        assert_eq!(ids(&state), ["out2", "out1", "in1"]);
    }

    #[test]
    fn reordering_one_flow_leaves_the_other_alone() {
        let mut state = AppState {
            devices: vec![
                device("out1", DeviceFlow::Output),
                device("in1", DeviceFlow::Input),
                device("in2", DeviceFlow::Input),
            ],
            ..AppState::default()
        };
        state.set_device_order(DeviceFlow::Input, &["in2".to_string(), "in1".to_string()]);
        assert_eq!(ids(&state), ["out1", "in2", "in1"]);
    }

    #[test]
    fn unknown_ids_are_ignored() {
        let mut state = AppState {
            devices: vec![device("out1", DeviceFlow::Output)],
            ..AppState::default()
        };
        state.set_device_order(
            DeviceFlow::Output,
            &["ghost".to_string(), "out1".to_string()],
        );
        assert_eq!(ids(&state), ["out1"]);
        assert_eq!(state.device_order, vec!["out1".to_string()]);
    }

    #[test]
    fn pinned_cards_float_without_losing_the_drag_order() {
        let mut state = AppState {
            devices: vec![
                device("a", DeviceFlow::Output),
                device("b", DeviceFlow::Output),
                device("c", DeviceFlow::Output),
            ],
            ..AppState::default()
        };
        let shown = |state: &AppState| -> Vec<&'static str> {
            state
                .filtered_devices(DeviceFlow::Output, "")
                .into_iter()
                .map(|d| match d.id.as_str() {
                    "a" => "a",
                    "b" => "b",
                    _ => "c",
                })
                .collect()
        };
        assert_eq!(shown(&state), ["a", "b", "c"]);

        // Pinning floats the card up, the other two keep their order.
        state.set_device_pinned("c", true);
        assert_eq!(shown(&state), ["c", "a", "b"]);

        // The list the drag sees is what gets stored, so the pin block stays on top.
        state.set_device_order(
            DeviceFlow::Output,
            &["c".to_string(), "b".to_string(), "a".to_string()],
        );
        assert_eq!(shown(&state), ["c", "b", "a"]);

        // Unpinning leaves the card where the user dragged it.
        state.set_device_pinned("c", false);
        assert_eq!(shown(&state), ["c", "b", "a"]);

        // And pinning a different card floats it too, keeping the rest as dragged.
        state.set_device_pinned("b", true);
        assert_eq!(shown(&state), ["b", "c", "a"]);
    }
}