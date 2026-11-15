//! Screen recording: Desktop Duplication -> GPU tone map / NV12 -> ffmpeg.exe
//! (raw frames on stdin). GDI capture is the fallback when duplication is
//! unavailable (SDR only).

use super::capture::{capture_gdi, duplicate_output, Pixels};
use super::cursor;
use super::display::{self, Monitor};
use super::gpu::{self, Converter, OutMode, Params, HIST_BINS, HIST_MAX_LOG2};
use crate::log;
use super::audio::{AudioStream, Source};
use hdrsnap_core::encoders::{self, AudioInput, AudioTracks, Container, EncodeJob};
use hdrsnap_core::settings::Settings;
use hdrsnap_core::tonemap::{Rolloff, MIN_HDR_FRACTION};
use std::collections::HashMap;
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// ffmpeg.exe next to HDRSnap.exe (or in the data folder, for a portable copy
/// these are the same).
pub fn ffmpeg_path(data_dir: &Path) -> Option<PathBuf> {
    [super::util::exe_dir(), data_dir.to_path_buf()]
        .iter()
        .flat_map(|d| [d.join("ffmpeg.exe"), d.join("ffmpeg").join("ffmpeg.exe"), d.join("ffmpeg").join("bin").join("ffmpeg.exe")])
        .find(|p| p.is_file())
}

static PROBES: Mutex<Option<HashMap<String, bool>>> = Mutex::new(None);

/// Can this ffmpeg + GPU actually encode with `id`? (cached per process)
pub fn encoder_available(ffmpeg: &Path, id: &str) -> bool {
    probe(ffmpeg, id, encoders::probe_args(id))
}

/// Same for an audio encoder (libopus is missing from some ffmpeg builds).
pub fn audio_encoder_available(ffmpeg: &Path, codec: &str) -> bool {
    probe(ffmpeg, &format!("audio:{codec}"), encoders::audio_probe_args(codec))
}

fn probe(ffmpeg: &Path, id: &str, args: Vec<String>) -> bool {
    if let Some(v) = PROBES.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(id)) {
        return *v;
    }
    let ok = Command::new(ffmpeg)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    PROBES.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(id.to_string(), ok);
    ok
}

/// Find the "auto" encoders ahead of time so starting a recording is instant.
pub fn warm_up_probes(ffmpeg: PathBuf, max_width: u32) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(3));
        for c in [Container::Mp4, Container::Webm] {
            for id in encoders::auto_candidates(c, max_width) {
                if encoder_available(&ffmpeg, id) {
                    log!("auto encoder for {}: {id}", c.ext());
                    break;
                }
            }
        }
    });
}

/// Open the audio sources chosen in Settings (none for GIF). A source that
/// fails is logged and left out; the video is recorded either way.
fn start_audio(cfg: &RecordConfig, container: Container) -> Vec<AudioStream> {
    let s = &cfg.settings;
    let Some((codec, _)) = encoders::audio_codec(container) else { return Vec::new() };
    if !s.record_system_audio && !s.record_microphone {
        return Vec::new();
    }
    if !audio_encoder_available(&cfg.ffmpeg, codec) {
        log!("audio encoder {codec} not available in this ffmpeg, recording without audio");
        return Vec::new();
    }
    let mut v = Vec::new();
    let mut sources = Vec::new();
    if s.record_system_audio {
        sources.push(Source::System);
    }
    if s.record_microphone {
        sources.push(Source::Microphone(s.microphone_device.clone()));
    }
    for src in sources {
        match AudioStream::start(src.clone()) {
            Ok(a) => v.push(a),
            Err(e) => log!("{}: {e}, recording without it", src.title()),
        }
    }
    v
}

/// Close ffmpeg's video input and the audio pipes together (ffmpeg only
/// finishes the file once every input has ended).
fn finish_with_audio(sink: Sink, audio: &mut [AudioStream]) -> Result<(), String> {
    for a in audio.iter() {
        a.request_stop();
    }
    let r = sink.finish();
    for a in audio.iter_mut() {
        a.finish();
    }
    r
}

