# HDRSnap 0.3.1

Lightweight screenshot and screen recording tool for Windows 10/11 that handles **HDR** correctly: captures from HDR monitors look the same as what you see, not washed out or blown out.

## Download

Each release comes in two flavours, both with `ffmpeg.exe` included:

- **`HDRSnap-<version>-setup.exe`**: Windows installer. Installs for your user (no admin prompt; you can choose all users), adds Start menu entries and an uninstaller. Settings and logs go to `%APPDATA%\HDRSnap`, screenshots to `Pictures\HDRSnap`, recordings to `Videos\HDRSnap`.
- **`HDRSnap-<version>-portable.zip`**: unzip anywhere (a USB stick works). Everything, including settings and captures, stays in that folder.

## Getting started

1. Run the installer, or unzip the portable version anywhere and run `HDRSnap.exe`. It lives in the system tray (orange viewfinder icon).
2. The first time, a short setup wizard asks for hotkeys, output folders and formats. You can skip it to keep the defaults, and reopen it any time from *Settings > General*.
3. Double-click the tray icon (or right-click > *Settings…*) to change anything. Changes apply instantly.

Keep `ffmpeg.exe` next to `HDRSnap.exe`; recording needs it. Screenshots work without it.

## Default hotkeys

| Action | Hotkey |
|---|---|
| Capture region | Ctrl+Alt+F12 |
| Capture full screen | Ctrl+F12 |
| Start / stop recording a region | Ctrl+Alt+Shift+F12 |
| Start / stop recording full screen | Ctrl+Shift+F12 |

Presets in the setup wizard and *Settings > Hotkeys* switch all four at once:

| Preset | Region | Full screen | Record region | Record full screen |
|---|---|---|---|---|
| HDRSnap (F12) | Ctrl+Alt+F12 | Ctrl+F12 | Ctrl+Alt+Shift+F12 | Ctrl+Shift+F12 |
| ShareX style | Ctrl+Print Screen | Print Screen | Shift+Print Screen | Ctrl+Shift+Print Screen |
| Snipping Tool style | Print Screen | Ctrl+Print Screen | Shift+Print Screen | Ctrl+Shift+Print Screen |

For the Print Screen presets, turn off *Use the Print screen key to open screen capture* in Windows' keyboard settings (HDRSnap warns and links there when it's on).

**Start with Windows:** *Settings > General*, the setup wizard or the tray menu. If you switch HDRSnap off in Task Manager > Startup apps, the toggle follows.

While recording, the tray icon turns red. Click it (or press the same hotkey again) to stop.

**Region selection:** drag to select, or hover a window and click to grab it. Enter takes the whole monitor under the cursor. Esc or right-click cancels.

## Screenshots

- PNG (lossless) or JPG. Both have a **Quality** slider: for JPG it's image quality, for PNG (always lossless) higher Quality means smaller files at a slightly slower save.
- Full screen captures all monitors or just the one under the cursor. The mouse cursor can be included.
- Saved to the screenshots folder and copied to the clipboard. Click the notification to open the file.

## Recording

- **MP4**, **WebM** or **GIF**, with encoder, Quality and frame rate settings.
- Encoders come from FFmpeg: NVIDIA NVENC (H.264 / HEVC / AV1), AMD AMF, Intel Quick Sync, x264 and x265 (CPU), VP9 and libaom AV1 (CPU). *Automatic* picks the best one your PC supports. Settings greys out encoders that don't work on your system.
- H.264 hardware encoders are limited to 4096 px wide. On wider displays such as a 5120×1440 super-ultrawide, *Automatic* uses HEVC, or AV1 for WebM.
- Full screen recording covers one monitor: the one under the cursor or the primary one. A region that crosses monitors is trimmed to one monitor.
- The cursor is drawn into the video (optional). A red frame shows around region recordings; it is excluded from capture so it never appears in the video.
- HDR monitors get the same tone mapping as screenshots, done on the GPU. The highlight roll-off adapts smoothly over time, so desktop content isn't dimmed and HDR highlights aren't clipped.
- GIFs are recorded losslessly first, then converted with an optimised palette when you stop. Long GIFs can take a few seconds to finish.

### Audio

- *Settings > Recording > Audio*: record **system audio** (everything the PC plays through the default output device) and/or a **microphone** (Windows default or a specific one).
- With both on, choose **Mixed into one track** or **Separate tracks**. Separate tracks keep system audio and microphone apart inside the same MP4 / WebM (named "System audio" and "Microphone") so a video editor can balance them; most players only play the first track.
- MP4 uses AAC, WebM uses Opus. GIFs have no sound.
- Audio stays in sync with the picture: silence while nothing plays is filled in, and small clock drift between the sound card and the PC is corrected.

## How the HDR handling works

With HDR on, Windows composes the desktop in **scRGB** (linear, 1.0 = 80 nits) and places SDR content at your *SDR content brightness*. HDRSnap:

1. Captures each monitor with DXGI Desktop Duplication in its native format: FP16 on HDR monitors, 8-bit on SDR monitors. SDR monitors are saved untouched.
2. Divides by that monitor's SDR white level, so normal desktop content comes out exactly as it would with HDR off. This is unit-tested to be bit-exact at every SDR brightness setting.
3. If the frame has real HDR content (games, HDR video), compresses highlights with a smooth roll-off up to the measured peak instead of clipping. Desktop-only captures skip this.
4. Brings wide-gamut colours into sRGB by desaturating toward the pixel's luminance, which keeps hue and brightness.

