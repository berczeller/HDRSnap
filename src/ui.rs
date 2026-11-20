//! Settings window and first-run setup wizard (egui).
//!
//! Runs as a separate short-lived process (`HDRSnap.exe --settings` or
//! `--setup`), so the tray app never pays for a GUI toolkit while idle.
//! Every change is written to settings.json immediately and the tray app is
//! told to reload, so nothing needs a restart.

use crate::win::app::{MAIN_CLASS, MSG_RELOAD, MSG_SUSPEND_HOTKEYS, VERSION};
use crate::win::{audio, display, record, util};
use eframe::egui::{self, Color32, RichText};
use hdrsnap_core::encoders::{self, Container};
use hdrsnap_core::hotkey;
use hdrsnap_core::settings::Settings;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const ACCENT: Color32 = Color32::from_rgb(255, 150, 70);
const TITLE_SETTINGS: &str = "HDRSnap Settings";
const TITLE_SETUP: &str = "HDRSnap Setup";
const LABEL_W: f32 = 200.0;

/// Left-aligned, fixed-width label column.
fn fixed_label(ui: &mut egui::Ui, text: &str) {
    let h = ui.spacing().interact_size.y;
    ui.allocate_ui_with_layout(egui::vec2(LABEL_W, h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_width(LABEL_W);
        ui.add(egui::Label::new(text).wrap());
    });
}

pub fn run(exe_dir: &Path, wizard: bool) {
    unsafe {
        let m = CreateMutexW(None, false, w!("Local\\HDRSnap.SettingsWindow"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            for t in [TITLE_SETTINGS, TITLE_SETUP] {
                if let Ok(h) = FindWindowW(PCWSTR::null(), &HSTRING::from(t)) {
                    let _ = ShowWindow(h, SW_RESTORE);
                    let _ = SetForegroundWindow(h);
                }
            }
            return;
        }
        std::mem::forget(m);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    let (settings, _) = Settings::load(exe_dir);
    let title = if wizard { TITLE_SETUP } else { TITLE_SETTINGS };
    let size = if wizard { [800.0, 540.0] } else { [880.0, 620.0] };
    let mut viewport = egui::ViewportBuilder::default().with_title(title).with_inner_size(size).with_min_inner_size([600.0, 460.0]);
    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(Arc::new(icon));
    }
    let options = eframe::NativeOptions { viewport, centered: true, ..Default::default() };
    let exe_dir = exe_dir.to_path_buf();
    let _ = eframe::run_native(
        title,
        options,
        Box::new(move |cc| {
            setup_style(&cc.egui_ctx);
            Ok(Box::new(UiApp::new(exe_dir, settings, wizard)))
        }),
    );
    notify_main(MSG_SUSPEND_HOTKEYS, 0); // never leave hotkeys suspended
}

fn load_icon() -> Option<egui::IconData> {
    let bytes = include_bytes!("../assets/icon64.png");
    let dec = png::Decoder::new(std::io::Cursor::new(&bytes[..]));
    let mut r = dec.read_info().ok()?;
    let mut buf = vec![0; r.output_buffer_size()?];
    let info = r.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    Some(egui::IconData { rgba: buf, width: info.width, height: info.height })
}

fn setup_style(ctx: &egui::Context) {
    // Segoe UI to match Windows, if present.
    let mut fonts = egui::FontDefinitions::default();
    if let Ok(data) = std::fs::read("C:\\Windows\\Fonts\\segoeui.ttf") {
        fonts.font_data.insert("segoe".into(), Arc::new(egui::FontData::from_owned(data)));
        fonts.families.get_mut(&egui::FontFamily::Proportional).unwrap().insert(0, "segoe".into());
    }
    ctx.set_fonts(fonts);
    ctx.options_mut(|o| o.theme_preference = egui::ThemePreference::System);
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        ctx.style_mut_of(theme, |s| {
            use egui::{FontFamily::Proportional, FontId, TextStyle};
            s.text_styles = [
                (TextStyle::Heading, FontId::new(22.0, Proportional)),
                (TextStyle::Body, FontId::new(14.5, Proportional)),
                (TextStyle::Button, FontId::new(14.5, Proportional)),
                (TextStyle::Small, FontId::new(12.5, Proportional)),
                (TextStyle::Monospace, FontId::new(13.5, egui::FontFamily::Monospace)),
            ]
            .into();
            s.spacing.item_spacing = egui::vec2(10.0, 9.0);
            s.spacing.button_padding = egui::vec2(12.0, 5.0);
            s.spacing.slider_width = 240.0;
            s.spacing.combo_width = 260.0;
            s.visuals.selection.bg_fill = ACCENT.linear_multiply(0.85);
            s.visuals.selection.stroke.color = Color32::BLACK;
            s.visuals.hyperlink_color = ACCENT;
            s.visuals.widgets.active.bg_fill = ACCENT;
        });
    }
}

fn notify_main(msg: PCWSTR, wparam: usize) {
    unsafe {
        if let Ok(h) = FindWindowW(MAIN_CLASS, PCWSTR::null()) {
            let id = RegisterWindowMessageW(msg);
            let _ = PostMessageW(Some(h), id, WPARAM(wparam), LPARAM(0));
        }
    }
}

/// Native folder picker (IFileOpenDialog).
fn pick_folder(start: &Path) -> Option<PathBuf> {
    unsafe {
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let opts = dlg.GetOptions().ok()?;
        dlg.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        if start.is_dir() {
            if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(start.as_os_str()), None) {
                let _ = dlg.SetFolder(&item);
            }
        }
        let owner = GetForegroundWindow();
        dlg.Show(Some(owner)).ok()?;
        let item = dlg.GetResult().ok()?;
        let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s.map(PathBuf::from)
    }
}

