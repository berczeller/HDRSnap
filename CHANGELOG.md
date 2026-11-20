# Changelog

All notable changes to this project are documented here. The format follows Keep a Changelog and the project uses Semantic Versioning.

## [0.3.2] - 2026-11-20

### Fixed
- The region selector's message loop could in theory miss the moment you finish a selection

### Changed
- Releases now include a source zip and `SHA256SUMS.txt`
- New README and this changelog

## [0.3.1] - 2026-11-16

### Added
- Start with Windows toggle in the tray menu and the setup wizard
- Hotkey presets (HDRSnap F12, ShareX style, Snipping Tool style) and a dropdown of common combinations per shortcut
- Warning with a link to Windows' keyboard settings when a Print Screen hotkey is used while Windows gives that key to the Snipping Tool

### Changed
- The setup wizard keeps your current hotkeys instead of resetting them
- Skip setup keeps your current settings
- Turning Start with Windows on also re-enables it when it was switched off in Task Manager, and switching it off there shows in the toggle

### Fixed
- A `settings.json` saved with a UTF-8 BOM (Notepad, PowerShell) was treated as invalid and replaced by defaults

## [0.3.0] - 2026-11-15

### Added
- System audio and microphone recording, mixed into one track or as separate tracks
- Windows installer next to the portable zip, both with FFmpeg 7.1.1
- `--record=<seconds>` command line option

### Fixed
- Recordings and screenshots came out washed out and over-bright after switching, minimizing or maximizing windows: HDR monitors are always captured in FP16 now, and failed Desktop Duplication or display enumeration is retried instead of falling back to GDI
- A capture format change in the middle of a recording no longer stops it
- Dropped frames are made up with repeats, so videos are never shorter than real time
- With FFmpeg 7.x, recordings with audio stalled the video for about 5 s at the start

[0.3.2]: https://github.com/berczeller/HDRSnap/releases/tag/0.3.2
[0.3.1]: https://github.com/berczeller/HDRSnap/releases/tag/0.3.1
[0.3.0]: https://github.com/berczeller/HDRSnap/tree/0.3.0
