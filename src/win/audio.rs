//! Audio capture for recordings: WASAPI loopback (what the PC plays) and
//! microphone input. Each source is converted to 32-bit float stereo,
//! aligned with the video's wall-clock timeline and streamed to ffmpeg
//! through its own named pipe (`\\.\pipe\...` is an ffmpeg input like a file).

use crate::log;
use hdrsnap_core::audio::{downmix_to_stereo, Timeline};
use hdrsnap_core::encoders::AudioInput;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::core::{GUID, HSTRING};
use windows::Win32::Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, GENERIC_READ, HANDLE, PROPERTYKEY};
use windows::Win32::Media::Audio::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::*;
use windows::Win32::System::Pipes::*;

#[derive(Clone, Debug)]
pub enum Source {
    /// Loopback of the default playback device.
    System,
    /// Endpoint ID, or empty for the default microphone.
    Microphone(String),
}

impl Source {
    pub fn title(&self) -> &'static str {
        match self {
            Source::System => "System audio",
            Source::Microphone(_) => "Microphone",
        }
    }
    fn tag(&self) -> &'static str {
        match self {
            Source::System => "sys",
            Source::Microphone(_) => "mic",
        }
    }
}

const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY =
    PROPERTYKEY { fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0), pid: 14 };
const SUBTYPE_PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
const SUBTYPE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
const WAVE_FORMAT_PCM_TAG: u16 = 1;
const WAVE_FORMAT_FLOAT_TAG: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE_TAG: u16 = 0xFFFE;

/// Active microphones as (endpoint ID, friendly name).
pub fn list_microphones() -> Vec<(String, String)> {
    let mut out = Vec::new();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let Ok(en) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else { return out };
        let Ok(col) = en.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) else { return out };
        for i in 0..col.GetCount().unwrap_or(0) {
            let Ok(dev) = col.Item(i) else { continue };
            let Ok(id) = dev.GetId() else { continue };
            let id_s = id.to_string().unwrap_or_default();
            CoTaskMemFree(Some(id.0 as *const _));
            out.push((id_s, device_name(&dev)));
        }
    }
    out
}

unsafe fn device_name(dev: &IMMDevice) -> String {
    unsafe {
        let Ok(store) = dev.OpenPropertyStore(STGM_READ) else { return String::new() };
        let Ok(v) = store.GetValue(&PKEY_DEVICE_FRIENDLY_NAME) else { return String::new() };
        match PropVariantToStringAlloc(&v) {
            Ok(p) => {
                let s = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0 as *const _));
                s
            }
            Err(_) => String::new(),
        }
    }
}

#[derive(Clone, Copy)]
enum SampleFmt {
    F32,
    I16,
    I24,
    I32,
}

struct Format {
    rate: u32,
    channels: usize,
    block_align: usize,
    fmt: SampleFmt,
}

unsafe fn parse_format(p: *const WAVEFORMATEX) -> Result<Format, String> {
    unsafe {
        let wf = std::ptr::read_unaligned(p);
        let mut tag = wf.wFormatTag;
        if tag == WAVE_FORMAT_EXTENSIBLE_TAG && wf.cbSize >= 22 {
            let ext = std::ptr::read_unaligned(p as *const WAVEFORMATEXTENSIBLE);
            let sub = ext.SubFormat;
            tag = if sub == SUBTYPE_FLOAT {
                WAVE_FORMAT_FLOAT_TAG
            } else if sub == SUBTYPE_PCM {
                WAVE_FORMAT_PCM_TAG
            } else {
                0
            };
        }
        let fmt = match (tag, wf.wBitsPerSample) {
            (WAVE_FORMAT_FLOAT_TAG, 32) => SampleFmt::F32,
            (WAVE_FORMAT_PCM_TAG, 16) => SampleFmt::I16,
            (WAVE_FORMAT_PCM_TAG, 24) => SampleFmt::I24,
            (WAVE_FORMAT_PCM_TAG, 32) => SampleFmt::I32,
            (t, b) => return Err(format!("unsupported audio format (tag {t}, {b} bit)")),
        };
        Ok(Format { rate: wf.nSamplesPerSec, channels: wf.nChannels.max(1) as usize, block_align: wf.nBlockAlign as usize, fmt })
    }
}