pub struct RecordConfig {
    pub device_name: String,
    /// Desktop coordinates.
    pub region: RECT,
    pub settings: Settings,
    pub exe_dir: PathBuf,
    pub ffmpeg: PathBuf,
    pub output: PathBuf,
}

pub struct RecordResult {
    pub path: PathBuf,
    pub frames: u64,
    pub dropped: u64,
    pub seconds: f64,
    pub encoder: String,
    pub hdr: bool,
    pub method: &'static str,
}

pub struct Recorder {
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    pub started: Instant,
}

impl Recorder {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Stop and wait for the file to be finished (used when exiting).
    pub fn stop_and_wait(&self, timeout: Duration) {
        self.stop();
        let t = Instant::now();
        while !self.done.load(Ordering::SeqCst) && t.elapsed() < timeout {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

pub fn start(cfg: RecordConfig, on_done: Box<dyn FnOnce(Result<RecordResult, String>) + Send>) -> Recorder {
    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let (s2, d2) = (stop.clone(), done.clone());
    std::thread::spawn(move || {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&cfg, &s2)))
            .unwrap_or_else(|_| Err("internal error while recording (see hdrsnap.log)".into()));
        if let Err(e) = &r {
            log!("recording failed: {e}");
        }
        d2.store(true, Ordering::SeqCst);
        on_done(r);
    });
    Recorder { stop, done, started: Instant::now() }
}

/// Clamp a desktop rectangle to the monitor and make it even-sized
/// (4:2:0 video needs even dimensions).
pub fn fit_region(r: RECT, mon: &RECT) -> RECT {
    let mut r = RECT { left: r.left.max(mon.left), top: r.top.max(mon.top), right: r.right.min(mon.right), bottom: r.bottom.min(mon.bottom) };
    if (r.right - r.left) % 2 == 1 {
        r.right -= 1;
    }
    if (r.bottom - r.top) % 2 == 1 {
        r.bottom -= 1;
    }
    r
}

fn resolve_encoder(cfg: &RecordConfig, container: Container, width: u32, height: u32) -> Result<String, String> {
    if container == Container::Gif {
        return Ok("gif".into());
    }
    let chosen = cfg.settings.encoder_for_format().to_string();
    // Hardware encoders have minimum frame sizes (varies by GPU); small
    // regions go to the CPU encoder, which is cheap at that size anyway.
    if width < 256 || height < 128 {
        let cpu = if container == Container::Webm { "libvpx-vp9" } else { "libx264" };
        if chosen == "auto" || encoders::info(&chosen).map(|e| e.hardware).unwrap_or(false) {
            log!("small region ({width}x{height}): using {cpu}");
            return Ok(cpu.into());
        }
    }
    if chosen != "auto" {
        let info = encoders::info(&chosen).ok_or(format!("unknown encoder '{chosen}'"))?;
        if width > info.max_width {
            return Err(format!(
                "{} can't encode frames wider than {} px (this one is {width} px). Pick HEVC or AV1 in Settings > Recording.",
                info.label, info.max_width
            ));
        }
        return Ok(chosen);
    }
    for id in encoders::auto_candidates(container, width) {
        if encoder_available(&cfg.ffmpeg, id) {
            return Ok(id.into());
        }
    }
    Err("no working video encoder found for this format".into())
}

// ---------------------------------------------------------------------------
// ffmpeg process with a writer thread and a bounded frame queue

struct Sink {
    child: Child,
    tx: Option<SyncSender<Vec<u8>>>,
    pool: Receiver<Vec<u8>>,
    writer: Option<JoinHandle<()>>,
    failed: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    log_path: PathBuf,
}

