# SETTMetter

Wireless device battery levels and system monitoring (CPU/RAM/GPU/VRAM/temperatures) on the Windows 11 taskbar.

One native executable, no installer, no runtime, no service. The only network access is the update check against the GitHub releases page, plus the optional CPU-temperature driver fetched through `winget` when you ask for it.

## Features

- **Devices** — Logitech (HID++, G435/G735), Bluetooth LE and classic Bluetooth, Xbox, PlayStation, Switch Pro, Steam, GameSir, Flydigi and BEITONG controllers, Corsair, Razer (mice and headsets), SteelSeries (incl. Nova Elite), HyperX, Astro, ROCCAT, NZXT, Plantronics, Alienware, MAONO, CROCIRIS, DARKRIM, Press Play, Fractal, Airoha-dongle headsets (Audeze Maxwell, AKG N9, ENDORFY Viro), AULA; Glorious, Hitscan, WLMouse, LAMZU, Arbiter, Endgame Gear, VAXEE, Scyrox, Ninjutso, Sprime, Finalmouse, MSI, Cooler Master, ZOWIE, PIAO, Niceboy, Chimera, Zebronics, Zenith, RAWM, Darmoshark, MCHOSE and Compx/Areson/Kysona mice (Pulsar, ATK/VXE, G-Wolves, Redragon, HECATE, Hitscan, Attack Shark); Keychron, Evision (ATK/VGN/AULA/Redragon), SONiX, Rongyuan, DAREU, Royal Kludge, Wobkey, Cherry, NuPhy and Semico keyboards; Turtle Beach, Sony INZONE, PlayStation Link, JBL/Jabra/Soundcore/Marshall/Moondrop, Phone Link, AirPods/Beats, Galaxy Buds, Nothing Ear and Fast Pair audio devices; RAPOO, VGN Client and Incott/Wraith mice; ASUS ROG devices; generic USB HID and optional [HeadsetControl](https://github.com/Sapd/HeadsetControl) headsets.
- **Sensors** — CPU, RAM, GPU and VRAM usage plus CPU/GPU/SSD temperatures, each with its own checkbox, including per-adapter toggles on multi-GPU machines.
- **Presets** — one-click sensor and icon combinations.
- **Alerts** — CPU/GPU temperature thresholds with hysteresis, and a notification when a device drops to 15%.
- **History graph** — a popup plotting the last ~5 minutes of CPU/RAM/GPU usage.
- **Audio** — lock the default output/microphone in place, mute the microphone with one click, keep a separate Default and Communications device, disable audio enhancements and unused endpoints, save device profiles, and route a microphone to any output ("listen to this device"). Drag the device cards to set your own order (kept across restarts and hot-plug). Everything lives in a flyout panel and in the right-click menu.
- **Microphone indicator** — a mic glyph at the right end of the widget, red and slashed while every active microphone is muted; click it to mute/unmute (it can be hidden in **Settings → Audio**). A configurable system-wide **mute shortcut** (**Settings → Audio → Mute shortcut**, `Break` by default) toggles the Windows microphone; when an app binds the same key (Discord's mute keybind), both toggle together, and the mic click sends that key too.
- **Native look** — follows the light/dark taskbar theme, scales with DPI, uses Windows' own Fluent icons.
- **Fallback tray icon** — keeps the settings menu reachable when every device and sensor is hidden.
- **Updates** — checks the GitHub releases page at launch; when a new version exists, **Update to vX** in the menu downloads it and replaces the running exe (restarts automatically, still elevated).
- **Private** — no telemetry, no accounts; the only outbound call is that update check.

## Install

Download `SETTMetter.exe` from the [latest release](https://github.com/otaviozanon/SETTMetter/releases/latest) and run it. No install step: the widget attaches to the taskbar and stays there.

Administrator rights are required - the manifest requests elevation, which the CPU temperature sensor needs. Because of that, **Start with Windows** (off by default) registers a logon task with the highest privileges: Windows silently blocks elevated programs started from the Run key.

CPU temperature also needs the [PawnIO](https://pawnio.eu) driver, a one-time signed install. Without it the reading falls back to Windows' ACPI thermal zone, which most desktop boards don't expose — so a desktop shows no CPU temperature. The app offers to install it through winget (`namazso.PawnIO`) and reopens itself when done; the same item stays in the menu. "Copy diagnostics" reports which source answered, or why none did.

## Usage

Right-click the widget (or the fallback tray icon) for the settings window:

- **Sensors** — live CPU/RAM/GPU/VRAM/SSD readings next to their checkboxes, per-adapter GPU rows, and the quick presets.
- **Display** - which group icons are drawn, the refresh rate, which monitor's taskbar hosts the widget, the manual position offset, and how tightly the readings are packed.
- **Alerts** — low-battery notifications and the CPU/GPU temperature thresholds, as sliders.
- **Devices** — every detected device with its level; uncheck to hide it from the taskbar.
- **Audio** — mute, device lock, separate communications device, enhancement/unused-device automation, the mute shortcut, re-enabling all endpoints, restarting the Windows audio service, and a shortcut to the full audio panel.
- **General** — start with Windows, updates, refresh now, usage history, diagnostics, device probing, the CPU-temperature driver, the classic menu and exit.

Left-clicking the mic glyph at the right end of the widget mutes/unmutes; left clicks anywhere else on the widget do nothing. Right-clicking the mic opens the audio panel; right-clicking anywhere else opens the settings window.

Both windows have a **power** button in the header (closes SETTMetter; a second click confirms, like **Exit SETTMetter** in General). Pinned devices stay at the top of the list, and dragging a card still saves your order inside the pinned and unpinned blocks.

Hover the widget for each device's full name and status; the tooltip also lists the top CPU/RAM/GPU-consuming processes and the current audio defaults.

## How it works

- **Taskbar widget** — a small transparent child window attached to the taskbar next to the notification area, redrawn with per-pixel alpha text.
- **Panels** — the audio panel and the settings window are ownerdraw popups sharing one painter (`src/ui`): 32-bit DIB backbuffer, GDI text, DWM-rounded corners, follows the Windows light/dark theme.
- **Devices** — Logitech HID++ feature queries, BLE GATT Battery Service, classic Bluetooth properties, `Windows.Gaming.Input` for Xbox controllers, raw input reports for DualSense/DualShock, and vendor dongle protocols for the other supported brands.
- **Sensors** — PDH performance counters for CPU/RAM/disk, DXGI + D3DKMT/NVML for GPU usage, VRAM and temperature across every adapter, and a PawnIO-based MSR reader for CPU package temperature.
- **Settings** — hot-path values (hidden devices/sensors, masks, refresh interval) are cached in memory and reloaded only after a menu toggle, instead of hitting the registry on every redraw.

## Credits

- [taskbar-battery](https://github.com/renanleonel/windows-taskbar-battery) by Renan Leonel — the base this project was forked from.
- [audio-lock](https://github.com/Paragon-Tweaks/audio-lock) by Paragon Tweaks — the audio device lock, microphone mute, profile manager and flyout panel (GPL-3.0).
- [PawnIO](https://pawnio.eu) by namazso — the MSR/SMN driver and the `IntelMSR`/`AMDFamily17` modules embedded in `assets/pawnio` (driver GPL-2.0, modules LGPL-2.1, see `assets/pawnio/COPYING.LGPL-2.1`).
- [Wireless-Device-Battery-Monitor](https://github.com/krisgfx9/Wireless-Device-Battery-Monitor), [Solaar](https://github.com/pwr-Solaar/Solaar), [HeadsetControl](https://github.com/Sapd/HeadsetControl), [MchoseBatteryTray](https://github.com/HifumiDaisuki01/MchoseBatteryTray), [OpenRazer](https://github.com/openrazer/openrazer), [rivalcfg](https://github.com/flozz/rivalcfg), [OpenLinkHub](https://github.com/jurkovic-nikola/OpenLinkHub), [TrafficMonitor](https://github.com/zhongyang219/TrafficMonitor) — protocol research and the taskbar placement technique.

Not affiliated with or endorsed by Logitech, Razer, Corsair, SteelSeries, MCHOSE, Delux, Compx, Redragon, Pulsar, Fallen, Sony, Apple, NVIDIA or Microsoft. All trademarks belong to their owners.

## License

This program is free software: you can redistribute it and/or modify it under the terms of the
[GNU General Public License version 3](LICENSE) as published by the Free Software Foundation.

It is distributed in the hope that it will be useful, but **without any warranty**; without even
the implied warranty of merchantability or fitness for a particular purpose. See the LICENSE file
for details.
