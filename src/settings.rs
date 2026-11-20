//! settings.json, stored next to the executable (portable).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const SETTINGS_VERSION: u32 = 2;

/// Default (screenshots, recordings) folders used instead of
/// `<data dir>\Screenshots` / `Recordings` when set (installed copy:
/// Pictures\HDRSnap and Videos\HDRSnap).
pub static DEFAULT_DIRS: std::sync::OnceLock<(PathBuf, PathBuf)> = std::sync::OnceLock::new();

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub settings_version: u32,
    /// The first-run wizard has been completed or skipped.
    pub setup_completed: bool,

    // --- General --------------------------------------------------------
    /// Screenshots folder. Empty = "Screenshots" next to the exe.
    /// Relative paths are relative to the exe folder.
    pub output_folder: String,
    /// Recordings folder. Empty = "Recordings" next to the exe.
    pub video_output_folder: String,
    pub filename_prefix: String,
    pub video_filename_prefix: String,
    pub copy_to_clipboard: bool,
    pub show_notification: bool,
    pub start_with_windows: bool,

    // --- Hotkeys --------------------------------------------------------
    pub hotkey_fullscreen: String,
    pub hotkey_region: String,
    /// Start / stop recording of the full screen.
    pub hotkey_record_fullscreen: String,
    /// Start / stop recording of a region.
    pub hotkey_record_region: String,

    // --- Screenshots ----------------------------------------------------
    /// "png" or "jpg"
    pub image_format: String,
    /// JPG quality 1..100
    pub jpg_quality: u32,
    /// PNG quality 0..100. PNG is always lossless: higher Quality = stronger
    /// compression (smaller file, slower save).
    pub png_quality: u32,
    pub include_cursor_screenshot: bool,
    /// "all_monitors" or "cursor_monitor"
    pub fullscreen_target: String,

    // --- HDR ------------------------------------------------------------
    /// "auto" or "clip"
    pub tonemap_mode: String,
    /// Roll-off start relative to SDR white (0.5..0.98).
    pub tonemap_knee: f32,
    /// 0..1, how much the brightest highlights drift towards white.
    pub highlight_desaturation: f32,
    /// Debug: also save the untouched HDR frame of each HDR monitor as .exr.
    pub save_raw_hdr_exr: bool,

    // --- Recording ------------------------------------------------------
    /// "mp4", "webm" or "gif"
    pub video_format: String,
    /// FFmpeg encoder for MP4, or "auto".
    pub mp4_encoder: String,
    /// FFmpeg encoder for WebM, or "auto".
    pub webm_encoder: String,
    /// 1..100
    pub video_quality: u32,
    pub video_fps: u32,
    pub gif_fps: u32,
    /// GIF width limit in pixels, 0 = original size.
    pub gif_max_width: u32,
    pub gif_quality: u32,
    pub record_cursor: bool,
    /// Full screen recording: "cursor_monitor" or "primary"
    pub record_target: String,
    pub show_record_border: bool,

    // --- Audio (MP4 / WebM only) ----------------------------------------
    /// Record what the PC plays (WASAPI loopback of the default output).
    pub record_system_audio: bool,
    pub record_microphone: bool,
    /// Microphone endpoint ID; empty = Windows default microphone.
    pub microphone_device: String,
    /// "mixed" (one track) or "separate" (one track per source).
    pub audio_tracks: String,

    // --- Advanced -------------------------------------------------------
    /// "auto" (DXGI Desktop Duplication, GDI fallback) or "gdi" (debug).
    pub capture_method: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            settings_version: SETTINGS_VERSION,
            setup_completed: false,
            output_folder: String::new(),
            video_output_folder: String::new(),
            filename_prefix: "Screenshot".into(),
            video_filename_prefix: "Recording".into(),
            copy_to_clipboard: true,
            show_notification: true,
            start_with_windows: false,
            hotkey_fullscreen: "Ctrl+F12".into(),
            hotkey_region: "Ctrl+Alt+F12".into(),
            hotkey_record_fullscreen: "Ctrl+Shift+F12".into(),
            hotkey_record_region: "Ctrl+Alt+Shift+F12".into(),
            image_format: "png".into(),
            jpg_quality: 92,
            png_quality: 70,
            include_cursor_screenshot: false,
            fullscreen_target: "all_monitors".into(),
            tonemap_mode: "auto".into(),
            tonemap_knee: 0.75,
            highlight_desaturation: 0.5,
            save_raw_hdr_exr: false,
            video_format: "mp4".into(),
            mp4_encoder: "auto".into(),
            webm_encoder: "auto".into(),
            video_quality: 70,
            video_fps: 60,
            gif_fps: 15,
            gif_max_width: 960,
            gif_quality: 80,
            record_cursor: true,
            record_target: "cursor_monitor".into(),
            show_record_border: true,
            record_system_audio: true,
            record_microphone: false,
            microphone_device: String::new(),
            audio_tracks: "mixed".into(),
            capture_method: "auto".into(),
        }
    }
}