impl Sink {
    fn spawn(ffmpeg: &Path, args: &[String], log_path: &Path) -> Result<Self, String> {
        log!("ffmpeg {}", args.join(" "));
        let log_file = std::fs::File::create(log_path).map_err(|e| format!("ffmpeg log: {e}"))?;
        let mut child = Command::new(ffmpeg)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log_file))
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| format!("could not start ffmpeg: {e}"))?;
        let mut stdin = child.stdin.take().unwrap();
        let (tx, rx) = sync_channel::<Vec<u8>>(6);
        let (ptx, prx) = sync_channel::<Vec<u8>>(12);
        let failed = Arc::new(AtomicBool::new(false));
        let f2 = failed.clone();
        let writer = std::thread::spawn(move || {
            for buf in rx {
                if stdin.write_all(&buf).is_err() {
                    f2.store(true, Ordering::SeqCst);
                    break;
                }
                let _ = ptx.try_send(buf);
            }
            // dropping stdin closes the pipe -> ffmpeg finishes the file
        });
        Ok(Self {
            child,
            tx: Some(tx),
            pool: prx,
            writer: Some(writer),
            failed,
            dropped: Arc::new(AtomicU64::new(0)),
            log_path: log_path.to_path_buf(),
        })
    }

    fn buffer(&self) -> Vec<u8> {
        self.pool.try_recv().unwrap_or_default()
    }

    /// Send one captured frame, repeated as needed so the video timeline
    /// matches wall-clock time (ffmpeg gets constant-rate raw frames, so a
    /// missing frame would otherwise make the video play too fast).
    /// `sent` = frames delivered so far, returns the new count. Only frames
    /// that actually reach ffmpeg are counted, so a dropped frame is made up
    /// by a repeat later and the video never runs ahead of wall-clock time
    /// (which would put it out of sync with the audio).
    fn send_timed(&self, buf: Vec<u8>, sent: u64, start: Instant, fps: u32) -> u64 {
        let target = (start.elapsed().as_secs_f64() * fps as f64) as u64 + 1;
        let reps = target.saturating_sub(sent).clamp(1, fps as u64 * 2);
        let mut delivered = 0;
        for _ in 1..reps {
            let mut dup = self.buffer();
            dup.clear();
            dup.extend_from_slice(&buf);
            if !self.send(dup) {
                return sent + delivered; // queue full: catch up next frame
            }
            delivered += 1;
        }
        delivered += self.send(buf) as u64;
        sent + delivered
    }

    /// Queue without dropping (used for the last frames when stopping).
    fn send_blocking(&self, buf: Vec<u8>) -> bool {
        if let Some(tx) = &self.tx {
            if tx.send(buf).is_ok() {
                return true;
            }
            self.failed.store(true, Ordering::SeqCst);
        }
        false
    }

    /// Queue a frame; if the encoder can't keep up the frame is dropped.
    fn send(&self, buf: Vec<u8>) -> bool {
        if let Some(tx) = &self.tx {
            match tx.try_send(buf) {
                Ok(()) => return true,
                Err(TrySendError::Full(_)) => {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => self.failed.store(true, Ordering::SeqCst),
            }
        }
        false
    }

    fn failed(&self) -> bool {
        self.failed.load(Ordering::SeqCst)
    }

    fn log_tail(&self) -> String {
        let t = std::fs::read_to_string(&self.log_path).unwrap_or_default();
        let lines: Vec<&str> = t.lines().filter(|l| !l.trim().is_empty()).collect();
        lines[lines.len().saturating_sub(3)..].join(" | ")
    }

    fn finish(mut self) -> Result<(), String> {
        self.tx = None;
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
        let status = self.child.wait().map_err(|e| e.to_string())?;
        if !status.success() || self.failed() {
            let tail = self.log_tail();
            return Err(if tail.is_empty() { format!("ffmpeg exited with {status}") } else { format!("ffmpeg: {tail}") });
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------

/// Enumerate monitors, retrying briefly while DXGI can't see `device_name`
/// (during a mode change enumeration can fail and fall back to the GDI list,
/// which has no HDR information and no Desktop Duplication).
fn find_monitor(device_name: &str) -> Result<Vec<Monitor>, String> {
    let t0 = Instant::now();
    loop {
        let monitors = display::enumerate().map_err(|e| format!("displays: {e}"))?;
        let ok = monitors.iter().any(|m| m.device_name == device_name && m.output.is_some());
        if ok || t0.elapsed() > Duration::from_secs(2) {
            return Ok(monitors);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn run(cfg: &RecordConfig, stop: &AtomicBool) -> Result<RecordResult, String> {
    let s = &cfg.settings;
    let mut monitors = find_monitor(&cfg.device_name)?;
    let mon = monitors.iter().find(|m| m.device_name == cfg.device_name).ok_or("monitor disappeared")?;
    let region = fit_region(cfg.region, &mon.rect);
    let (w, h) = ((region.right - region.left) as u32, (region.bottom - region.top) as u32);
    if w < 16 || h < 16 {
        return Err("region is too small to record".into());
    }
    let container = Container::from_str(&s.video_format);
    let fps = if container == Container::Gif { s.gif_fps } else { s.video_fps }.max(1);
    let encoder = resolve_encoder(cfg, container, w, h)?;

    let final_path = cfg.output.clone();
    let encode_path = if container == Container::Gif { final_path.with_extension("gif.part.mkv") } else { final_path.clone() };
    let log_path = cfg.exe_dir.join("ffmpeg.log");

    let mut audio = start_audio(cfg, container);
    let use_gdi = s.capture_method == "gdi" || mon.output.is_none();
    let started = Instant::now();
    let outcome = if use_gdi {
        if mon.output.is_none() {
            log!("warning: no DXGI output for {}, recording via GDI (HDR monitors will look clipped)", mon.device_name);
        }
        record_gdi(cfg, region, fps, &encoder, container, &encode_path, &log_path, stop, &mut audio)
    } else {
        // Desktop Duplication can refuse to start for a moment while windows
        // switch between composed and full screen presentation (minimise,
        // maximise, alt-tab). Falling back to GDI right away would record an
        // HDR monitor clipped and washed out, so retry for a few seconds with
        // fresh DXGI objects first.
        let t0 = Instant::now();
        let mut attempt = 1;
        loop {
            let mon = monitors.iter().find(|m| m.device_name == cfg.device_name).ok_or("monitor disappeared")?;
            match record_gpu(cfg, mon, region, fps, &encoder, container, &encode_path, &log_path, stop, &mut audio) {
                Err(e) if e.starts_with("SETUP:") && t0.elapsed() < Duration::from_secs(4) && !stop.load(Ordering::SeqCst) => {
                    log!("Desktop Duplication not ready ({}), retry {attempt}", &e[6..]);
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(250));
                    monitors = find_monitor(&cfg.device_name)?;
                }
                Err(e) if e.starts_with("SETUP:") => {
                    log!("warning: GPU recording unavailable ({}), using GDI{}", &e[6..], if mon.hdr { " (HDR will look clipped)" } else { "" });
                    break record_gdi(cfg, region, fps, &encoder, container, &encode_path, &log_path, stop, &mut audio);
                }
                r => break r,
            }
        }
    };
    let (frames, dropped, hdr, method) = match outcome {
        Ok((0, ..)) => {
            let _ = std::fs::remove_file(&encode_path);
            return Err("Recording stopped before any frames were captured".into());
        }
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(&encode_path);
            return Err(e);
        }
    };

    if container == Container::Gif {
        log!("GIF palette pass");
        let args = encoders::gif_args(&encode_path.to_string_lossy(), &final_path.to_string_lossy(), fps, s.gif_max_width, s.gif_quality);
        let st = Command::new(&cfg.ffmpeg)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log_path).map(Stdio::from).unwrap_or(Stdio::null()))
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(&encode_path);
        if !st.success() {
            return Err(format!("GIF conversion failed (see ffmpeg.log), exit {st}"));
        }
    }
    let seconds = started.elapsed().as_secs_f64();
    log!("recorded {} frames ({} dropped) in {:.1}s with {encoder} via {method} -> {}", frames, dropped, seconds, final_path.display());
    Ok(RecordResult { path: final_path, frames, dropped, seconds, encoder, hdr, method })
}

// ---------------------------------------------------------------------------
// GPU path

struct Dup {
    output: IDXGIOutput5,
    dev: ID3D11Device,
    dup: Option<IDXGIOutputDuplication>,
    hdr: bool,
    lost_since: Option<Instant>,
}

impl Dup {
    fn new(output: &IDXGIOutput, dev: &ID3D11Device, hdr: bool) -> windows::core::Result<Self> {
        let o5: IDXGIOutput5 = output.cast()?;
        let (dup, _) = duplicate_output(&o5, dev, hdr)?;
        Ok(Self { output: o5, dev: dev.clone(), dup: Some(dup), hdr, lost_since: None })
    }

    /// Drop the duplication; the next `acquire` creates a new one.
    fn reset(&mut self) {
        self.dup = None;
        self.lost_since.get_or_insert_with(Instant::now);
    }

    /// Wait up to `ms` for a new desktop frame. Returns the texture if one
    /// arrived (call `release` after copying). While duplication is lost
    /// (UAC prompt, lock screen, mode switch) it keeps retrying and returns
    /// None, so the recording simply repeats the last image.
    fn acquire(&mut self, ms: u32) -> windows::core::Result<Option<ID3D11Texture2D>> {
        unsafe {
            if self.dup.is_none() {
                match duplicate_output(&self.output, &self.dev, self.hdr).map(|(d, _)| d) {
                    Ok(d) => {
                        log!("desktop duplication restored");
                        self.dup = Some(d);
                        self.lost_since = None;
                    }
                    Err(e) => {
                        let since = *self.lost_since.get_or_insert_with(Instant::now);
                        if since.elapsed() > Duration::from_secs(120) {
                            return Err(e); // give up after two minutes
                        }
                        std::thread::sleep(Duration::from_millis(ms.clamp(1, 50) as u64));
                        return Ok(None);
                    }
                }
            }
            let dup = self.dup.as_ref().unwrap();
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res = None;
            match dup.AcquireNextFrame(ms, &mut info, &mut res) {
                // (A mouse-only update still carries the full desktop image.)
                Ok(()) => match res {
                    Some(r) => Ok(Some(r.cast()?)),
                    None => {
                        let _ = dup.ReleaseFrame();
                        Ok(None)
                    }
                },
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => Ok(None),
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST || e.code() == DXGI_ERROR_INVALID_CALL => {
                    log!("desktop duplication lost ({e}), retrying");
                    self.dup = None; // release before re-creating
                    self.lost_since = Some(Instant::now());
                    Ok(None)
                }
                Err(e) => Err(e),
            }
        }
    }

    fn release(&self) {
        if let Some(d) = &self.dup {
            unsafe {
                let _ = d.ReleaseFrame();
            }
        }
    }

    fn rotation(&self) -> i32 {
        self.dup.as_ref().map(|d| unsafe { d.GetDesc().Rotation.0 }).unwrap_or(1)
    }
}

struct PeakTracker {
    peak: f32,
    disp_peak: f32,
}

impl PeakTracker {
    fn update(&mut self, hist: &[u32; HIST_BINS + 1], samples: u32, dt: f32) {
        let hdr = hist[HIST_BINS];
        let target = if (hdr as f32) < MIN_HDR_FRACTION * samples as f32 {
            1.0
        } else {
            let skip = (samples as f32 * 0.0005) as u32;
            let mut acc = 0;
            let mut bin = 0;
            for i in (0..HIST_BINS).rev() {
                acc += hist[i];
                if acc > skip {
                    bin = i;
                    break;
                }
            }
            let mut p = 2f32.powf((bin + 1) as f32 / HIST_BINS as f32 * HIST_MAX_LOG2);
            if self.disp_peak > 1.05 {
                p = p.min(self.disp_peak);
            }
            p.max(1.0)
        };
        // brighten quickly, relax slowly (no visible pumping)
        let tau = if target > self.peak { 0.25 } else { 1.0 };
        let a = 1.0 - (-dt / tau).exp();
        self.peak += (target - self.peak) * a;
    }
}

#[allow(clippy::too_many_arguments)]
fn record_gpu(
    cfg: &RecordConfig,
    mon: &Monitor,
    region: RECT,
    fps: u32,
    encoder: &str,
    container: Container,
    path: &Path,
    log_path: &Path,
    stop: &AtomicBool,
    audio: &mut [AudioStream],
) -> Result<(u64, u64, bool, &'static str), String> {
    let s = &cfg.settings;
    let setup = |e: windows::core::Error| format!("SETUP:{e}");
    let adapter: IDXGIAdapter = mon.adapter.as_ref().unwrap().cast().map_err(setup)?;
    let (dev, ctx) = gpu::create_device(Some(&adapter)).map_err(setup)?;
    let mut dup = Dup::new(mon.output.as_ref().unwrap(), &dev, mon.hdr).map_err(setup)?;
    let rotation = dup.rotation().clamp(1, 4);

    // First frame tells us format + size.
    let t0 = Instant::now();
    // On an HDR monitor never settle for an 8-bit first frame: that would lock
    // the whole recording into the SDR path with clipped colours.
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    let first = loop {
        if let Some(t) = dup.acquire(100).map_err(setup)? {
            unsafe { t.GetDesc(&mut desc) };
            if !mon.hdr || desc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT || t0.elapsed() > Duration::from_millis(1500) {
                break t;
            }
            log!("first frame is 8-bit on an HDR monitor (format {}), re-creating the duplication", desc.Format.0);
            dup.release();
            drop(t);
            dup.reset();
        }
        if t0.elapsed() > Duration::from_secs(3) {
            return Err("SETUP:no frame from Desktop Duplication".into());
        }
    };
    let mut is_hdr = desc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT;
    if mon.hdr && !is_hdr {
        log!("warning: recording an HDR monitor from 8-bit frames, colours may be clipped");
    }
    let (w, h) = ((region.right - region.left) as u32, (region.bottom - region.top) as u32);
    let mode = if container == Container::Gif { OutMode::Rgba } else { OutMode::Nv12 };
    let mut conv = Converter::new(&dev, &ctx, desc.Format, desc.Width, desc.Height, w, h, mode).map_err(setup)?;
    if is_hdr && !conv.has_histogram() {
        log!("histogram shader unavailable: highlight roll-off disabled for this recording");
    }
    unsafe { ctx.CopyResource(conv.source(), &first) };
    dup.release();
    drop(first);

    let audio_inputs: Vec<AudioInput> = audio.iter().map(|a| a.input.clone()).collect();
    let job = EncodeJob {
        ffmpeg_input_pix_fmt: if mode == OutMode::Nv12 { "nv12" } else { "rgba" },
        width: w,
        height: h,
        fps,
        encoder,
        quality: s.video_quality,
        container,
        output: &path.to_string_lossy(),
        audio: &audio_inputs,
        audio_tracks: AudioTracks::from_str(&s.audio_tracks),
    };
    let sink = Sink::spawn(&cfg.ffmpeg, &encoders::live_args(&job), log_path)?;
    log!(
        "recording {}x{} @{fps} from {} ({}), {} -> {}",
        w,
        h,
        mon.device_name,
        if is_hdr { "HDR, tone mapped" } else { "SDR" },
        encoder,
        path.display()
    );

    let mut params = Params {
        a: [region.left - mon.rect.left, region.top - mon.rect.top, w as i32, h as i32],
        b: [desc.Width as i32, desc.Height as i32, rotation, is_hdr as i32],
        c: [1.0 / mon.sdr_white_scale.max(0.01), s.tonemap_knee, 1.0, 1.0],
        d: [s.highlight_desaturation, 0.0, 4.0, 0.0],
        e: [0; 4],
        f: [0, if mode == OutMode::Nv12 { 0 } else { 1 }, 0, 0],
    };
    let mut peak = PeakTracker { peak: 1.0, disp_peak: mon.max_nits / (mon.sdr_white_scale * 80.0) };
    let auto_tm = s.tonemap_mode != "clip";
    let samples = w.div_ceil(4) * h.div_ceil(4);

    let interval = Duration::from_secs_f64(1.0 / fps as f64);
    let start = Instant::now();
    for a in audio.iter() {
        // delay audio by the GPU readback latency so it lines up with the picture
        a.begin(start.checked_sub(interval * gpu::READBACK_LATENCY).unwrap_or(start));
    }
    let mut n: u64 = 0; // frames delivered to ffmpeg (timeline position)
    let mut last_cursor: isize = 0;
    let mut error = None;
    let mut format_mismatch: Option<Instant> = None;

    while !stop.load(Ordering::SeqCst) {
        if sink.failed() {
            break;
        }
        let due = start + interval * n as u32;
        let now = Instant::now();
        let wait = due.saturating_duration_since(now).as_millis() as u32;
        match dup.acquire(wait) {
            Ok(Some(t)) => {
                let mut d = D3D11_TEXTURE2D_DESC::default();
                unsafe { t.GetDesc(&mut d) };
                if d.Width != desc.Width || d.Height != desc.Height {
                    dup.release();
                    // Resolution / rotation changed: keep what we have so far.
                    log!("display mode changed, stopping the recording");
                    break;
                }
                if d.Format != desc.Format {
                    dup.release();
                    drop(t);
                    let since = *format_mismatch.get_or_insert_with(Instant::now);
                    let hdr_frame = d.Format == DXGI_FORMAT_R16G16B16A16_FLOAT;
                    if hdr_frame || since.elapsed() > Duration::from_secs(2) {
                        // HDR came back (or this is the new normal, e.g. HDR
                        // was switched off): convert from the new format.
                        log!("capture format changed {} -> {}, rebuilding the converter", desc.Format.0, d.Format.0);
                        if let Ok(rest) = conv.drain() {
                            for b in rest {
                                n += sink.send_blocking(b) as u64;
                            }
                        }
                        conv = Converter::new(&dev, &ctx, d.Format, d.Width, d.Height, w, h, mode).map_err(|e| format!("GPU: {e}"))?;
                        desc.Format = d.Format;
                        is_hdr = hdr_frame;
                        params.b[3] = is_hdr as i32;
                        last_cursor = 0;
                        format_mismatch = None;
                    } else {
                        // 8-bit frame on an HDR monitor: DXGI switched the
                        // duplication to BGRA8 (clipped). Repeat the last good
                        // frame and ask for an FP16 duplication again.
                        log!("8-bit frame from an HDR monitor (format {}), re-creating the duplication", d.Format.0);
                        dup.reset();
                    }
                    continue;
                }
                format_mismatch = None;
                unsafe { ctx.CopyResource(conv.source(), &t) };
                dup.release();
            }
            Ok(None) => {}
            Err(e) => {
                error = Some(format!("capture lost: {e}"));
                break;
            }
        }
        let now = Instant::now();
        if now < due {
            continue;
        }

        // cursor
        params.f[0] = 0;
        if s.record_cursor {
            if let Some((pos, hc)) = cursor::current() {
                if hc.0 as isize != last_cursor {
                    if let Some(img) = cursor::render(hc) {
                        let _ = conv.set_cursor(img.handle, img.width as u32, img.height as u32, &img.rgba);
                        params.e[2] = img.width as i32;
                        params.e[3] = img.height as i32;
                        last_cursor = img.handle;
                        CURSOR_HOT.with(|c| c.set(img.hotspot));
                    }
                }
                let hot = CURSOR_HOT.with(|c| c.get());
                params.e[0] = pos.x - hot.0 - region.left;
                params.e[1] = pos.y - hot.1 - region.top;
                params.f[0] = (last_cursor != 0) as i32;
            }
        }

        // highlight roll-off for HDR content
        if is_hdr && auto_tm && peak.peak > 1.01 {
            if let Some(c) = Rolloff::new(s.tonemap_knee, peak.peak) {
                let (k, pk, b) = c.params();
                params.c[1] = k;
                params.c[2] = pk;
                params.c[3] = b;
                params.d[1] = 1.0;
            }
        } else {
            params.d[1] = 0.0;
        }

        let mut buf = sink.buffer();
        match conv.process(&params, is_hdr && auto_tm, &mut buf) {
            Ok(hist) => {
                if let Some(hh) = hist {
                    peak.update(&hh, samples, 1.0 / fps as f32);
                }
                if conv.has_output() {
                    n = sink.send_timed(buf, n, start, fps);
                }
            }
            Err(e) => {
                error = Some(format!("GPU: {e}"));
                break;
            }
        }
    }

    if let Ok(rest) = conv.drain() {
        for b in rest {
            n += sink.send_blocking(b) as u64;
        }
    }
    let dropped = sink.dropped.load(Ordering::Relaxed);
    let fin = finish_with_audio(sink, audio);
    if let Some(e) = error {
        return Err(e);
    }
    fin?;
    Ok((n, dropped, is_hdr, "DXGI"))
}

thread_local! {
    static CURSOR_HOT: std::cell::Cell<(i32, i32)> = const { std::cell::Cell::new((0, 0)) };
}

// ---------------------------------------------------------------------------
// GDI path (no HDR, CPU only)

#[allow(clippy::too_many_arguments)]
fn record_gdi(
    cfg: &RecordConfig,
    region: RECT,
    fps: u32,
    encoder: &str,
    container: Container,
    path: &Path,
    log_path: &Path,
    stop: &AtomicBool,
    audio: &mut [AudioStream],
) -> Result<(u64, u64, bool, &'static str), String> {
    let s = &cfg.settings;
    let (w, h) = ((region.right - region.left) as u32, (region.bottom - region.top) as u32);
    let audio_inputs: Vec<AudioInput> = audio.iter().map(|a| a.input.clone()).collect();
    let job = EncodeJob {
        ffmpeg_input_pix_fmt: "bgra",
        width: w,
        height: h,
        fps,
        encoder,
        quality: s.video_quality,
        container,
        output: &path.to_string_lossy(),
        audio: &audio_inputs,
        audio_tracks: AudioTracks::from_str(&s.audio_tracks),
    };
    let sink = Sink::spawn(&cfg.ffmpeg, &encoders::live_args(&job), log_path)?;
    log!("recording {}x{} @{fps} via GDI, {} -> {}", w, h, encoder, path.display());
    let interval = Duration::from_secs_f64(1.0 / fps as f64);
    let start = Instant::now();
    for a in audio.iter() {
        a.begin(start);
    }
    let mut n: u64 = 0;
    let mut cur: Option<cursor::CursorImage> = None;
    while !stop.load(Ordering::SeqCst) && !sink.failed() {
        let due = start + interval * n as u32;
        let now = Instant::now();
        if now < due {
            std::thread::sleep(due - now);
        }
        let f = capture_gdi(&region).map_err(|e| format!("GDI capture: {e}"))?;
        let Pixels::Bgra8(mut px) = f.pixels else { return Err("unexpected GDI pixel format".into()) };
        if s.record_cursor {
            if let Some((pos, hc)) = cursor::current() {
                if cur.as_ref().map(|c| c.handle) != Some(hc.0 as isize) {
                    cur = cursor::render(hc);
                }
                if let Some(c) = &cur {
                    blend_bgra(&mut px, w as usize, h as usize, pos.x - c.hotspot.0 - region.left, pos.y - c.hotspot.1 - region.top, c);
                }
            }
        }
        n = sink.send_timed(px, n, start, fps);
    }
    let dropped = sink.dropped.load(Ordering::Relaxed);
    finish_with_audio(sink, audio)?;
    Ok((n, dropped, false, "GDI"))
}

fn blend_bgra(px: &mut [u8], w: usize, h: usize, x0: i32, y0: i32, c: &cursor::CursorImage) {
    for cy in 0..c.height as i32 {
        let y = y0 + cy;
        if y < 0 || y >= h as i32 {
            continue;
        }
        for cx in 0..c.width as i32 {
            let x = x0 + cx;
            if x < 0 || x >= w as i32 {
                continue;
            }
            let s = (cy as usize * c.width + cx as usize) * 4;
            let a = c.rgba[s + 3] as u32;
            if a == 0 {
                continue;
            }
            let d = (y as usize * w + x as usize) * 4;
            for (k, sc) in [2usize, 1, 0].iter().enumerate() {
                px[d + k] = ((c.rgba[s + sc] as u32 * a + px[d + k] as u32 * (255 - a) + 127) / 255) as u8;
            }
        }
    }
}
