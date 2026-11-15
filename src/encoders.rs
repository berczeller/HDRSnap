//! Video encoder catalogue and FFmpeg command lines.
//!
//! Frames are piped into ffmpeg.exe as raw video on stdin: NV12 from the GPU
//! path (already tone mapped and converted to BT.709 limited range), or BGRA
//! from the GDI fallback / GIF path.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Container {
    Mp4,
    Webm,
    Gif,
}

impl Container {
    pub fn from_str(s: &str) -> Self {
        match s {
            "webm" => Container::Webm,
            "gif" => Container::Gif,
            _ => Container::Mp4,
        }
    }
    pub fn ext(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::Webm => "webm",
            Container::Gif => "gif",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    X264,
    X265,
    Nvenc,
    Amf,
    Qsv,
    Vp9,
    Aom,
}

pub struct EncoderInfo {
    pub id: &'static str,
    pub label: &'static str,
    pub containers: &'static [Container],
    pub kind: Kind,
    pub hardware: bool,
    /// Codec max frame width (H.264 hardware encoders stop at 4096).
    pub max_width: u32,
    /// AV1 / HEVC use a different quality scale.
    pub codec: &'static str,
}

use Container::*;

pub const ENCODERS: &[EncoderInfo] = &[
    EncoderInfo { id: "h264_nvenc", label: "NVIDIA NVENC H.264", containers: &[Mp4], kind: Kind::Nvenc, hardware: true, max_width: 4096, codec: "h264" },
    EncoderInfo { id: "hevc_nvenc", label: "NVIDIA NVENC HEVC (H.265)", containers: &[Mp4], kind: Kind::Nvenc, hardware: true, max_width: 8192, codec: "hevc" },
    EncoderInfo { id: "av1_nvenc", label: "NVIDIA NVENC AV1", containers: &[Mp4, Webm], kind: Kind::Nvenc, hardware: true, max_width: 8192, codec: "av1" },
    EncoderInfo { id: "h264_amf", label: "AMD AMF H.264", containers: &[Mp4], kind: Kind::Amf, hardware: true, max_width: 4096, codec: "h264" },
    EncoderInfo { id: "hevc_amf", label: "AMD AMF HEVC (H.265)", containers: &[Mp4], kind: Kind::Amf, hardware: true, max_width: 8192, codec: "hevc" },
    EncoderInfo { id: "av1_amf", label: "AMD AMF AV1", containers: &[Mp4, Webm], kind: Kind::Amf, hardware: true, max_width: 8192, codec: "av1" },
    EncoderInfo { id: "h264_qsv", label: "Intel Quick Sync H.264", containers: &[Mp4], kind: Kind::Qsv, hardware: true, max_width: 4096, codec: "h264" },
    EncoderInfo { id: "hevc_qsv", label: "Intel Quick Sync HEVC (H.265)", containers: &[Mp4], kind: Kind::Qsv, hardware: true, max_width: 8192, codec: "hevc" },
    EncoderInfo { id: "av1_qsv", label: "Intel Quick Sync AV1", containers: &[Mp4, Webm], kind: Kind::Qsv, hardware: true, max_width: 8192, codec: "av1" },
    EncoderInfo { id: "libx264", label: "x264 H.264 (CPU)", containers: &[Mp4], kind: Kind::X264, hardware: false, max_width: 16384, codec: "h264" },
    EncoderInfo { id: "libx265", label: "x265 HEVC (CPU, slow)", containers: &[Mp4], kind: Kind::X265, hardware: false, max_width: 16384, codec: "hevc" },
    EncoderInfo { id: "libvpx-vp9", label: "VP9 (CPU)", containers: &[Webm], kind: Kind::Vp9, hardware: false, max_width: 16384, codec: "vp9" },
    EncoderInfo { id: "libaom-av1", label: "AV1 libaom (CPU, very slow)", containers: &[Webm], kind: Kind::Aom, hardware: false, max_width: 16384, codec: "av1" },
];

pub fn info(id: &str) -> Option<&'static EncoderInfo> {
    ENCODERS.iter().find(|e| e.id == id)
}

pub fn encoders_for(c: Container) -> impl Iterator<Item = &'static EncoderInfo> {
    ENCODERS.iter().filter(move |e| e.containers.contains(&c))
}