impl Settings {
    pub fn file_path(exe_dir: &Path) -> PathBuf {
        exe_dir.join("settings.json")
    }

    /// Load settings; missing file or fields fall back to defaults. Returns
    /// an error string if the file exists but could not be parsed.
    pub fn load(exe_dir: &Path) -> (Self, Option<String>) {
        match std::fs::read_to_string(Self::file_path(exe_dir)) {
            Ok(text) => match Self::from_json(&text) {
                Ok(s) => (s, None),
                Err(e) => (Self::default(), Some(format!("settings.json is invalid ({e}); using defaults"))),
            },
            Err(_) => (Self::default(), None),
        }
    }

    pub fn from_json(text: &str) -> Result<Self, String> {
        // Notepad / PowerShell may save hand edits with a UTF-8 BOM.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut v: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let version = v.get("settings_version").and_then(|x| x.as_u64()).unwrap_or(1);
        if version < 2 {
            migrate_v1(&mut v);
        }
        let s: Settings = serde_json::from_value(v).map_err(|e| e.to_string())?;
        Ok(s.sanitized())
    }

    pub fn save(&self, exe_dir: &Path) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let path = Self::file_path(exe_dir);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)
    }

    pub fn sanitized(mut self) -> Self {
        let d = Settings::default();
        self.settings_version = SETTINGS_VERSION;
        self.image_format = self.image_format.to_ascii_lowercase();
        if self.image_format == "jpeg" {
            self.image_format = "jpg".into();
        }
        if !["png", "jpg"].contains(&self.image_format.as_str()) {
            self.image_format = d.image_format.clone();
        }
        self.video_format = self.video_format.to_ascii_lowercase();
        if !["mp4", "webm", "gif"].contains(&self.video_format.as_str()) {
            self.video_format = d.video_format.clone();
        }
        if !["mixed", "separate"].contains(&self.audio_tracks.as_str()) {
            self.audio_tracks = d.audio_tracks.clone();
        }
        if self.mp4_encoder.trim().is_empty() {
            self.mp4_encoder = "auto".into();
        }
        if self.webm_encoder.trim().is_empty() {
            self.webm_encoder = "auto".into();
        }
        self.jpg_quality = self.jpg_quality.clamp(1, 100);
        self.png_quality = self.png_quality.min(100);
        self.video_quality = self.video_quality.clamp(1, 100);
        self.gif_quality = self.gif_quality.clamp(1, 100);
        self.video_fps = self.video_fps.clamp(1, 240);
        self.gif_fps = self.gif_fps.clamp(1, 50);
        if self.gif_max_width != 0 {
            self.gif_max_width = self.gif_max_width.clamp(64, 7680);
        }
        self.tonemap_knee = if self.tonemap_knee.is_finite() { self.tonemap_knee.clamp(0.5, 0.98) } else { d.tonemap_knee };
        self.highlight_desaturation =
            if self.highlight_desaturation.is_finite() { self.highlight_desaturation.clamp(0.0, 1.0) } else { d.highlight_desaturation };
        if self.filename_prefix.trim().is_empty() {
            self.filename_prefix = d.filename_prefix.clone();
        }
        if self.video_filename_prefix.trim().is_empty() {
            self.video_filename_prefix = d.video_filename_prefix.clone();
        }
        self
    }

    fn resolve(exe_dir: &Path, s: &str, default: &str) -> PathBuf {
        let s = s.trim();
        if s.is_empty() {
            if let Some((shots, videos)) = DEFAULT_DIRS.get() {
                return if default == "Recordings" { videos.clone() } else { shots.clone() };
            }
            return exe_dir.join(default);
        }
        let p = PathBuf::from(s);
        if p.is_absolute() {
            p
        } else {
            exe_dir.join(p)
        }
    }

    pub fn output_dir(&self, exe_dir: &Path) -> PathBuf {
        Self::resolve(exe_dir, &self.output_folder, "Screenshots")
    }

    pub fn video_output_dir(&self, exe_dir: &Path) -> PathBuf {
        Self::resolve(exe_dir, &self.video_output_folder, "Recordings")
    }

    pub fn is_jpg(&self) -> bool {
        self.image_format == "jpg"
    }

    /// PNG Quality (0..100) -> deflate level (0..9).
    pub fn png_level(&self) -> u8 {
        ((self.png_quality as f32) * 9.0 / 100.0).round() as u8
    }

    pub fn encoder_for_format(&self) -> &str {
        match self.video_format.as_str() {
            "webm" => &self.webm_encoder,
            "gif" => "gif",
            _ => &self.mp4_encoder,
        }
    }
}