Settings > HDR has the roll-off controls, a list of your displays with their HDR state and brightness, and a GPU self test that checks the recording shader against the screenshot code.

## Files

Portable: all next to the exe. Installed: in `%APPDATA%\HDRSnap` (open it from *Settings > About > Open settings folder*); default capture folders are `Pictures\HDRSnap` and `Videos\HDRSnap`. The installed copy is recognised by the `HDRSnap.installed` file next to the exe.


- `settings.json`: all settings. Edited by the settings window, but you can also edit it by hand.
- `hdrsnap.log`: what happened, including per-capture HDR statistics.
- `ffmpeg.log`: output of the last recording's FFmpeg run.
- `Screenshots\`, `Recordings\`: default output folders.

Outside the folder, the only thing HDRSnap writes is the optional *Start with Windows* entry under `HKCU\…\Run`.

Command line: `--capture` (one full screen shot, then exit), `--record=<seconds>` (record the monitor set in Settings for that long, then exit), `--settings`, `--setup`, `--selftest-gpu`.

## Building from source

On Windows: install Rust (https://rustup.rs, MSVC toolchain plus Visual Studio Build Tools), then:

```
cargo build --release
```

Output: `target\release\HDRSnap.exe`. Put `ffmpeg.exe` next to it for recording. Run the core unit tests with `cargo test --lib`. To build both release packages (portable zip + installer) into `dist\`, install Inno Setup 6 (`winget install JRSoftware.InnoSetup`) and run `powershell -ExecutionPolicy Bypass -File build-release.ps1`. It downloads the pinned FFmpeg 7.1.1 essentials build into `vendor\ffmpeg\` (checked by SHA-256) the first time.

### Layout

- `src/tonemap.rs`: HDR to SDR conversion (CPU), tests
- `src/encoders.rs`: encoder catalogue, Quality mapping, FFmpeg arguments
- `src/settings.rs`, `src/hotkey.rs`, `src/imageio.rs`: settings (with v1 migration), hotkeys, PNG/JPG/EXR
- `src/ui.rs`: settings window and setup wizard (egui, runs as a separate process so the tray app stays tiny)
- `src/win/app.rs`: tray app, hotkeys, screenshot and recording flows
- `src/win/capture.rs`, `src/win/display.rs`: capture and monitor/HDR detection
- `src/win/overlay.rs`: region selector and recording border
- `src/win/record.rs`, `src/win/gpu.rs`: recording pipeline and GPU tone mapping compute shader
- `src/win/audio.rs`, `src/audio.rs`: WASAPI loopback / microphone capture, downmix and A/V timeline (audio reaches ffmpeg through named pipes)
- `src/win/cursor.rs`, `src/win/util.rs`: cursor rendering, log, clipboard, autostart

## Licenses

HDRSnap is MIT licensed. Rust dependencies (`windows`, `eframe`/`egui`, `png`, `jpeg-encoder`, `serde`) are MIT/Apache-2.0.

`ffmpeg.exe` is a separate program: FFmpeg 7.1.1 "essentials" build by gyan.dev, licensed under the **GPL v3**. Its source code is available at https://ffmpeg.org and build details at https://www.gyan.dev/ffmpeg/builds/. HDRSnap only runs it as an external process. See `FFMPEG-LICENSE.txt`.

## Changes

### 0.3.1

- **Start with Windows** is now also in the tray menu (with a check mark) and in the setup wizard. Turning it on in HDRSnap also re-enables it if it was switched off in Task Manager > Startup apps; switching it off there is reflected in HDRSnap's toggle.
- **Setup wizard keeps your current hotkeys** instead of resetting them, and offers presets (HDRSnap F12, ShareX style, Snipping Tool style) plus a dropdown of common combinations per shortcut, with *Custom…* for anything else. *Settings > Hotkeys* has the same presets.
- Warning (with a link to Windows settings) when a Print Screen hotkey is used while Windows gives that key to the Snipping Tool.
- *Skip setup* keeps your current settings instead of resetting everything to defaults.
- Fix: a `settings.json` saved with a UTF-8 BOM (Notepad, PowerShell) was treated as invalid and replaced by defaults.

### 0.3.0

- **Fix: washed out / over-bright recordings and screenshots after switching, minimising or maximising windows.** Several paths could quietly end up capturing an HDR monitor in 8 bits (the same clipped "ShareX look" HDRSnap exists to avoid):
  - Desktop Duplication was created with `[FP16, BGRA8]`, so DXGI was allowed to hand out 8-bit frames from an HDR desktop. HDR monitors now offer FP16 only, every frame's format is checked, and an 8-bit frame on an HDR monitor makes HDRSnap re-create the duplication instead of using it. The first frame of a recording is no longer allowed to lock the whole recording into the SDR path.
  - When Desktop Duplication refused to start for a moment (common during full screen / window transitions), recording and screenshots fell back to GDI straight away. They now retry with fresh DXGI objects for a few seconds first and log a warning if GDI is really needed.
  - A failed DXGI display enumeration fell back to the GDI monitor list, which has no HDR information. It is now retried.
  - A capture format change mid-recording no longer stops the recording.
- **New: audio recording** (system audio and microphone, mixed or as separate tracks).
- Dropped frames (encoder too slow) are now made up with repeats, so the video never runs shorter than real time.
- `--record=<seconds>` command line option.
- Two release packages: Windows installer and portable zip, both with FFmpeg 7.1.1 essentials.
- Fix: with FFmpeg 7.x, recording with audio stalled the video for ~5 s at the start (ffmpeg probed the raw audio inputs in real time).