/// Preference order for "auto". Hardware first; H.264 hardware is skipped
/// for frames wider than it supports (e.g. 5120x1440 super-ultrawides).
pub fn auto_candidates(c: Container, width: u32) -> Vec<&'static str> {
    let order: &[&str] = match c {
        Mp4 => &["h264_nvenc", "hevc_nvenc", "h264_amf", "hevc_amf", "h264_qsv", "hevc_qsv", "libx264"],
        Webm => &["av1_nvenc", "av1_amf", "av1_qsv", "libvpx-vp9"],
        Gif => &[],
    };
    order.iter().copied().filter(|id| info(id).map(|e| width <= e.max_width).unwrap_or(false)).collect()
}

/// Map the 1..100 Quality slider to the encoder's quantizer (lower = better).
pub fn quantizer(e: &EncoderInfo, quality: u32) -> u32 {
    let q = quality.clamp(1, 100) as f32;
    let t = (q - 1.0) / 99.0; // 0 = worst, 1 = best
    let (worst, best) = match (e.kind, e.codec) {
        (Kind::Vp9 | Kind::Aom, _) => (55.0, 15.0),
        (_, "av1") => (55.0, 18.0),
        _ => (40.0, 14.0),
    };
    (worst + (best - worst) * t).round() as u32
}

/// One raw audio stream for ffmpeg: interleaved f32le stereo, read from a
/// named pipe (or any path ffmpeg can open).
#[derive(Clone, Debug, PartialEq)]
pub struct AudioInput {
    pub path: String,
    pub sample_rate: u32,
    /// Track name shown by players / editors ("System audio", "Microphone").
    pub title: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioTracks {
    /// Every source mixed into one track.
    Mixed,
    /// One audio track per source inside the video file.
    Separate,
}

impl AudioTracks {
    pub fn from_str(s: &str) -> Self {
        if s == "separate" { AudioTracks::Separate } else { AudioTracks::Mixed }
    }
}

/// Audio codec + bitrate for a container (GIF has no audio).
pub fn audio_codec(c: Container) -> Option<(&'static str, &'static str)> {
    match c {
        Mp4 => Some(("aac", "192k")),
        Webm => Some(("libopus", "160k")),
        Gif => None,
    }
}

pub struct EncodeJob<'a> {
    pub ffmpeg_input_pix_fmt: &'a str, // "nv12" or "bgra"
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub encoder: &'a str,
    pub quality: u32,
    pub container: Container,
    pub output: &'a str,
    pub audio: &'a [AudioInput],
    pub audio_tracks: AudioTracks,
}