fn open_path(p: &Path) {
    let _ = std::fs::create_dir_all(p);
    unsafe {
        ShellExecuteW(Some(HWND::default()), w!("open"), &HSTRING::from(p.as_os_str()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    General,
    Hotkeys,
    Screenshots,
    Recording,
    Hdr,
    About,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HkField {
    Full,
    Region,
    RecFull,
    RecRegion,
}

impl HkField {
    const ALL: [HkField; 4] = [HkField::Region, HkField::Full, HkField::RecRegion, HkField::RecFull];
    fn label(self) -> &'static str {
        match self {
            HkField::Full => "Capture full screen",
            HkField::Region => "Capture region",
            HkField::RecFull => "Start / stop recording (full screen)",
            HkField::RecRegion => "Start / stop recording (region)",
        }
    }
    fn get(self, s: &mut Settings) -> &mut String {
        match self {
            HkField::Full => &mut s.hotkey_fullscreen,
            HkField::Region => &mut s.hotkey_region,
            HkField::RecFull => &mut s.hotkey_record_fullscreen,
            HkField::RecRegion => &mut s.hotkey_record_region,
        }
    }
}

/// Ready-made hotkey sets. Order of `keys`: region, full screen,
/// record region, record full screen (same as `HkField::ALL`).
struct HkPreset {
    name: &'static str,
    keys: [&'static str; 4],
}

const HK_PRESETS: &[HkPreset] = &[
    HkPreset { name: "HDRSnap (F12)", keys: ["Ctrl+Alt+F12", "Ctrl+F12", "Ctrl+Alt+Shift+F12", "Ctrl+Shift+F12"] },
    HkPreset {
        name: "ShareX style (Print Screen = full screen)",
        keys: ["Ctrl+PrintScreen", "PrintScreen", "Shift+PrintScreen", "Ctrl+Shift+PrintScreen"],
    },
    HkPreset {
        name: "Snipping Tool style (Print Screen = region)",
        keys: ["PrintScreen", "Ctrl+PrintScreen", "Shift+PrintScreen", "Ctrl+Shift+PrintScreen"],
    },
];

impl HkField {
    fn index(self) -> usize {
        HkField::ALL.iter().position(|f| *f == self).unwrap_or(0)
    }

    /// Common alternatives offered in the dropdown (every preset's key for
    /// this action, plus a few extras).
    fn choices(self) -> Vec<&'static str> {
        let mut v: Vec<&'static str> = HK_PRESETS.iter().map(|p| p.keys[self.index()]).collect();
        v.extend_from_slice(match self {
            HkField::Region => &["Alt+Shift+F12"][..],
            HkField::Full => &["Alt+F12"][..],
            HkField::RecRegion => &["Ctrl+Alt+Shift+R"][..],
            HkField::RecFull => &["Ctrl+Alt+Shift+F"][..],
        });
        let mut seen = Vec::new();
        v.retain(|k| {
            let new = !seen.contains(k);
            seen.push(*k);
            new
        });
        v
    }
}

/// Same key combination (ignores spelling differences like "PrtSc").
fn same_hotkey(a: &str, b: &str) -> bool {
    match (hotkey::parse(a), hotkey::parse(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a.trim().is_empty() && b.trim().is_empty(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Probe {
    Checking,
    Yes,
    No,
}

struct MonitorRow {
    name: String,
    w: i32,
    h: i32,
    hdr: bool,
    sdr_nits: f32,
    peak: f32,
}

struct UiApp {
    exe_dir: PathBuf,
    s: Settings,
    saved: Settings,
    page: Page,
    wizard: Option<usize>,
    wizard_from_settings: bool,
    listening: Option<HkField>,
    probes: Arc<Mutex<HashMap<&'static str, Probe>>>,
    ffmpeg: Option<PathBuf>,
    ffmpeg_version: Arc<Mutex<String>>,
    monitors: Vec<MonitorRow>,
    /// Microphones as (endpoint ID, name).
    mics: Vec<(String, String)>,
    /// Full hotkey editor open for this field (wizard / hotkeys page).
    hk_custom: [bool; 4],
    /// Windows gives Print Screen to the Snipping Tool.
    prtsc_snipping: bool,
    last_change: Option<Instant>,
    finished: bool,
}

impl UiApp {
    fn new(exe_dir: PathBuf, s: Settings, wizard: bool) -> Self {
        let ffmpeg = record::ffmpeg_path(&exe_dir);
        let probes: Arc<Mutex<HashMap<&'static str, Probe>>> = Arc::new(Mutex::new(HashMap::new()));
        let ffmpeg_version = Arc::new(Mutex::new(String::new()));
        if let Some(ff) = &ffmpeg {
            for e in encoders::ENCODERS {
                probes.lock().unwrap().insert(e.id, Probe::Checking);
            }
            // Probe encoders in the background (a few at a time).
            for chunk in encoders::ENCODERS.chunks(4) {
                let ids: Vec<&'static str> = chunk.iter().map(|e| e.id).collect();
                let (ff, probes) = (ff.clone(), probes.clone());
                std::thread::spawn(move || {
                    for id in ids {
                        let ok = record::encoder_available(&ff, id);
                        probes.lock().unwrap().insert(id, if ok { Probe::Yes } else { Probe::No });
                    }
                });
            }
            let (ff, ver) = (ff.clone(), ffmpeg_version.clone());
            std::thread::spawn(move || {
                use std::os::windows::process::CommandExt;
                if let Ok(o) = std::process::Command::new(ff).arg("-version").creation_flags(0x0800_0000).output() {
                    let text = String::from_utf8_lossy(&o.stdout);
                    *ver.lock().unwrap() = text.lines().next().unwrap_or("").to_string();
                }
            });
        }
        let monitors = display::enumerate()
            .map(|ms| {
                ms.iter()
                    .map(|m| MonitorRow {
                        name: if m.friendly_name.is_empty() {
                            m.device_name.clone()
                        } else {
                            format!("{} ({})", m.friendly_name, m.device_name)
                        },
                        w: m.width(),
                        h: m.height(),
                        hdr: m.hdr,
                        sdr_nits: m.sdr_white_scale * 80.0,
                        peak: m.max_nits,
                    })
                    .collect()
            })
            .unwrap_or_default();
        // The wizard starts from the current hotkeys; presets are one click away.
        let mut s = s;
        if s.start_with_windows && util::autostart_disabled_by_windows() {
            s.start_with_windows = false; // switched off in Windows' Startup apps
        }
        // `--page=<name>` / `--step=<n>` open a specific page (handy for docs).
        let arg = |k: &str| std::env::args().find_map(|a| a.strip_prefix(k).map(String::from));
        let page = match arg("--page=").as_deref() {
            Some("hotkeys") => Page::Hotkeys,
            Some("screenshots") => Page::Screenshots,
            Some("recording") => Page::Recording,
            Some("hdr") => Page::Hdr,
            Some("about") => Page::About,
            _ => Page::General,
        };
        let step = arg("--step=").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0).min(3);
        Self {
            exe_dir,
            saved: s.clone(),
            s,
            page,
            wizard: if wizard { Some(step) } else { None },
            wizard_from_settings: false,
            listening: None,
            probes,
            ffmpeg,
            ffmpeg_version,
            monitors,
            // own thread: COM there is MTA, whatever the UI thread uses
            mics: std::thread::spawn(audio::list_microphones).join().unwrap_or_default(),
            hk_custom: [false; 4],
            prtsc_snipping: util::print_screen_opens_snipping_tool(),
            last_change: None,
            finished: false,
        }
    }

    fn max_width(&self) -> u32 {
        self.monitors.iter().map(|m| m.w.max(0) as u32).max().unwrap_or(1920)
    }

    fn save_now(&mut self) {
        self.s = self.s.clone().sanitized();
        if let Err(e) = self.s.save(&self.exe_dir) {
            eprintln!("save failed: {e}");
        }
        self.saved = self.s.clone();
        notify_main(MSG_RELOAD, 0);
    }

    fn set_listening(&mut self, f: Option<HkField>) {
        if self.listening.is_none() && f.is_some() {
            notify_main(MSG_SUSPEND_HOTKEYS, 1);
        }
        if self.listening.is_some() && f.is_none() {
            notify_main(MSG_SUSPEND_HOTKEYS, 0);
        }
        self.listening = f;
    }

    fn handle_hotkey_input(&mut self, ctx: &egui::Context) {
        let Some(field) = self.listening else { return };
        let events = ctx.input(|i| i.events.clone());
        for ev in events {
            if let egui::Event::Key { key, pressed: true, modifiers, .. } = ev {
                if key == egui::Key::Escape && !modifiers.any() {
                    self.set_listening(None);
                    return;
                }
                if let Some(name) = key_name(key) {
                    let mut parts = Vec::new();
                    if modifiers.ctrl || modifiers.command {
                        parts.push("Ctrl".to_string());
                    }
                    if modifiers.alt {
                        parts.push("Alt".to_string());
                    }
                    if modifiers.shift {
                        parts.push("Shift".to_string());
                    }
                    parts.push(name);
                    *field.get(&mut self.s) = parts.join("+");
                    self.set_listening(None);
                    return;
                }
            }
        }
    }
}

impl eframe::App for UiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_hotkey_input(ctx);
        if self.wizard.is_some() {
            self.wizard_ui(ctx);
        } else {
            self.settings_ui(ctx);
            // Live apply (debounced while dragging sliders).
            if self.s != self.saved {
                let t = *self.last_change.get_or_insert_with(Instant::now);
                if t.elapsed() > Duration::from_millis(250) {
                    self.save_now();
                    self.last_change = None;
                } else {
                    ctx.request_repaint_after(Duration::from_millis(260));
                }
            }
        }
        // keep encoder probe results flowing in
        if self.probes.lock().unwrap().values().any(|p| *p == Probe::Checking) {
            ctx.request_repaint_after(Duration::from_millis(300));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.wizard.is_none() && self.s != self.saved {
            self.save_now();
        }
        if self.wizard.is_some() && !self.wizard_from_settings && !self.finished {
            // Closing the first-run wizard counts as "skip": don't ask again.
            let (mut s, _) = Settings::load(&self.exe_dir);
            s.setup_completed = true;
            self.s = s;
            self.save_now();
        }
        if self.listening.is_some() {
            notify_main(MSG_SUSPEND_HOTKEYS, 0);
        }
    }
}

// ---------------------------------------------------------------------------
// Widgets

fn toggle(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let size = ui.spacing().interact_size.y * egui::vec2(2.0, 1.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(resp.id, *on);
        let vis = ui.style().interact_selectable(&resp, *on);
        let rect = rect.expand(vis.expansion);
        let r = 0.5 * rect.height();
        let fill = if *on { ACCENT } else { vis.bg_fill };
        ui.painter().rect(rect, r, fill, vis.bg_stroke, egui::StrokeKind::Inside);
        let x = egui::lerp((rect.left() + r)..=(rect.right() - r), t);
        ui.painter().circle(egui::pos2(x, rect.center().y), 0.72 * r, Color32::WHITE, egui::Stroke::NONE);
    }
    resp
}

/// One labelled setting row with an optional hint underneath.
fn row(ui: &mut egui::Ui, label: &str, hint: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        fixed_label(ui, label);
        add(ui);
    });
    if !hint.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(LABEL_W + ui.spacing().item_spacing.x);
            let w = ui.available_width();
            ui.allocate_ui_with_layout(egui::vec2(w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.add(egui::Label::new(RichText::new(hint).small().weak()).wrap());
            });
        });
    }
    ui.add_space(4.0);
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(title).strong().size(16.0));
    ui.separator();
}

