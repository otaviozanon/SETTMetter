# SETTMetter

**Wireless device battery levels and system monitoring (CPU/RAM/GPU/VRAM/temperatures) right on the Windows 11 taskbar.**

- **Devices:** Logitech receivers/dongles (HID++), Bluetooth LE and classic Bluetooth, Xbox/PlayStation controllers, Delux/MCHOSE/Corsair/Razer/SteelSeries mice & keyboards over their dongles, generic USB HID, and optional [HeadsetControl](https://github.com/Sapd/HeadsetControl) headsets.
- **System monitoring:** CPU/RAM/GPU/VRAM usage and CPU/GPU/SSD temperatures, with an individual on/off checkbox per sensor — including per-adapter checkboxes on multi-GPU machines.
- **Presets:** one-click sensor/icon combinations (Full, Minimal, Usage only, Temperatures only, Numbers only).
- **Temperature alerts:** configurable CPU/GPU thresholds, with hysteresis so they don't spam.
- **Usage history graph:** a popup window plotting the last ~5 minutes of CPU/RAM/GPU usage.
- **Lightweight:** a single native Rust executable with no installer or runtime dependencies, idle between checks.
- **Native look:** follows the light/dark taskbar theme, scales with DPI, uses Windows' own Fluent icons.
- **Low battery alerts:** a Windows notification when a device drops to 15%.
- **Fallback tray icon:** if every device and sensor is hidden, a small notification-area icon keeps the right-click settings menu reachable.
- **Private:** everything stays on your PC. No network requests, no telemetry, no accounts.

---

## Using it

Right-click the widget (or the fallback tray icon, if everything is hidden) for:

- **Devices**: every detected device, with its level. Uncheck to hide from the taskbar.
- **System sensors**: CPU usage/temperature, RAM usage, per-GPU usage/temperature, per-GPU VRAM usage, SSD temperature — each its own checkbox.
- **Presets**: apply a sensor/icon combination in one click.
- **CPU/GPU alert thresholds**: pick the temperature that triggers a notification (60–95 °C), independently per component.
- **Refresh rate**: how often system sensors are sampled.
- **Icons**: toggle which icons are drawn next to values.
- **Show usage history graph**: opens a popup plotting recent CPU/RAM/GPU usage.
- **Refresh now**, **Low battery alerts**, **Start with Windows**, **Probe an unsupported device (60 s)**, **Copy diagnostics**, **Exit**.

Hover over the widget for each device's full name and status; the tooltip also lists the top CPU/RAM/GPU-consuming processes.

## How it works

- **Taskbar widget:** a small transparent child window attached to the taskbar next to the notification area (same technique as TrafficMonitor), redrawn with per-pixel alpha text.
- **Devices:** Logitech HID++ feature queries, BLE GATT Battery Service, classic Bluetooth device properties, `Windows.Gaming.Input` for Xbox controllers, raw input reports for DualSense/DualShock, and vendor-specific dongle protocols for the other supported brands.
- **System monitoring:** PDH performance counters for CPU/RAM/disk, DXGI + D3DKMT/NVML for GPU usage/VRAM/temperature across every detected adapter, and a PawnIO-based MSR reader for CPU package temperature.
- **Registry settings cache:** hot-path reads (hidden devices/sensors, masks, refresh interval) are cached in memory and only reloaded after a menu toggle, instead of hitting the registry every redraw.

## Credits

- [taskbar-battery](https://github.com/renanleonel/windows-taskbar-battery) by Renan Leonel: the base this project was forked from.
- TraySense: the source of the system monitoring logic (CPU/RAM/GPU/VRAM/temperatures), ported here for personal use.
- [Wireless-Device-Battery-Monitor](https://github.com/krisgfx9/Wireless-Device-Battery-Monitor), [Solaar](https://github.com/pwr-Solaar/Solaar), [HeadsetControl](https://github.com/Sapd/HeadsetControl), [MchoseBatteryTray](https://github.com/HifumiDaisuki01/MchoseBatteryTray), [OpenRazer](https://github.com/openrazer/openrazer), [rivalcfg](https://github.com/flozz/rivalcfg), [OpenLinkHub](https://github.com/jurkovic-nikola/OpenLinkHub), [TrafficMonitor](https://github.com/zhongyang219/TrafficMonitor): protocol research and taskbar placement technique.

This project isn't affiliated with or endorsed by Logitech, Razer, Corsair, SteelSeries, MCHOSE, Delux, Sony, Apple, NVIDIA, or Microsoft. All trademarks belong to their owners. For personal use only.

## License

[MIT](LICENSE)