/// Arguments for the live encode (frames on stdin).
pub fn live_args(job: &EncodeJob) -> Vec<String> {
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "warning", "-y", "-f", "rawvideo", "-pix_fmt"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    a.push(job.ffmpeg_input_pix_fmt.into());
    a.extend(["-s".into(), format!("{}x{}", job.width, job.height)]);
    a.extend(["-framerate".into(), job.fps.to_string()]);
    a.extend(["-thread_queue_size".into(), "64".into(), "-i".into(), "-".into()]);
    let audio = if job.container == Gif { &[][..] } else { job.audio };
    for inp in audio {
        a.extend(
            // raw PCM needs no probing; without these ffmpeg 7.x waits ~5 s of
            // real-time audio before it starts reading the video
            ["-probesize", "32", "-analyzeduration", "0", "-f", "f32le", "-ar", &inp.sample_rate.to_string(), "-ch_layout", "stereo", "-thread_queue_size", "4096", "-i", &inp.path]
                .iter()
                .map(|s| s.to_string()),
        );
    }

    if job.container == Gif {
        // GIF: lossless intermediate first, palette pass when recording stops.
        a.extend(["-c:v", "libx264rgb", "-preset", "ultrafast", "-qp", "0"].iter().map(|s| s.to_string()));
        a.push(job.output.into());
        return a;
    }

    let e = info(job.encoder).unwrap_or_else(|| info("libx264").unwrap());
    let qv = quantizer(e, job.quality).to_string();
    if job.ffmpeg_input_pix_fmt != "nv12" {
        // RGB in (GDI path): convert with the right matrix + range.
        a.extend(["-vf".into(), "scale=out_color_matrix=bt709:out_range=tv,format=nv12".into()]);
    }
    a.extend(["-c:v".into(), e.id.into()]);
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    match e.kind {
        Kind::X264 => a.extend(s(&["-preset", "veryfast", "-crf", &qv, "-pix_fmt", "yuv420p"])),
        Kind::X265 => a.extend(s(&["-preset", "ultrafast", "-crf", &qv, "-pix_fmt", "yuv420p"])),
        Kind::Nvenc => a.extend(s(&["-preset", "p4", "-tune", "hq", "-rc", "vbr", "-cq", &qv, "-b:v", "0"])),
        Kind::Amf => a.extend(s(&["-quality", "balanced", "-rc", "cqp", "-qp_i", &qv, "-qp_p", &qv])),
        Kind::Qsv => a.extend(s(&["-preset", "faster", "-global_quality", &qv])),
        Kind::Vp9 => a.extend(s(&["-deadline", "realtime", "-cpu-used", "8", "-row-mt", "1", "-crf", &qv, "-b:v", "0", "-pix_fmt", "yuv420p"])),
        Kind::Aom => a.extend(s(&["-usage", "realtime", "-cpu-used", "8", "-row-mt", "1", "-crf", &qv, "-b:v", "0", "-pix_fmt", "yuv420p"])),
    }
    if e.codec == "hevc" && job.container == Mp4 {
        a.extend(s(&["-tag:v", "hvc1"])); // plays in Windows/Apple players
    }
    // Keyframe every 2 s so seeking works well.
    a.extend(["-g".into(), (job.fps * 2).to_string()]);
    a.extend(s(&["-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-color_range", "tv"]));
    a.extend(audio_args(audio, job.audio_tracks, job.container));
    if job.container == Mp4 {
        a.extend(s(&["-movflags", "+faststart"]));
    }
    a.push(job.output.into());
    a
}

/// Stream mapping + audio encoding. Inputs 1.. are the audio sources.
fn audio_args(audio: &[AudioInput], tracks: AudioTracks, container: Container) -> Vec<String> {
    let Some((codec, bitrate)) = audio_codec(container) else { return Vec::new() };
    if audio.is_empty() {
        return Vec::new();
    }
    let mut a: Vec<String> = vec!["-map".into(), "0:v".into()];
    let titles: Vec<String>;
    if tracks == AudioTracks::Mixed && audio.len() > 1 {
        let ins: String = (1..=audio.len()).map(|i| format!("[{i}:a]")).collect();
        a.push("-filter_complex".into());
        a.push(format!("{ins}amix=inputs={}:duration=longest:normalize=0,alimiter=limit=0.98:level=false[aout]", audio.len()));
        a.extend(["-map".into(), "[aout]".into()]);
        titles = vec![audio.iter().map(|i| i.title.as_str()).collect::<Vec<_>>().join(" + ")];
    } else {
        for i in 1..=audio.len() {
            a.extend(["-map".into(), format!("{i}:a")]);
        }
        titles = audio.iter().map(|i| i.title.clone()).collect();
    }
    a.extend(["-c:a".into(), codec.into(), "-b:a".into(), bitrate.into(), "-ar".into(), "48000".into()]);
    for (i, t) in titles.iter().enumerate() {
        a.push(format!("-metadata:s:a:{i}"));
        a.push(format!("title={t}"));
    }
    // The first track plays by default; the others are still there for editors.
    a.extend(["-disposition:a:0".into(), "default".into()]);
    a
}