fn combo<T: PartialEq + Clone>(ui: &mut egui::Ui, id: &str, value: &mut T, options: &[(T, &str)]) {
    let current = options.iter().find(|(v, _)| v == value).map(|(_, l)| *l).unwrap_or("?");
    egui::ComboBox::from_id_salt(id).selected_text(current).show_ui(ui, |ui| {
        for (v, l) in options {
            ui.selectable_value(value, v.clone(), *l);
        }
    });
}

fn key_name(k: egui::Key) -> Option<String> {
    use egui::Key::*;
    let s = match k {
        ArrowUp => "Up".into(),
        ArrowDown => "Down".into(),
        ArrowLeft => "Left".into(),
        ArrowRight => "Right".into(),
        Num0 | Num1 | Num2 | Num3 | Num4 | Num5 | Num6 | Num7 | Num8 | Num9 => k.name().to_string(),
        Insert => "Insert".into(),
        Delete => "Delete".into(),
        Home => "Home".into(),
        End => "End".into(),
        PageUp => "PageUp".into(),
        PageDown => "PageDown".into(),
        Space => "Space".into(),
        Tab => "Tab".into(),
        Enter => "Enter".into(),
        Backspace => "Backspace".into(),
        Minus => "-".into(),
        Equals => "=".into(),
        Comma => ",".into(),
        Period => ".".into(),
        Slash => "/".into(),
        Backslash => "\\".into(),
        Semicolon => ";".into(),
        OpenBracket => "[".into(),
        CloseBracket => "]".into(),
        Backtick => "`".into(),
        _ => {
            let n = k.name();
            if hotkey::parse(n).is_some() {
                n.to_string()
            } else {
                return None;
            }
        }
    };
    Some(s)
}

