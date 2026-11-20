# SETTMetter

Wireless device battery levels and system monitoring (CPU/RAM/GPU/VRAM/temperatures) on the Windows 11 taskbar.

One native executable: no installer, no runtime, no service, no network access.

## Features

- **Devices** — Logitech receivers and dongles (HID++), Bluetooth LE and classic Bluetooth, Xbox and PlayStation controllers, vendor dongles for Corsair, Razer, SteelSeries, Delux, MCHOSE and Compx/Areson (Redragon, Pulsar, ARDOR, Fallen), generic USB HID, plus optional [HeadsetControl](https://github.com/Sapd/HeadsetControl) headsets.
- **Sensors** — CPU, RAM, GPU and VRAM usage plus CPU/GPU/SSD temperatures, each with its own checkbox, including per-adapter toggles on multi-GPU machines.
- **Presets** — one-click sensor and icon combinations.
- **Alerts** — CPU/GPU temperature thresholds with hysteresis, and a notification when a device drops to 15%.
- **History graph** — a popup plotting the last ~5 minutes of CPU/RAM/GPU usage.
- **Native look** — follows the light/dark taskbar theme, scales with DPI, uses Windows' own Fluent icons.
- **Fallback tray icon** — keeps the settings menu reachable when every device and sensor is hidden.
- **Private** — everything stays on your PC. No telemetry, no accounts.

## Install

Download `SETTMetter.exe` from the [latest release](https://github.com/otaviozanon/SETTMetter/releases/latest) and run it. No install step: the widget attaches to the taskbar and stays there.

Administrator rights are required — the manifest requests elevation, which the CPU temperature sensor needs.

## Usage

Right-click the widget (or the fallback tray icon) for:

- **Devices** — every detected device with its level; uncheck to hide it from the taskbar.
- **System sensors** — CPU usage/temperature, RAM usage, per-GPU usage/temperature, per-GPU VRAM usage, SSD temperature.
- **Presets** — apply a sensor/icon combination in one click.
- **CPU/GPU alert thresholds** — 60–95 °C, independently per component.
- **Refresh rate** — how often system sensors are sampled.
- **Screen** — which monitor's taskbar the widget is docked to (secondary taskbars need "Show my taskbar on all displays").
- **Icons** — which icons are drawn next to the values.
- **Show usage history graph**, **Refresh now**, **Low battery alerts**, **Start with Windows**, **Probe an unsupported device (60 s)**, **Copy diagnostics**, **Exit**.

Hover the widget for each device's full name and status; the tooltip also lists the top CPU/RAM/GPU-consuming processes.

## How it works

- **Taskbar widget** — a small transparent child window attached to the taskbar next to the notification area, redrawn with per-pixel alpha text.
- **Devices** — Logitech HID++ feature queries, BLE GATT Battery Service, classic Bluetooth properties, `Windows.Gaming.Input` for Xbox controllers, raw input reports for DualSense/DualShock, and vendor dongle protocols for the other supported brands.
- **Sensors** — PDH performance counters for CPU/RAM/disk, DXGI + D3DKMT/NVML for GPU usage, VRAM and temperature across every adapter, and a PawnIO-based MSR reader for CPU package temperature.
- **Settings** — hot-path values (hidden devices/sensors, masks, refresh interval) are cached in memory and reloaded only after a menu toggle, instead of hitting the registry on every redraw.

## Credits

- [taskbar-battery](https://github.com/renanleonel/windows-taskbar-battery) by Renan Leonel — the base this project was forked from.
- [Wireless-Device-Battery-Monitor](https://github.com/krisgfx9/Wireless-Device-Battery-Monitor), [Solaar](https://github.com/pwr-Solaar/Solaar), [HeadsetControl](https://github.com/Sapd/HeadsetControl), [MchoseBatteryTray](https://github.com/HifumiDaisuki01/MchoseBatteryTray), [OpenRazer](https://github.com/openrazer/openrazer), [rivalcfg](https://github.com/flozz/rivalcfg), [OpenLinkHub](https://github.com/jurkovic-nikola/OpenLinkHub), [TrafficMonitor](https://github.com/zhongyang219/TrafficMonitor) — protocol research and the taskbar placement technique.

Not affiliated with or endorsed by Logitech, Razer, Corsair, SteelSeries, MCHOSE, Delux, Compx, Redragon, Pulsar, Fallen, Sony, Apple, NVIDIA or Microsoft. All trademarks belong to their owners.

## License

[MIT](LICENSE)