/// Quick availability probe for an audio encoder.
pub fn audio_probe_args(codec: &str) -> Vec<String> {
    ["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo", "-t", "0.1", "-c:a", codec, "-f", "null", "-"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Second pass for GIF: palette per clip, optional downscale.
pub fn gif_args(input: &str, output: &str, fps: u32, max_width: u32, quality: u32) -> Vec<String> {
    let q = quality.clamp(1, 100);
    let colors = (32 + (q * 224) / 100).min(256);
    let dither = if q >= 60 { "sierra2_4a" } else { "bayer:bayer_scale=3" };
    let scale = if max_width > 0 { format!("scale='min({max_width},iw)':-2:flags=lanczos,") } else { String::new() };
    let filter = format!(
        "fps={fps},{scale}split[a][b];[a]palettegen=max_colors={colors}:stats_mode=diff[p];[b][p]paletteuse=dither={dither}:diff_mode=rectangle"
    );
    ["-hide_banner", "-loglevel", "warning", "-y", "-i", input, "-vf", &filter, "-loop", "0", output]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Quick availability probe: encode one tiny frame.
pub fn probe_args(encoder: &str) -> Vec<String> {
    let fmt = if encoder.contains("qsv") || encoder.contains("nvenc") || encoder.contains("amf") { "nv12" } else { "yuv420p" };
    [
        "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "color=black:s=320x240:r=30", "-frames:v", "1", "-pix_fmt", fmt,
        "-c:v", encoder, "-f", "null", "-",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_skips_h264_hw_for_wide_frames() {
        let c = auto_candidates(Mp4, 5120);
        assert_eq!(c[0], "hevc_nvenc");
        assert!(c.contains(&"libx264"));
        assert_eq!(auto_candidates(Mp4, 2560)[0], "h264_nvenc");
        assert_eq!(auto_candidates(Webm, 5120)[0], "av1_nvenc");
    }

    #[test]
    fn quality_mapping() {
        let x = info("libx264").unwrap();
        assert_eq!(quantizer(x, 100), 14);
        assert_eq!(quantizer(x, 1), 40);
        assert!(quantizer(x, 70) < 24);
        let a = info("av1_nvenc").unwrap();
        assert!(quantizer(a, 70) > quantizer(x, 70));
    }

    #[test]
    fn builds_args() {
        let job = EncodeJob {
            ffmpeg_input_pix_fmt: "nv12",
            width: 5120,
            height: 1440,
            fps: 60,
            encoder: "hevc_nvenc",
            quality: 70,
            container: Mp4,
            output: "out.mp4",
            audio: &[],
            audio_tracks: AudioTracks::Mixed,
        };
        let a = live_args(&job).join(" ");
        assert!(!a.contains("-map"));
        assert!(a.contains("-pix_fmt nv12 -s 5120x1440 -framerate 60"));
        assert!(a.contains("-c:v hevc_nvenc"));
        assert!(a.contains("-tag:v hvc1"));
        assert!(a.ends_with("out.mp4"));
        let g = gif_args("in.mkv", "o.gif", 15, 960, 80).join(" ");
        assert!(g.contains("palettegen=max_colors=211"));
    }

    fn audio_job<'a>(audio: &'a [AudioInput], tracks: AudioTracks, container: Container) -> Vec<String> {
        live_args(&EncodeJob {
            ffmpeg_input_pix_fmt: "nv12",
            width: 1920,
            height: 1080,
            fps: 60,
            encoder: "libx264",
            quality: 70,
            container,
            output: "out",
            audio,
            audio_tracks: tracks,
        })
    }

    #[test]
    fn audio_args_mixed_and_separate() {
        let ins = [
            AudioInput { path: r"\\.\pipe\sys".into(), sample_rate: 48000, title: "System audio".into() },
            AudioInput { path: r"\\.\pipe\mic".into(), sample_rate: 44100, title: "Microphone".into() },
        ];
        let m = audio_job(&ins, AudioTracks::Mixed, Mp4).join(" ");
        assert!(m.contains(r"-ar 44100 -ch_layout stereo -thread_queue_size 4096 -i \\.\pipe\mic"));
        assert!(m.contains("[1:a][2:a]amix=inputs=2"));
        assert!(m.contains("-map 0:v -filter_complex"));
        assert!(m.contains("-map [aout] -c:a aac"));
        assert!(m.contains("title=System audio + Microphone"));

        let s = audio_job(&ins, AudioTracks::Separate, Webm).join(" ");
        assert!(!s.contains("amix"));
        assert!(s.contains("-map 0:v -map 1:a -map 2:a -c:a libopus"));
        assert!(s.contains("-metadata:s:a:1 title=Microphone"));

        // one source: no mixing needed even in Mixed mode
        let one = audio_job(&ins[..1], AudioTracks::Mixed, Mp4).join(" ");
        assert!(one.contains("-map 0:v -map 1:a -c:a aac"));

        // GIF never gets audio
        let g = audio_job(&ins, AudioTracks::Mixed, Gif).join(" ");
        assert!(!g.contains("pipe"));
    }
}