const KEY_CHOICES: &[&str] = &[
    "F1",
    "F2",
    "F3",
    "F4",
    "F5",
    "F6",
    "F7",
    "F8",
    "F9",
    "F10",
    "F11",
    "F12",
    "F13",
    "F14",
    "F15",
    "F16",
    "F17",
    "F18",
    "F19",
    "F20",
    "F21",
    "F22",
    "F23",
    "F24",
    "PrintScreen",
    "Pause",
    "ScrollLock",
    "Insert",
    "Delete",
    "Home",
    "End",
    "PageUp",
    "PageDown",
    "A",
    "B",
    "C",
    "D",
    "E",
    "F",
    "G",
    "H",
    "I",
    "J",
    "K",
    "L",
    "M",
    "N",
    "O",
    "P",
    "Q",
    "R",
    "S",
    "T",
    "U",
    "V",
    "W",
    "X",
    "Y",
    "Z",
    "0",
    "1",
    "2",
    "3",
    "4",
    "5",
    "6",
    "7",
    "8",
    "9",
    "Num0",
    "Num1",
    "Num2",
    "Num3",
    "Num4",
    "Num5",
    "Num6",
    "Num7",
    "Num8",
    "Num9",
    "Space",
    "Up",
    "Down",
    "Left",
    "Right",
];

impl UiApp {
    /// "Preset" dropdown that sets all four shortcuts at once.
    fn hotkey_preset_row(&mut self, ui: &mut egui::Ui) {
        let current: Vec<String> = HkField::ALL.iter().map(|f| f.get(&mut self.s).clone()).collect();
        let matched = HK_PRESETS.iter().position(|p| (0..4).all(|i| same_hotkey(p.keys[i], &current[i])));
        let mut pick = None;
        row(ui, "Preset", "Sets all four shortcuts at once. You can still change each one below.", |ui| {
            let sel = matched.map(|i| HK_PRESETS[i].name).unwrap_or("Your own combination");
            egui::ComboBox::from_id_salt("hkpreset").width(320.0).selected_text(sel).show_ui(ui, |ui| {
                for (i, p) in HK_PRESETS.iter().enumerate() {
                    if ui.selectable_label(matched == Some(i), p.name).clicked() {
                        pick = Some(i);
                    }
                }
            });
        });
        if let Some(i) = pick {
            for (f, k) in HkField::ALL.iter().zip(HK_PRESETS[i].keys) {
                *f.get(&mut self.s) = k.to_string();
            }
            self.hk_custom = [false; 4];
            self.set_listening(None);
        }
    }

    /// One shortcut: dropdown of common combinations, "Custom…" opens the
    /// full editor (modifier toggles, any key, press-to-record).
    fn hotkey_choice_row(&mut self, ui: &mut egui::Ui, field: HkField) {
        let i = field.index();
        let cur = field.get(&mut self.s).clone();
        let choices = field.choices();
        let label = |k: &str| hotkey::parse(k).map(hotkey::format).unwrap_or_else(|| k.to_string());
        let mut new_value: Option<String> = None;
        let mut open_custom = false;
        ui.horizontal(|ui| {
            fixed_label(ui, field.label());
            let sel = if cur.trim().is_empty() { "Disabled".to_string() } else { label(&cur) };
            egui::ComboBox::from_id_salt(("hkchoice", i)).width(220.0).selected_text(sel).show_ui(ui, |ui| {
                if !cur.trim().is_empty() && !choices.iter().any(|c| same_hotkey(c, &cur)) {
                    let _ = ui.selectable_label(true, format!("{}  (current)", label(&cur)));
                }
                for c in &choices {
                    let selected = same_hotkey(c, &cur);
                    let text = if selected { format!("{}  (current)", label(c)) } else { label(c) };
                    if ui.selectable_label(selected, text).clicked() {
                        new_value = Some(c.to_string());
                    }
                }
                if ui.selectable_label(cur.trim().is_empty(), "Disabled").clicked() {
                    new_value = Some(String::new());
                }
                ui.separator();
                if ui.selectable_label(false, "Custom…").clicked() {
                    open_custom = true;
                }
            });
            if !self.hk_custom[i] && ui.small_button("Custom…").clicked() {
                open_custom = true;
            }
        });
        if let Some(v) = new_value {
            *field.get(&mut self.s) = v;
        }
        if open_custom {
            self.hk_custom[i] = true;
        }
        if self.hk_custom[i] {
            ui.horizontal(|ui| {
                ui.add_space(LABEL_W + ui.spacing().item_spacing.x);
                self.hotkey_editor(ui, field);
            });
        } else if !cur.trim().is_empty() && HkField::ALL.iter().any(|f| *f != field && same_hotkey(f.get(&mut self.s), &cur)) {
            ui.horizontal(|ui| {
                ui.add_space(LABEL_W + ui.spacing().item_spacing.x);
                ui.label(RichText::new("Same as another HDRSnap hotkey").small().color(Color32::from_rgb(230, 90, 80)));
            });
        }
    }