/// v1 (milestone 1) -> v2: new default hotkeys, PNG "compression" -> "Quality".
fn migrate_v1(v: &mut serde_json::Value) {
    let Some(obj) = v.as_object_mut() else { return };
    // Only replace hotkeys the user never changed from the old defaults.
    for (key, old) in [("hotkey_fullscreen", "PrintScreen"), ("hotkey_region", "Ctrl+PrintScreen")] {
        if obj.get(key).and_then(|x| x.as_str()) == Some(old) {
            obj.remove(key);
        }
    }
    if let Some(level) = obj.remove("png_compression").and_then(|x| x.as_u64()) {
        obj.insert("png_quality".into(), serde_json::json!(((level.min(9) as f64) * 100.0 / 9.0).round() as u64));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_file_uses_defaults() {
        let s = Settings::from_json(r#"{"settings_version":2,"image_format":"JPEG","jpg_quality":500}"#).unwrap();
        assert_eq!(s.image_format, "jpg");
        assert_eq!(s.jpg_quality, 100);
        assert_eq!(s.hotkey_fullscreen, "Ctrl+F12");
        assert!(!s.setup_completed);
    }

    #[test]
    fn accepts_utf8_bom() {
        let s = Settings::from_json("\u{feff}{\"settings_version\":2,\"start_with_windows\":true}").unwrap();
        assert!(s.start_with_windows);
    }

    #[test]
    fn migrates_v1() {
        let s = Settings::from_json(
            r#"{"hotkey_fullscreen":"PrintScreen","hotkey_region":"Alt+F1","png_compression":9,"output_folder":"D:\\Shots"}"#,
        )
        .unwrap();
        assert_eq!(s.hotkey_fullscreen, "Ctrl+F12"); // old default replaced
        assert_eq!(s.hotkey_region, "Alt+F1"); // user's own choice kept
        assert_eq!(s.png_quality, 100);
        assert_eq!(s.png_level(), 9);
        assert_eq!(s.output_folder, "D:\\Shots");
        assert!(!s.setup_completed); // wizard shows once after upgrading
        assert_eq!(s.settings_version, 2);
    }

    #[test]
    fn png_quality_mapping() {
        let mut s = Settings { png_quality: 0, ..Settings::default() };
        assert_eq!(s.png_level(), 0);
        s.png_quality = 70;
        assert_eq!(s.png_level(), 6);
    }
}
