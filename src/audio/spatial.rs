use std::thread;
use std::time::{Duration, Instant};

use winaudio::Media::Audio::{
    SetDefaultSpatialAudioFormatStatus, SpatialAudioDeviceConfiguration,
};
use winaudio::core::HSTRING;

const NONE_SPATIAL_FORMAT: &str = "{00000000-0000-0000-0000-000000000000}";
const SPATIAL_OP_TIMEOUT: Duration = Duration::from_secs(2);
const SPATIAL_POLL_INTERVAL: Duration = Duration::from_millis(10);

// Windows.Foundation.AsyncStatus values (avoids adding a direct windows-future dependency).
const ASYNC_STATUS_STARTED: i32 = 0;
const ASYNC_STATUS_COMPLETED: i32 = 1;

fn swd_device_path(mmdevice_id: &str) -> HSTRING {
    HSTRING::from(format!(
        r"\\?\SWD#MMDEVAPI#{}#{{e6327cad-dcec-4949-ae8a-991e976a79d2}}",
        mmdevice_id
    ))
}

fn is_active_format(format: &HSTRING) -> bool {
    !format.is_empty() && !format.to_string_lossy().eq_ignore_ascii_case(NONE_SPATIAL_FORMAT)
}

fn active_spatial_format_from_config(config: &SpatialAudioDeviceConfiguration) -> Option<String> {
    if let Ok(active) = config.ActiveSpatialAudioFormat()
        && is_active_format(&active)
    {
        return Some(active.to_string_lossy());
    }

    config
        .DefaultSpatialAudioFormat()
        .ok()
        .filter(is_active_format)
        .map(|default_fmt| default_fmt.to_string_lossy())
}

fn wait_until_completed<FStatus, FCancel>(
    mut poll_status: FStatus,
    cancel: FCancel,
    timeout: Duration,
    poll_interval: Duration,
) -> bool
where
    FStatus: FnMut() -> Option<i32>,
    FCancel: FnOnce(),
{
    let deadline = Instant::now() + timeout;
    loop {
        match poll_status() {
            Some(ASYNC_STATUS_COMPLETED) => return true,
            Some(ASYNC_STATUS_STARTED) => {
                let now = Instant::now();
                if now >= deadline {
                    cancel();
                    return false;
                }
                thread::sleep(poll_interval.min(deadline - now));
            }
            _ => return false,
        }
    }
}

fn set_spatial_format_on_config(
    config: &SpatialAudioDeviceConfiguration,
    format_guid: &str,
) -> bool {
    let target = HSTRING::from(format_guid);
    let Ok(op) = config.SetDefaultSpatialAudioFormatAsync(&target) else {
        return false;
    };

    let completed = wait_until_completed(
        || op.Status().ok().map(|status| status.0),
        || {
            let _ = op.Cancel();
        },
        SPATIAL_OP_TIMEOUT,
        SPATIAL_POLL_INTERVAL,
    );
    if !completed {
        return false;
    }

    op.GetResults()
        .and_then(|res| res.Status())
        .is_ok_and(|status| status == SetDefaultSpatialAudioFormatStatus::Succeeded)
}

pub fn active_spatial_format(mmdevice_id: &str) -> Option<String> {
    let Ok(config) = SpatialAudioDeviceConfiguration::GetForDeviceId(&swd_device_path(mmdevice_id))
    else {
        return None;
    };
    if !config.IsSpatialAudioSupported().unwrap_or(false) {
        return None;
    }

    active_spatial_format_from_config(&config)
}

pub fn is_spatial_enabled(mmdevice_id: &str) -> bool {
    active_spatial_format(mmdevice_id).is_some()
}

pub fn set_spatial_format(mmdevice_id: &str, format_guid: &str) -> bool {
    let Ok(config) = SpatialAudioDeviceConfiguration::GetForDeviceId(&swd_device_path(mmdevice_id))
    else {
        return false;
    };
    if !config.IsSpatialAudioSupported().unwrap_or(false) {
        return false;
    }

    set_spatial_format_on_config(&config, format_guid)
}

pub fn disable_spatial(mmdevice_id: &str) -> Option<String> {
    let Ok(config) = SpatialAudioDeviceConfiguration::GetForDeviceId(&swd_device_path(mmdevice_id))
    else {
        return None;
    };
    if !config.IsSpatialAudioSupported().unwrap_or(false) {
        return None;
    }

    let prev = active_spatial_format_from_config(&config)?;
    set_spatial_format_on_config(&config, NONE_SPATIAL_FORMAT).then_some(prev)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_until_completed_succeeds_immediately_and_after_polls() {
        let mut canceled = false;
        assert!(wait_until_completed(
            || Some(ASYNC_STATUS_COMPLETED),
            || canceled = true,
            Duration::from_millis(50),
            Duration::from_millis(5),
        ));
        assert!(!canceled);

        let mut polls = 0usize;
        let mut canceled = false;
        assert!(wait_until_completed(
            || {
                polls += 1;
                if polls >= 3 {
                    Some(ASYNC_STATUS_COMPLETED)
                } else {
                    Some(ASYNC_STATUS_STARTED)
                }
            },
            || canceled = true,
            Duration::from_millis(200),
            Duration::from_millis(5),
        ));
        assert_eq!(polls, 3);
        assert!(!canceled);
    }

    #[test]
    fn wait_until_completed_cancels_on_timeout_and_stops_on_error() {
        let mut canceled = false;
        assert!(!wait_until_completed(
            || Some(ASYNC_STATUS_STARTED),
            || canceled = true,
            Duration::from_millis(25),
            Duration::from_millis(5),
        ));
        assert!(canceled);

        let mut canceled = false;
        assert!(!wait_until_completed(
            || Some(3),
            || canceled = true,
            Duration::from_millis(200),
            Duration::from_millis(5),
        ));
        assert!(!canceled);
    }
}