    /// Windows 11 can give the plain Print Screen key to the Snipping Tool.
    fn print_screen_warning(&mut self, ui: &mut egui::Ui) {
        let uses_prtsc = HkField::ALL.iter().any(|f| hotkey::parse(f.get(&mut self.s)).map(|h| h.vk == 0x2C).unwrap_or(false));
        if uses_prtsc && self.prtsc_snipping {
            ui.add_space(4.0);
            ui.label(
                RichText::new("Windows is set to open the Snipping Tool with Print Screen, so it may take the key before HDRSnap does. Turn off \"Use the Print screen key to open screen capture\" in Windows' keyboard settings.")
                    .small()
                    .color(Color32::from_rgb(230, 150, 60)),
            );
            if ui.button("Open Windows keyboard settings").clicked() {
                unsafe {
                    ShellExecuteW(None, w!("open"), w!("ms-settings:easeofaccess-keyboard"), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
                }
            }
        }
    }

    /// Modifier toggles + key dropdown + "press keys" capture.
    fn hotkey_editor(&mut self, ui: &mut egui::Ui, field: HkField) {
        let text = field.get(&mut self.s).clone();
        let parsed = hotkey::parse(&text);
        let (mut ctrl, mut alt, mut shift, mut win) = parsed
            .map(|h| {
                (
                    h.mods & hotkey::MOD_CONTROL != 0,
                    h.mods & hotkey::MOD_ALT != 0,
                    h.mods & hotkey::MOD_SHIFT != 0,
                    h.mods & hotkey::MOD_WIN != 0,
                )
            })
            .unwrap_or_default();
        let mut key = parsed.map(|h| hotkey::format(hotkey::Hotkey { mods: 0, vk: h.vk })).unwrap_or_default();
        let before = (ctrl, alt, shift, win, key.clone());
        let listening = self.listening == Some(field);

        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.toggle_value(&mut ctrl, "Ctrl");
                ui.toggle_value(&mut alt, "Alt");
                ui.toggle_value(&mut shift, "Shift");
                ui.toggle_value(&mut win, "Win");
                ui.label("+");
                egui::ComboBox::from_id_salt(("key", field as u8))
                    .width(120.0)
                    .selected_text(if key.is_empty() { "None" } else { key.as_str() })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut key, String::new(), "None (disabled)");
                        for k in KEY_CHOICES {
                            ui.selectable_value(&mut key, k.to_string(), *k);
                        }
                    });
                let btn = if listening { RichText::new("Press keys… (Esc cancels)").color(ACCENT) } else { RichText::new("Record") };
                if ui.button(btn).clicked() {
                    self.set_listening(if listening { None } else { Some(field) });
                }
            });
            // warnings
            let current = field.get(&mut self.s).clone();
            if !current.is_empty() {
                let dup = HkField::ALL.iter().any(|f| {
                    *f != field
                        && hotkey::parse(f.get(&mut self.s)).is_some()
                        && hotkey::parse(f.get(&mut self.s)) == hotkey::parse(&current)
                });
                if dup {
                    ui.label(RichText::new("Same as another HDRSnap hotkey").small().color(Color32::from_rgb(230, 90, 80)));
                } else if hotkey::parse(&current).map(|h| h.mods == 0).unwrap_or(false) {
                    ui.label(RichText::new("No modifier: this key will stop working in other apps").small().weak());
                }
            } else {
                ui.label(RichText::new("Disabled").small().weak());
            }
        });

        if (ctrl, alt, shift, win, key.clone()) != before {
            let mut parts = Vec::new();
            if ctrl {
                parts.push("Ctrl");
            }
            if alt {
                parts.push("Alt");
            }
            if shift {
                parts.push("Shift");
            }
            if win {
                parts.push("Win");
            }
            *field.get(&mut self.s) =
                if key.is_empty() { String::new() } else { parts.into_iter().map(String::from).chain([key]).collect::<Vec<_>>().join("+") };
        }
    }

    fn folder_row(&mut self, ui: &mut egui::Ui, label: &str, video: bool) {
        let resolved = if video { self.s.video_output_dir(&self.exe_dir) } else { self.s.output_dir(&self.exe_dir) };
        row(ui, label, "", |ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(resolved.display().to_string()).monospace());
                ui.horizontal(|ui| {
                    if ui.button("Browse…").clicked() {
                        if let Some(p) = pick_folder(&resolved) {
                            let v = p.display().to_string();
                            if video {
                                self.s.video_output_folder = v;
                            } else {
                                self.s.output_folder = v;
                            }
                        }
                    }
                    if ui.button("Open").clicked() {
                        open_path(&resolved);
                    }
                    if ui.button("Reset").on_hover_text("Use the folder next to HDRSnap.exe").clicked() {
                        if video {
                            self.s.video_output_folder.clear();
                        } else {
                            self.s.output_folder.clear();
                        }
                    }
                });
            });
        });
    }

    fn image_format_rows(&mut self, ui: &mut egui::Ui) {
        row(ui, "Image format", "", |ui| {
            combo(
                ui,
                "imgfmt",
                &mut self.s.image_format,
                &[("png".to_string(), "PNG (lossless)"), ("jpg".to_string(), "JPG (smaller files)")],
            );
        });
        if self.s.is_jpg() {
            row(ui, "Quality", "Higher = sharper image, larger files. 90+ keeps text crisp.", |ui| {
                ui.add(egui::Slider::new(&mut self.s.jpg_quality, 1..=100));
            });
        } else {
            row(ui, "Quality", "PNG is always lossless. Higher Quality = smaller files, a little slower to save.", |ui| {
                ui.add(egui::Slider::new(&mut self.s.png_quality, 0..=100));
            });
        }
    }

    fn encoder_label(&self, id: &str) -> String {
        let p = self.probes.lock().unwrap().get(id).copied();
        let name = encoders::info(id).map(|e| e.label).unwrap_or(id);
        match p {
            Some(Probe::Checking) => format!("{name}  (checking…)"),
            Some(Probe::No) => format!("{name}  (not available)"),
            _ => name.to_string(),
        }
    }

    fn auto_choice(&self, c: Container) -> Option<&'static str> {
        let probes = self.probes.lock().unwrap();
        encoders::auto_candidates(c, self.max_width()).into_iter().find(|id| probes.get(id) == Some(&Probe::Yes))
    }

    fn video_rows(&mut self, ui: &mut egui::Ui) {
        if self.ffmpeg.is_none() {
            ui.label(
                RichText::new("ffmpeg.exe was not found next to HDRSnap.exe, so recording is disabled. It ships in the release zip.")
                    .color(Color32::from_rgb(230, 90, 80)),
            );
        }
        row(ui, "Video format", "", |ui| {
            combo(
                ui,
                "vidfmt",
                &mut self.s.video_format,
                &[("mp4".to_string(), "MP4"), ("webm".to_string(), "WebM"), ("gif".to_string(), "GIF (animated)")],
            );
        });
        let container = Container::from_setting(&self.s.video_format);
        if container != Container::Gif {
            let current = self.s.encoder_for_format().to_string();
            let auto = self.auto_choice(container);
            let auto_label = match auto {
                Some(id) => format!("Automatic: {}", encoders::info(id).map(|e| e.label).unwrap_or(id)),
                None => "Automatic (best available)".into(),
            };
            let mut chosen = current.clone();
            row(ui, "Encoder", "Hardware encoders (NVENC / AMF / Quick Sync) are fastest and barely use the CPU.", |ui| {
                let sel = if chosen == "auto" { auto_label.clone() } else { self.encoder_label(&chosen) };
                egui::ComboBox::from_id_salt("encoder").width(320.0).selected_text(sel).show_ui(ui, |ui| {
                    ui.selectable_value(&mut chosen, "auto".to_string(), auto_label.as_str());
                    for e in encoders::encoders_for(container) {
                        let available = self.probes.lock().unwrap().get(e.id) != Some(&Probe::No);
                        ui.add_enabled_ui(available, |ui| {
                            ui.selectable_value(&mut chosen, e.id.to_string(), self.encoder_label(e.id));
                        });
                    }
                });
            });
            if let Some(info) = encoders::info(&chosen) {
                if self.max_width() > info.max_width {
                    ui.horizontal(|ui| {
                        ui.add_space(LABEL_W + 10.0);
                        ui.label(
                            RichText::new(format!(
                                "{} is limited to {} px wide; full screen on your {} px display needs HEVC or AV1.",
                                info.label,
                                info.max_width,
                                self.max_width()
                            ))
                            .small()
                            .color(Color32::from_rgb(230, 150, 60)),
                        );
                    });
                }
            }
            if chosen != current {
                if container == Container::Webm {
                    self.s.webm_encoder = chosen;
                } else {
                    self.s.mp4_encoder = chosen;
                }
            }
            row(ui, "Quality", "Higher = better image, larger files.", |ui| {
                ui.add(egui::Slider::new(&mut self.s.video_quality, 1..=100));
            });
            row(ui, "Frame rate", "", |ui| {
                for f in [30u32, 60, 120] {
                    ui.selectable_value(&mut self.s.video_fps, f, format!("{f}"));
                }
                ui.add(egui::DragValue::new(&mut self.s.video_fps).range(1..=240).suffix(" fps"));
            });
        } else {
            row(ui, "Quality", "Higher = more colours and smoother gradients, larger files.", |ui| {
                ui.add(egui::Slider::new(&mut self.s.gif_quality, 1..=100));
            });
            row(ui, "Frame rate", "GIFs above 30 fps get large quickly.", |ui| {
                ui.add(egui::Slider::new(&mut self.s.gif_fps, 5..=50).suffix(" fps"));
            });
            row(ui, "Max width", "0 = keep the original size.", |ui| {
                ui.add(egui::DragValue::new(&mut self.s.gif_max_width).range(0..=7680).suffix(" px"));
            });
        }
    }

    // -----------------------------------------------------------------------

    fn settings_ui(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("nav")
            .resizable(false)
            .exact_width(190.0)
            .frame(egui::Frame::side_top_panel(&ctx.style()).inner_margin(egui::Margin::symmetric(12, 8)))
            .show(ctx, |ui| {
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    ui.add_space(6.0);
                    ui.label(RichText::new("HDRSnap").heading().strong());
                });
                ui.add_space(14.0);
                for (p, name) in [
                    (Page::General, "General"),
                    (Page::Hotkeys, "Hotkeys"),
                    (Page::Screenshots, "Screenshots"),
                    (Page::Recording, "Recording"),
                    (Page::Hdr, "HDR"),
                    (Page::About, "About"),
                ] {
                    let sel = self.page == p;
                    let text = RichText::new(name).size(15.5);
                    let resp = ui.add_sized([164.0, 34.0], egui::Button::selectable(sel, text));
                    if resp.clicked() {
                        self.page = p;
                    }
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    ui.add_space(10.0);
                    ui.label(RichText::new("Changes apply instantly").small().weak());
                });
            });

        egui::CentralPanel::default().frame(egui::Frame::central_panel(&ctx.style()).inner_margin(egui::Margin::symmetric(22, 12))).show(
            ctx,
            |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.add_space(6.0);
                    match self.page {
                        Page::General => self.page_general(ui),
                        Page::Hotkeys => self.page_hotkeys(ui),
                        Page::Screenshots => self.page_screenshots(ui),
                        Page::Recording => self.page_recording(ui),
                        Page::Hdr => self.page_hdr(ui),
                        Page::About => self.page_about(ui),
                    }
                    ui.add_space(12.0);
                });
            },
        );
    }

    fn page_general(&mut self, ui: &mut egui::Ui) {
        section(ui, "Output");
        self.folder_row(ui, "Screenshots folder", false);
        self.folder_row(ui, "Recordings folder", true);
        row(ui, "Screenshot file name", "Saved as <name>_YYYY-MM-DD_HH-MM-SS", |ui| {
            ui.add(egui::TextEdit::singleline(&mut self.s.filename_prefix).desired_width(200.0));
        });
        row(ui, "Recording file name", "", |ui| {
            ui.add(egui::TextEdit::singleline(&mut self.s.video_filename_prefix).desired_width(200.0));
        });
        section(ui, "Behaviour");
        row(ui, "Copy screenshots to clipboard", "", |ui| {
            toggle(ui, &mut self.s.copy_to_clipboard);
        });
        row(ui, "Show notifications", "", |ui| {
            toggle(ui, &mut self.s.show_notification);
        });
        row(ui, "Start with Windows", "Starts HDRSnap in the tray when you sign in. Also in the tray menu. If you switch it off in Task Manager > Startup apps, this follows.", |ui| {
            toggle(ui, &mut self.s.start_with_windows);
        });
        section(ui, "Setup");
        row(ui, "Setup wizard", "Walk through hotkeys, folders and formats again.", |ui| {
            if ui.button("Run setup wizard…").clicked() {
                self.wizard = Some(0);
                self.wizard_from_settings = true;
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Title(TITLE_SETUP.into()));
            }
        });
    }

    fn page_hotkeys(&mut self, ui: &mut egui::Ui) {
        section(ui, "Hotkeys");
        ui.label(RichText::new("Pick a preset, or click Record and press a key combination. Recording hotkeys start and stop.").weak());
        ui.add_space(6.0);
        self.hotkey_preset_row(ui);
        for f in HkField::ALL {
            ui.horizontal(|ui| {
                fixed_label(ui, f.label());
                self.hotkey_editor(ui, f);
            });
            ui.add_space(8.0);
        }
        self.print_screen_warning(ui);
        ui.add_space(4.0);
        if ui.button("Restore default hotkeys").clicked() {
            let d = Settings::default();
            self.s.hotkey_fullscreen = d.hotkey_fullscreen;
            self.s.hotkey_region = d.hotkey_region;
            self.s.hotkey_record_fullscreen = d.hotkey_record_fullscreen;
            self.s.hotkey_record_region = d.hotkey_record_region;
        }
    }

    fn page_screenshots(&mut self, ui: &mut egui::Ui) {
        section(ui, "Format");
        self.image_format_rows(ui);
        section(ui, "Capture");
        row(ui, "Full screen captures", "", |ui| {
            combo(
                ui,
                "fstarget",
                &mut self.s.fullscreen_target,
                &[("all_monitors".to_string(), "All monitors"), ("cursor_monitor".to_string(), "Monitor under the cursor")],
            );
        });
        row(ui, "Include mouse cursor", "", |ui| {
            toggle(ui, &mut self.s.include_cursor_screenshot);
        });
    }

    fn page_recording(&mut self, ui: &mut egui::Ui) {
        section(ui, "Video");
        self.video_rows(ui);
        section(ui, "Capture");
        row(ui, "Full screen recording", "Recordings cover one monitor.", |ui| {
            combo(
                ui,
                "rectarget",
                &mut self.s.record_target,
                &[("cursor_monitor".to_string(), "Monitor under the cursor"), ("primary".to_string(), "Primary monitor")],
            );
        });
        row(ui, "Record mouse cursor", "", |ui| {
            toggle(ui, &mut self.s.record_cursor);
        });
        row(ui, "Show border around region", "Red frame outside the recorded area. It never appears in the video.", |ui| {
            toggle(ui, &mut self.s.show_record_border);
        });
        self.audio_rows(ui);
    }

    fn audio_rows(&mut self, ui: &mut egui::Ui) {
        section(ui, "Audio");
        if Container::from_setting(&self.s.video_format) == Container::Gif {
            ui.label(RichText::new("GIFs have no sound. Pick MP4 or WebM to record audio.").weak());
            return;
        }
        row(ui, "Record system audio", "Everything the PC plays through the default output device.", |ui| {
            toggle(ui, &mut self.s.record_system_audio);
        });
        row(ui, "Record microphone", "", |ui| {
            toggle(ui, &mut self.s.record_microphone);
        });
        if self.s.record_microphone {
            row(ui, "Microphone", "", |ui| {
                let name = |id: &str| self.mics.iter().find(|(i, _)| i == id).map(|(_, n)| n.clone());
                let sel = if self.s.microphone_device.is_empty() {
                    "Windows default".to_string()
                } else {
                    name(&self.s.microphone_device).unwrap_or_else(|| "(not connected) Windows default".into())
                };
                egui::ComboBox::from_id_salt("mic").width(320.0).selected_text(sel).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.s.microphone_device, String::new(), "Windows default");
                    for (id, n) in &self.mics {
                        ui.selectable_value(&mut self.s.microphone_device, id.clone(), n);
                    }
                });
            });
        }
        if self.s.record_system_audio && self.s.record_microphone {
            row(
                ui,
                "Audio tracks",
                "Separate tracks keep system audio and microphone apart inside the video, so an editor can adjust each one. Most players only play the first track (system audio).",
                |ui| {
                    combo(
                        ui,
                        "audiotracks",
                        &mut self.s.audio_tracks,
                        &[("mixed".to_string(), "Mixed into one track"), ("separate".to_string(), "Separate tracks")],
                    );
                },
            );
        }
    }

    fn page_hdr(&mut self, ui: &mut egui::Ui) {
        section(ui, "HDR to SDR");
        row(ui, "Highlight handling", "Applies to screenshots and recordings from HDR monitors.", |ui| {
            combo(
                ui,
                "tm",
                &mut self.s.tonemap_mode,
                &[("auto".to_string(), "Smooth roll-off (recommended)"), ("clip".to_string(), "Clip at SDR white")],
            );
        });
        let rolloff = self.s.tonemap_mode != "clip";
        row(
            ui,
            "Roll-off start",
            "Lower keeps more highlight detail but dims whites in HDR scenes. Desktop-only captures are never affected.",
            |ui| {
                ui.add_enabled(rolloff, egui::Slider::new(&mut self.s.tonemap_knee, 0.5..=0.98).fixed_decimals(2));
            },
        );
        row(ui, "Highlight whitening", "How much the brightest highlights fade towards white.", |ui| {
            ui.add_enabled(rolloff, egui::Slider::new(&mut self.s.highlight_desaturation, 0.0..=1.0).fixed_decimals(2));
        });
        section(ui, "Displays");
        for m in &self.monitors {
            ui.horizontal(|ui| {
                fixed_label(ui, &m.name);
                if m.hdr {
                    ui.label(RichText::new("HDR").strong().color(ACCENT));
                    ui.label(format!("{}×{} · SDR content {:.0} nits · peak {:.0} nits", m.w, m.h, m.sdr_nits, m.peak));
                } else {
                    ui.label(format!("SDR · {}×{}", m.w, m.h));
                }
            });
        }
        section(ui, "Diagnostics");
        row(ui, "Save raw HDR frame (.exr)", "Also saves the untouched HDR capture next to each screenshot.", |ui| {
            toggle(ui, &mut self.s.save_raw_hdr_exr);
        });
        row(ui, "GPU self test", "Checks the recording tone mapper against the screenshot one.", |ui| {
            if ui.button("Run").clicked() {
                if let Ok(exe) = std::env::current_exe() {
                    let _ = std::process::Command::new(exe).arg("--selftest-gpu").spawn();
                }
            }
        });
    }

    fn page_about(&mut self, ui: &mut egui::Ui) {
        section(ui, "About");
        ui.label(RichText::new(format!("HDRSnap {VERSION}")).strong());
        ui.label("Portable screenshot and screen recording tool that handles HDR correctly.");
        ui.add_space(8.0);
        let ver = self.ffmpeg_version.lock().unwrap().clone();
        match &self.ffmpeg {
            Some(p) => {
                ui.label(format!("FFmpeg: {}", p.display()));
                if !ver.is_empty() {
                    ui.label(RichText::new(ver).small().weak());
                }
            }
            None => {
                ui.label("FFmpeg: not found (recording disabled)");
            }
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Open settings folder").clicked() {
                open_path(&self.exe_dir);
            }
            if ui.button("Open log").clicked() {
                let p = self.exe_dir.join("hdrsnap.log");
                unsafe {
                    ShellExecuteW(None, w!("open"), w!("notepad.exe"), &HSTRING::from(p.as_os_str()), PCWSTR::null(), SW_SHOWNORMAL);
                }
            }
        });
    }

    // -----------------------------------------------------------------------
    // Wizard

    fn wizard_ui(&mut self, ctx: &egui::Context) {
        let step = self.wizard.unwrap_or(0);
        const STEPS: [&str; 4] = ["Hotkeys", "Folders & startup", "Screenshots", "Recording"];

        egui::TopBottomPanel::top("wiz_top")
            .frame(egui::Frame::side_top_panel(&ctx.style()).inner_margin(egui::Margin::symmetric(22, 6)))
            .show(ctx, |ui| {
                ui.add_space(12.0);
                ui.label(RichText::new("Welcome to HDRSnap").heading().strong());
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    for (i, name) in STEPS.iter().enumerate() {
                        let (col, txt) = if i == step {
                            (ACCENT, RichText::new(format!("{}  {}", i + 1, name)).strong().color(ACCENT))
                        } else if i < step {
                            (ui.visuals().text_color(), RichText::new(format!("{}  {}", i + 1, name)))
                        } else {
                            (ui.visuals().weak_text_color(), RichText::new(format!("{}  {}", i + 1, name)).weak())
                        };
                        let _ = col;
                        ui.label(txt);
                        if i + 1 < STEPS.len() {
                            ui.label(RichText::new("›").weak());
                        }
                    }
                });
                ui.add_space(8.0);
            });

        egui::TopBottomPanel::bottom("wiz_bottom")
            .frame(egui::Frame::side_top_panel(&ctx.style()).inner_margin(egui::Margin::symmetric(22, 4)))
            .show(ctx, |ui| {
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if !self.wizard_from_settings && ui.button("Skip setup (keep current settings)").clicked() {
                        let mut d = self.saved.clone();
                        d.setup_completed = true;
                        self.s = d;
                        self.save_now();
                        self.finished = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    if self.wizard_from_settings && ui.button("Cancel").clicked() {
                        self.s = self.saved.clone();
                        self.leave_wizard(ctx);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let last = step + 1 == STEPS.len();
                        let next = egui::Button::new(RichText::new(if last { "Finish" } else { "Next" }).strong().color(Color32::BLACK))
                            .fill(ACCENT);
                        if ui.add(next).clicked() {
                            if last {
                                self.set_listening(None);
                                self.s.setup_completed = true;
                                self.save_now();
                                self.finished = true;
                                if self.wizard_from_settings {
                                    self.leave_wizard(ctx);
                                } else {
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                            } else {
                                self.wizard = Some(step + 1);
                            }
                        }
                        if step > 0 && ui.button("Back").clicked() {
                            self.wizard = Some(step - 1);
                        }
                    });
                });
                ui.add_space(10.0);
            });

        egui::CentralPanel::default().frame(egui::Frame::central_panel(&ctx.style()).inner_margin(egui::Margin::symmetric(22, 12))).show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.add_space(8.0);
                match step {
                    0 => {
                        ui.label(RichText::new("Keyboard shortcuts").size(17.0).strong());
                        ui.label(RichText::new("Your current shortcuts are kept. Pick a preset or another common combination if you prefer. Recording hotkeys start and stop recording.").weak());
                        ui.add_space(10.0);
                        self.hotkey_preset_row(ui);
                        ui.add_space(4.0);
                        for f in HkField::ALL {
                            self.hotkey_choice_row(ui, f);
                            ui.add_space(6.0);
                        }
                        self.print_screen_warning(ui);
                    }
                    1 => {
                        ui.label(RichText::new("Where should files go?").size(17.0).strong());
                        let where_text = if util::is_installed() {
                            "By default screenshots go to Pictures\\HDRSnap and recordings to Videos\\HDRSnap."
                        } else {
                            "By default both folders live next to HDRSnap.exe, so the app stays portable."
                        };
                        ui.label(RichText::new(where_text).weak());
                        ui.add_space(10.0);
                        self.folder_row(ui, "Screenshots", false);
                        self.folder_row(ui, "Recordings", true);
                        ui.add_space(10.0);
                        row(ui, "Start with Windows", "Start HDRSnap in the tray when you sign in.", |ui| {
                            toggle(ui, &mut self.s.start_with_windows);
                        });
                    }
                    2 => {
                        ui.label(RichText::new("Screenshot format").size(17.0).strong());
                        ui.add_space(10.0);
                        self.image_format_rows(ui);
                    }
                    _ => {
                        ui.label(RichText::new("Recording format").size(17.0).strong());
                        ui.add_space(10.0);
                        self.video_rows(ui);
                    }
                }
            });
        });
    }

    fn leave_wizard(&mut self, ctx: &egui::Context) {
        self.set_listening(None);
        self.wizard = None;
        self.wizard_from_settings = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(TITLE_SETTINGS.into()));
    }
}