fn to_f32(data: &[u8], f: &Format, out: &mut Vec<f32>) {
    let bytes = f.block_align / f.channels;
    out.clear();
    for s in data.chunks_exact(bytes) {
        out.push(match f.fmt {
            SampleFmt::F32 => f32::from_le_bytes([s[0], s[1], s[2], s[3]]),
            SampleFmt::I16 => i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0,
            SampleFmt::I24 => (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f32 / 8_388_608.0,
            SampleFmt::I32 => i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f32 / 2_147_483_648.0,
        });
    }
}

/// HANDLE wrapper that can move to the writer thread.
struct PipeHandle(isize);
unsafe impl Send for PipeHandle {}

pub struct AudioStream {
    pub input: AudioInput,
    stop: Arc<AtomicBool>,
    start_at: Arc<Mutex<Option<Instant>>>,
    connected: Arc<AtomicBool>,
    capture: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    pipe_name: String,
}

impl AudioStream {
    /// Open the device and the pipe. Audio is discarded until `begin`.
    pub fn start(source: Source) -> Result<Self, String> {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let pipe_name = format!(
            r"\\.\pipe\hdrsnap-{}-{}-{}",
            std::process::id(),
            source.tag(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let stop = Arc::new(AtomicBool::new(false));
        let start_at = Arc::new(Mutex::new(None));
        let connected = Arc::new(AtomicBool::new(false));

        // pipe first, so ffmpeg can open it as soon as it starts
        let h = unsafe {
            CreateNamedPipeW(
                &HSTRING::from(pipe_name.as_str()),
                FILE_FLAGS_AND_ATTRIBUTES(PIPE_ACCESS_OUTBOUND.0),
                PIPE_TYPE_BYTE | PIPE_WAIT,
                1,
                1 << 20,
                0,
                0,
                None,
            )
        };
        if h.is_invalid() {
            return Err(format!("audio pipe: {}", windows::core::Error::from_thread()));
        }
        let (tx, rx) = channel::<Vec<u8>>();
        let writer = {
            let (h, c2) = (PipeHandle(h.0 as isize), connected.clone());
            std::thread::spawn(move || pipe_writer(h, rx, c2))
        };

        let (ready_tx, ready_rx) = sync_channel::<Result<u32, String>>(1);
        let capture = {
            let (src, st, sa) = (source.clone(), stop.clone(), start_at.clone());
            std::thread::spawn(move || {
                if let Err(e) = capture_thread(&src, &st, &sa, tx, &ready_tx) {
                    log!("{} capture: {e}", src.title());
                    let _ = ready_tx.try_send(Err(e));
                }
            })
        };
        let mut s = Self {
            input: AudioInput { path: pipe_name.clone(), sample_rate: 0, title: source.title().into() },
            stop,
            start_at,
            connected,
            capture: Some(capture),
            writer: Some(writer),
            pipe_name,
        };
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(rate)) => {
                s.input.sample_rate = rate;
                Ok(s)
            }
            Ok(Err(e)) => {
                s.finish();
                Err(e)
            }
            Err(_) => {
                s.finish();
                Err("audio device did not respond".into())
            }
        }
    }

    /// Time zero of the video; audio before it is discarded.
    pub fn begin(&self, t0: Instant) {
        *self.start_at.lock().unwrap() = Some(t0);
    }

    /// Stop capturing (the writer keeps flushing to ffmpeg in the background).
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Wait for both threads. Call after ffmpeg exited or was told to finish.
    pub fn finish(&mut self) {
        self.request_stop();
        if let Some(c) = self.capture.take() {
            let _ = c.join();
        }
        if !self.connected.load(Ordering::SeqCst) {
            // ffmpeg never opened the pipe (it failed early): connect ourselves
            // so the writer's ConnectNamedPipe returns.
            unsafe {
                if let Ok(h) = CreateFileW(
                    &HSTRING::from(self.pipe_name.as_str()),
                    GENERIC_READ.0,
                    FILE_SHARE_MODE(0),
                    None,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    None,
                ) {
                    let _ = CloseHandle(h);
                }
            }
        }
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
    }
}

impl Drop for AudioStream {
    fn drop(&mut self) {
        self.finish();
    }
}

fn pipe_writer(h: PipeHandle, rx: Receiver<Vec<u8>>, connected: Arc<AtomicBool>) {
    let h = HANDLE(h.0 as *mut _);
    unsafe {
        match ConnectNamedPipe(h, None) {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => {}
            Err(e) => {
                log!("audio pipe connect: {e}");
                let _ = CloseHandle(h);
                return;
            }
        }
        connected.store(true, Ordering::SeqCst);
        for buf in rx {
            if WriteFile(h, Some(&buf), None, None).is_err() {
                break; // ffmpeg went away; drain nothing more
            }
        }
        let _ = FlushFileBuffers(h);
        let _ = CloseHandle(h); // ffmpeg sees end of stream
    }
}

fn capture_thread(
    src: &Source,
    stop: &AtomicBool,
    start_at: &Mutex<Option<Instant>>,
    tx: Sender<Vec<u8>>,
    ready: &std::sync::mpsc::SyncSender<Result<u32, String>>,
) -> Result<(), String> {
    fn e(what: &'static str) -> impl Fn(windows::core::Error) -> String {
        move |err| format!("{what}: {err}")
    }
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(e("device list"))?;
        let dev = match src {
            Source::System => en.GetDefaultAudioEndpoint(eRender, eConsole).map_err(e("no playback device"))?,
            Source::Microphone(id) if !id.is_empty() => match en.GetDevice(&HSTRING::from(id.as_str())) {
                Ok(d) => d,
                Err(err) => {
                    log!("microphone {id} not available ({err}), using the default one");
                    en.GetDefaultAudioEndpoint(eCapture, eConsole).map_err(e("no microphone"))?
                }
            },
            Source::Microphone(_) => en.GetDefaultAudioEndpoint(eCapture, eConsole).map_err(e("no microphone"))?,
        };
        let name = device_name(&dev);
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).map_err(e("activate"))?;
        let mix = client.GetMixFormat().map_err(e("mix format"))?;
        let parsed = parse_format(mix);
        let flags = if matches!(src, Source::System) { AUDCLNT_STREAMFLAGS_LOOPBACK } else { 0 };
        // 200 ms shared-mode buffer
        let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, mix, None);
        CoTaskMemFree(Some(mix as *const _));
        let fmt = parsed?;
        init.map_err(e("initialize"))?;
        let cap: IAudioCaptureClient = client.GetService().map_err(e("capture service"))?;
        client.Start().map_err(e("start"))?;
        log!("{}: {name} ({} Hz, {} ch)", src.title(), fmt.rate, fmt.channels);
        let _ = ready.try_send(Ok(fmt.rate));

        let mut tl = Timeline::new(fmt.rate);
        let mut t0 = None;
        let mut samples = Vec::new();
        let mut stereo = Vec::new();
        let send_silence = |frames: u64| {
            if frames > 0 {
                let _ = tx.send(vec![0u8; frames as usize * 8]);
            }
        };
        let result = loop {
            if stop.load(Ordering::SeqCst) {
                break Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
            if t0.is_none() {
                t0 = *start_at.lock().unwrap();
            }
            let mut got = false;
            loop {
                match cap.GetNextPacketSize() {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(err) => return Err(format!("capture: {err}")),
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut bflags = 0u32;
                if let Err(err) = cap.GetBuffer(&mut data, &mut frames, &mut bflags, None, None) {
                    return Err(format!("capture: {err}"));
                }
                if let Some(t0) = t0 {
                    got = true;
                    stereo.clear();
                    if bflags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                        stereo.resize(frames as usize * 2, 0.0);
                    } else {
                        let bytes = std::slice::from_raw_parts(data, frames as usize * fmt.block_align);
                        to_f32(bytes, &fmt, &mut samples);
                        downmix_to_stereo(&samples, fmt.channels, &mut stereo);
                    }
                    let (pad, skip) = tl.place(frames as u64, t0.elapsed().as_secs_f64());
                    send_silence(pad);
                    let keep = &stereo[(skip as usize * 2).min(stereo.len())..];
                    if !keep.is_empty() {
                        let mut b = Vec::with_capacity(keep.len() * 4);
                        for s in keep {
                            b.extend_from_slice(&s.to_le_bytes());
                        }
                        let _ = tx.send(b);
                    }
                }
                let _ = cap.ReleaseBuffer(frames);
            }
            if let (Some(t0), false) = (t0, got) {
                send_silence(tl.idle(t0.elapsed().as_secs_f64()));
            }
        };
        if let Some(t0) = t0 {
            send_silence(tl.finish(t0.elapsed().as_secs_f64()));
        }
        let _ = client.Stop();
        result
    }
}
