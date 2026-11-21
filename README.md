<p align="center"><img src="assets/icon64.png" width="64" alt="HDRSnap"></p>

# HDRSnap

### HDR-aware screenshots and screen recordings for Windows.

![OS](https://img.shields.io/badge/Windows-10%20%7C%2011-1a9fff?style=flat-square) ![GPU](https://img.shields.io/badge/GPU-DirectX%2011-86dc8f?style=flat-square) ![Language](https://img.shields.io/badge/Rust-stable-c4a7ff?style=flat-square) ![Version](https://img.shields.io/github/v/release/berczeller/HDRSnap?style=flat-square&label=version&color=ffc36b) ![License](https://img.shields.io/badge/license-MIT-6fd6a0?style=flat-square)

---

Most capture tools grab an HDR desktop as if it were SDR, so screenshots and videos come out washed out or blown out. HDRSnap captures each monitor in its native format and tone maps HDR to SDR, so what you save looks like what you see. It sits in the tray, works with hotkeys and records video with sound through FFmpeg.

## Features

- Region, window and full screen screenshots to PNG or JPG, copied to the clipboard
- MP4, WebM or GIF recording with hardware encoders (NVENC, AMF, Quick Sync) or CPU encoders
- Correct HDR to SDR conversion on the CPU for screenshots and on the GPU for recordings, with an adaptive highlight roll-off
- System audio and microphone recording, mixed into one track or kept as separate tracks
- Hotkey presets (HDRSnap F12, ShareX style, Snipping Tool style) or any custom combination
- Optional cursor capture, a red border around region recordings that never shows up in the video
- Installer or portable zip, start with Windows from the tray menu

## Requirements

- Windows 10 or 11, x64
- A DirectX 11 GPU (Desktop Duplication). Without one, HDRSnap falls back to GDI capture, which can't handle HDR.
- `ffmpeg.exe` for recording. Both release packages include it.

## Install

Pick one from [Releases](https://github.com/berczeller/HDRSnap/releases):

- **Installer:** run `HDRSnap-0.3.2-setup.exe`. It installs for your user without an admin prompt (you can choose all users), adds Start menu entries and an uninstaller. Settings and logs go to `%APPDATA%\HDRSnap`, screenshots to `Pictures\HDRSnap`, recordings to `Videos\HDRSnap`.
- **Portable:** extract `HDRSnap-0.3.2-portable.zip` anywhere and run `HDRSnap.exe`. Settings and captures stay in that folder.

`SHA256SUMS.txt` on the release page has the checksums.

## Usage

HDRSnap lives in the tray (orange viewfinder icon). The first launch opens a short setup wizard for hotkeys, folders and formats. Double-click the tray icon or pick **Settings...** to change anything later; changes apply right away.

| Action | Default hotkey |
|---|---|
| Capture region | `Ctrl+Alt+F12` |
| Capture full screen | `Ctrl+F12` |
| Start / stop recording a region | `Ctrl+Alt+Shift+F12` |
| Start / stop recording full screen | `Ctrl+Shift+F12` |

Presets in the wizard and **Settings > Hotkeys** switch all four at once:

| Preset | Region | Full screen | Record region | Record full screen |
|---|---|---|---|---|
| HDRSnap (F12) | Ctrl+Alt+F12 | Ctrl+F12 | Ctrl+Alt+Shift+F12 | Ctrl+Shift+F12 |
| ShareX style | Ctrl+Print Screen | Print Screen | Shift+Print Screen | Ctrl+Shift+Print Screen |
| Snipping Tool style | Print Screen | Ctrl+Print Screen | Shift+Print Screen | Ctrl+Shift+Print Screen |

- **Region selection:** drag to select, or hover a window and click to grab it. `Enter` takes the whole monitor under the cursor, `Esc` or right-click cancels.
- **Recording:** the tray icon turns red. Click it or press the same hotkey to stop. Full screen recording covers one monitor (under the cursor or the primary one, set in **Settings > Recording**).
- **Audio:** **Settings > Recording > Audio** turns on system audio and the microphone. With both on, **Separate tracks** keeps them apart in the file so an editor can balance them; most players only play the first track. MP4 uses AAC, WebM uses Opus, GIFs have no sound.
- **Start with Windows:** **Settings > General**, the setup wizard or the tray menu. Switching HDRSnap off in Task Manager > Startup apps is reflected in the toggle.
- **Command line:** `--capture` (one full screen shot), `--record=<seconds>` (record for that long), `--settings`, `--setup`, `--selftest-gpu`.

> [!NOTE]
> For the Print Screen presets, turn off **Use the Print screen key to open screen capture** in Windows' keyboard settings, or the Snipping Tool takes the key first. HDRSnap warns and links there when it's on.

## How it works

With HDR on, Windows composes the desktop in scRGB (linear, 1.0 = 80 nits) and places SDR content at your **SDR content brightness**. HDRSnap:

1. Captures each monitor with DXGI Desktop Duplication: FP16 on HDR monitors, 8-bit on SDR monitors (saved untouched).
2. Divides by the monitor's SDR white level, so desktop content comes out exactly as with HDR off. Unit tests check this is bit-exact at every SDR brightness setting.
3. If the frame has real HDR content (games, HDR video), rolls highlights off smoothly up to the measured peak instead of clipping them.
4. Brings wide-gamut colors into sRGB by desaturating toward the pixel's luminance, which keeps hue and brightness.

**Settings > HDR** has the roll-off controls, the list of displays with their HDR state, and a GPU self test that compares the recording shader with the screenshot code.

Files, next to the exe (portable) or in `%APPDATA%\HDRSnap` (installed):

```
settings.json   all settings, safe to edit by hand
hdrsnap.log     what happened, with per-capture HDR statistics
ffmpeg.log      output of the last recording's FFmpeg run
```

Outside that folder, HDRSnap only writes the optional start-with-Windows entry under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.

## Troubleshooting

| Problem | Fix |
|---|---|
| A hotkey does nothing | Another app already uses it; HDRSnap shows a notification. Pick another one in **Settings > Hotkeys**. |
| Print Screen opens the Snipping Tool | Turn off **Use the Print screen key to open screen capture** in Windows' keyboard settings. |
| Recording is greyed out in the tray menu | `ffmpeg.exe` is missing next to `HDRSnap.exe`. |
| Colors look washed out | Check `hdrsnap.log` for `warning:` or `using GDI` lines and open an issue with them. |

## Building from source

Rust (stable, MSVC toolchain) with the Visual Studio 2022 Build Tools C++ workload. For the release packages, also Inno Setup 6.

```
cargo build --release
cargo test --lib
powershell -ExecutionPolicy Bypass -File build-release.ps1
```

`cargo build` gives `target\release\HDRSnap.exe`; put `ffmpeg.exe` next to it to record. `build-release.ps1` runs the tests and writes the installer and portable zip to `dist\`. On first run it downloads the pinned FFmpeg 7.1.1 essentials build into `vendor\ffmpeg\` and checks its SHA-256.

```
src/tonemap.rs, src/audio.rs     HDR to SDR conversion, audio timeline (platform independent, tested)
src/encoders.rs, src/settings.rs encoder catalogue and FFmpeg arguments, settings.json
src/ui.rs                        settings window and setup wizard (egui, separate process)
src/win/app.rs                   tray app, hotkeys, screenshot and recording flows
src/win/capture.rs, display.rs   Desktop Duplication capture, monitor and HDR detection
src/win/record.rs, gpu.rs        recording pipeline, GPU tone mapping compute shader
src/win/audio.rs                 WASAPI loopback and microphone capture
installer/HDRSnap.iss            Inno Setup script
```

## Credits

**Author:** Tom Berczeller

- [FFmpeg](https://ffmpeg.org), Windows build by [gyan.dev](https://www.gyan.dev/ffmpeg/builds/), bundled as `ffmpeg.exe`
- [egui / eframe](https://github.com/emilk/egui) by Emil Ernerfeldt
- [windows-rs](https://github.com/microsoft/windows-rs) by Microsoft
- [png](https://github.com/image-rs/image-png) by the image-rs team, [jpeg-encoder](https://github.com/vstroebel/jpeg-encoder) by Volker Ströbel
- [serde](https://serde.rs) by David Tolnay and contributors
- [Inno Setup](https://jrsoftware.org/isinfo.php) by Jordan Russell and Martijn Laan, for the installer

## License

Released under the [MIT License](LICENSE), © 2026 Tom Berczeller.

`ffmpeg.exe` is a separate program licensed under the GPL v3 and only run as an external process; see `FFMPEG-LICENSE.txt`. The Rust dependencies are MIT or Apache-2.0.
