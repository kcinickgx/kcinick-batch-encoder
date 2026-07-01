// Encoding logic: file discovery, probing (ffprobe) and building the ffmpeg
// argument list. This is the Rust port of fast6.php.

use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub const EXTENSIONS: [&str; 4] = ["mp4", "mkv", "avi", "mov"];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CropMode {
    /// centered crop (equal top and bottom)
    Centered = 1,
    /// crop 1/3 from the top, 2/3 from the bottom
    OneThird = 2,
    /// no crop
    None = 3,
}

/// Video codec chosen by the user.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
    Av1,
}

impl Codec {
    pub fn label(&self) -> &'static str {
        match self {
            Codec::H264 => "H264",
            Codec::H265 => "H265",
            Codec::Av1 => "AV1",
        }
    }
    /// NVENC (GPU) encoder name for this codec.
    pub fn nvenc(&self) -> &'static str {
        match self {
            Codec::H264 => "h264_nvenc",
            Codec::H265 => "hevc_nvenc",
            Codec::Av1 => "av1_nvenc",
        }
    }
}

/// Concrete encoder resolved after detection (GPU or CPU).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum VideoEncoder {
    Nvenc(&'static str), // h264_nvenc / hevc_nvenc / av1_nvenc
    X26x(&'static str),  // libx264 / libx265
    Svt,                 // libsvtav1
    Aom,                 // libaom-av1
}

impl VideoEncoder {
    pub fn label(&self) -> String {
        match self {
            VideoEncoder::Nvenc(n) => format!("{n} (GPU)"),
            VideoEncoder::X26x(n) => format!("{n} (CPU)"),
            VideoEncoder::Svt => "libsvtav1 (CPU)".into(),
            VideoEncoder::Aom => "libaom-av1 (CPU)".into(),
        }
    }
    /// Short output codec name (H264 / H265 / AV1).
    pub fn codec_name(&self) -> &'static str {
        match self {
            VideoEncoder::Nvenc("h264_nvenc") | VideoEncoder::X26x("libx264") => "H264",
            VideoEncoder::Nvenc("hevc_nvenc") | VideoEncoder::X26x("libx265") => "H265",
            VideoEncoder::X26x(_) => "H265",
            VideoEncoder::Nvenc(_) | VideoEncoder::Svt | VideoEncoder::Aom => "AV1",
        }
    }
}

/// Detects the best available encoder for `codec`.
/// First checks whether the GPU can actually encode that codec (a minimal encode
/// to null); if it fails (e.g. AV1 on Turing/RTX 20xx), it falls back to CPU.
pub fn detect_encoder(ffmpeg: &str, codec: Codec) -> VideoEncoder {
    let nv = codec.nvenc();
    if nvenc_works(ffmpeg, nv) {
        return VideoEncoder::Nvenc(nv);
    }
    let encs = list_encoders(ffmpeg);
    match codec {
        Codec::H264 => {
            if encs.contains("libx264") {
                VideoEncoder::X26x("libx264")
            } else {
                VideoEncoder::Nvenc(nv)
            }
        }
        Codec::H265 => {
            if encs.contains("libx265") {
                VideoEncoder::X26x("libx265")
            } else {
                VideoEncoder::Nvenc(nv)
            }
        }
        Codec::Av1 => {
            if encs.contains("libsvtav1") {
                VideoEncoder::Svt
            } else if encs.contains("libaom-av1") {
                VideoEncoder::Aom
            } else {
                VideoEncoder::Nvenc(nv)
            }
        }
    }
}

fn nvenc_works(ffmpeg: &str, encoder: &str) -> bool {
    let out = no_window(&mut Command::new(ffmpeg))
        .args([
            "-hide_banner", "-loglevel", "error",
            "-f", "lavfi", "-i", "color=c=black:s=256x256:r=30",
            "-frames:v", "1",
            "-c:v", encoder,
            "-f", "null", "-",
        ])
        .output();
    matches!(out, Ok(o) if o.status.success())
}

fn list_encoders(ffmpeg: &str) -> String {
    no_window(&mut Command::new(ffmpeg))
        .args(["-hide_banner", "-encoders"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase())
        .unwrap_or_default()
}

fn preset_num(preset: &str) -> i64 {
    preset.trim_start_matches(['p', 'P']).parse().unwrap_or(7)
}

/// The UI uses P1=best quality .. P7=fastest (same as ALL the CPU encoders). NVENC has
/// the scale REVERSED (p1=fastest, p7=slowest/best quality), so we invert it:
/// UI pN -> nvenc p(8-N)  ->  p1->p7 (best quality), p7->p1 (fastest). That way the slider
/// means the same thing on GPU and CPU. DO NOT remove the inversion: NVENC would be backwards.
fn nvenc_preset(n: i64) -> String {
    format!("p{}", (8 - n).clamp(1, 7))
}

/// Maps the UI preset number (p1=quality .. p7=speed) to an x264/x265 preset.
fn x26x_preset(n: i64) -> &'static str {
    match n {
        1 => "veryslow",
        2 => "slower",
        3 => "slow",
        4 => "medium",
        5 => "fast",
        6 => "faster",
        _ => "veryfast", // p7+
    }
}

/// Info read from the first video stream.
#[derive(Clone, Default)]
pub struct VideoInfo {
    pub w: i64,
    pub h: i64,
    pub hdr: bool,
    pub ten_bit: bool, // the source is >8 bits (P010/yuv420p10le, etc.)
    pub trc: String,
    pub primaries: String,
    pub space: String,
    pub range: String,
}

/// Applies CREATE_NO_WINDOW on Windows so probing doesn't pop up console windows.
fn no_window(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// Derives the ffprobe.exe path from the ffmpeg.exe path (same directory).
pub fn ffprobe_from_ffmpeg(ffmpeg: &str) -> PathBuf {
    let p = Path::new(ffmpeg);
    let dir = p.parent().unwrap_or_else(|| Path::new("."));
    #[cfg(windows)]
    let name = "ffprobe.exe";
    #[cfg(not(windows))]
    let name = "ffprobe";
    dir.join(name)
}

/// Finds video files in `dir` and ALL its subdirectories (recursive).
/// Skips output folders named "reencoded" so it doesn't re-encode finished files.
pub fn find_videos(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    find_videos_rec(dir, &mut out);
    out.sort();
    out
}

fn find_videos_rec(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.eq_ignore_ascii_case("reencoded") {
                continue; // don't descend into output folders
            }
            find_videos_rec(&path, out);
        } else if path.is_file() {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if EXTENSIONS.contains(&ext.to_lowercase().as_str()) {
                    out.push(path);
                }
            }
        }
    }
}

/// Reads resolution + color tags of the first video stream and decides if it's HDR.
pub fn probe_video(ffprobe: &Path, file: &Path) -> VideoInfo {
    let output = no_window(&mut Command::new(ffprobe))
        .args([
            "-v", "error",
            "-select_streams", "v:0",
            "-show_entries",
            "stream=width,height,pix_fmt,bits_per_raw_sample,color_space,color_transfer,color_primaries,color_range",
            "-of", "default=nk=0:nw=1",
            "-i",
        ])
        .arg(file)
        .output();

    let mut info = VideoInfo {
        range: "tv".into(),
        ..Default::default()
    };

    let Ok(out) = output else {
        return info;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut pix_fmt = String::new();
    let mut bits: i64 = 0;
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim().to_string();
        match k.trim() {
            "width" => info.w = v.parse().unwrap_or(0),
            "height" => info.h = v.parse().unwrap_or(0),
            "pix_fmt" => pix_fmt = v,
            "bits_per_raw_sample" => bits = v.parse().unwrap_or(0),
            "color_transfer" => info.trc = v,
            "color_primaries" => info.primaries = v,
            "color_space" => info.space = v,
            "color_range" => info.range = v,
            _ => {}
        }
    }

    // bit depth: bits_per_raw_sample is the most reliable; if missing, look at pix_fmt
    info.ten_bit = if bits > 0 {
        bits >= 10
    } else {
        let p = pix_fmt.as_str();
        p.contains("p10") || p.contains("010") || p.contains("p12") || p.contains("p16")
    };

    info.hdr = info.trc == "smpte2084" || info.trc == "arib-std-b67";
    if info.hdr {
        if info.primaries.is_empty() || info.primaries == "unknown" {
            info.primaries = "bt2020".into();
        }
        if info.space.is_empty() || info.space == "unknown" {
            info.space = "bt2020nc".into();
        }
        if info.range.is_empty() || info.range == "unknown" {
            info.range = "tv".into();
        }
    }
    info
}

/// Channel count of the first audio stream (2 if it can't be read).
pub fn probe_channels(ffprobe: &Path, file: &Path) -> i64 {
    let output = no_window(&mut Command::new(ffprobe))
        .args([
            "-v", "error",
            "-select_streams", "a:0",
            "-show_entries", "stream=channels",
            "-of", "default=nk=1:nw=1",
            "-i",
        ])
        .arg(file)
        .output();

    if let Ok(out) = output {
        let text = String::from_utf8_lossy(&out.stdout);
        if let Some(first) = text.lines().next() {
            if let Ok(c) = first.trim().parse::<i64>() {
                if c > 0 {
                    return c;
                }
            }
        }
    }
    2
}

/// File bitrate in kbps. Tries the video stream bitrate and, if it's not
/// available (common in MKV), falls back to the container's total bitrate (works
/// as a safe cap). Returns 0 if it can't be determined.
pub fn probe_bitrate(ffprobe: &Path, file: &Path) -> i64 {
    if let Some(bps) = ffprobe_num(
        ffprobe,
        file,
        &["-select_streams", "v:0", "-show_entries", "stream=bit_rate"],
    ) {
        return bps / 1000;
    }
    if let Some(bps) = ffprobe_num(ffprobe, file, &["-show_entries", "format=bit_rate"]) {
        return bps / 1000;
    }
    0
}

fn ffprobe_num(ffprobe: &Path, file: &Path, entries: &[&str]) -> Option<i64> {
    let output = no_window(&mut Command::new(ffprobe))
        .args(["-v", "error"])
        .args(entries)
        .args(["-of", "default=nk=1:nw=1", "-i"])
        .arg(file)
        .output();
    let out = output.ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let v = text.lines().next()?.trim().parse::<i64>().ok()?;
    (v > 0).then_some(v)
}

/// Stream summary ready to display (fields already formatted).
#[derive(Clone, Default)]
pub struct StreamSummary {
    pub duration: f64, // seconds (0 if unknown)
    pub dur_str: String,
    pub v_codec: String,
    pub v_res: String,
    pub v_fps: String,
    pub v_bits: String,    // "10-bit" / "8-bit"
    pub v_fmt: String,     // pix_fmt
    pub v_bitrate: String, // "6667 kbps"
    pub a_codec: String,
    pub a_layout: String,
    pub a_rate: String,
    pub a_bitrate: String,
}

/// Full stream probe (video 0 + audio 0) + duration, for display in the UI.
pub fn probe_streams(ffprobe: &Path, file: &Path) -> StreamSummary {
    let mut s = StreamSummary::default();
    let output = no_window(&mut Command::new(ffprobe))
        .args([
            "-v", "error",
            "-show_entries",
            "format=duration:stream=index,codec_type,codec_name,width,height,pix_fmt,\
             r_frame_rate,sample_rate,channels,channel_layout,bit_rate",
            "-of", "default=noprint_wrappers=0:nokey=0",
            "-i",
        ])
        .arg(file)
        .output();
    let Ok(out) = output else {
        return s;
    };
    let text = String::from_utf8_lossy(&out.stdout);

    // section parser: [STREAM]...[/STREAM] and [FORMAT]...[/FORMAT]
    let mut section: Vec<(String, String)> = Vec::new();
    let mut in_stream = false;
    for line in text.lines() {
        let line = line.trim();
        match line {
            "[STREAM]" | "[FORMAT]" => {
                in_stream = true;
                section.clear();
            }
            "[/STREAM]" => {
                apply_stream(&mut s, &section);
                in_stream = false;
            }
            "[/FORMAT]" => {
                for (k, v) in &section {
                    if k == "duration" {
                        s.duration = v.parse().unwrap_or(0.0);
                    }
                }
                in_stream = false;
            }
            _ if in_stream => {
                if let Some((k, v)) = line.split_once('=') {
                    section.push((k.trim().to_string(), v.trim().to_string()));
                }
            }
            _ => {}
        }
    }

    s.dur_str = fmt_hms(s.duration);
    s
}

fn apply_stream(s: &mut StreamSummary, sec: &[(String, String)]) {
    let get = |key: &str| -> String {
        sec.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    match get("codec_type").as_str() {
        "video" if s.v_codec.is_empty() => {
            s.v_codec = get("codec_name").to_uppercase();
            let w = get("width");
            let h = get("height");
            s.v_res = format!("{w}x{h}");
            s.v_fps = fmt_fps(&get("r_frame_rate"));
            let pf = get("pix_fmt");
            s.v_bits = bits_label(&pf);
            s.v_fmt = pf;
            s.v_bitrate = bitrate_kbps(&get("bit_rate"));
        }
        "audio" if s.a_codec.is_empty() => {
            s.a_codec = get("codec_name").to_uppercase();
            let ch = get("channels");
            let lay = get("channel_layout");
            s.a_layout = if lay.is_empty() || lay == "unknown" {
                format!("{ch} ch")
            } else {
                format!("{lay} ({ch} ch)")
            };
            let sr = get("sample_rate");
            if !sr.is_empty() {
                s.a_rate = format!("{sr} Hz");
            }
            s.a_bitrate = bitrate_kbps(&get("bit_rate"));
        }
        _ => {}
    }
}

fn fmt_fps(r: &str) -> String {
    if let Some((n, d)) = r.split_once('/') {
        let n: f64 = n.parse().unwrap_or(0.0);
        let d: f64 = d.parse().unwrap_or(1.0);
        if d > 0.0 {
            return format!("{:.3}", n / d);
        }
    }
    r.to_string()
}

/// Bit depth from the pix_fmt.
fn bits_label(pf: &str) -> String {
    if pf.contains("p10") || pf.contains("010") {
        "10-bit".into()
    } else if pf.contains("p12") || pf.contains("012") {
        "12-bit".into()
    } else if pf.is_empty() {
        String::new()
    } else {
        "8-bit".into()
    }
}

/// "12345678" (bits/s) -> "12346 kbps". Empty if there's no data.
fn bitrate_kbps(raw: &str) -> String {
    let bps: f64 = raw.trim().parse().unwrap_or(0.0);
    if bps > 0.0 {
        format!("{:.0} kbps", bps / 1000.0)
    } else {
        String::new()
    }
}

/// Formats seconds as H:MM:SS.
pub fn fmt_hms(secs: f64) -> String {
    if secs <= 0.0 {
        return "—".into();
    }
    let s = secs as i64;
    format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// Computes the 16:9 -> 21:9 crop. Returns "W:H:0:Y" or None if nothing to crop.
/// Uses the 2.38888 ratio (3440x1440) to match the previous library.
pub fn calc_crop(w: i64, h: i64, mode: CropMode) -> Option<String> {
    if mode == CropMode::None {
        return None;
    }
    let mut new_h = ((w as f64) * 1440.0 / 3440.0).round() as i64;
    new_h -= new_h % 2; // even height
    if new_h >= h {
        return None; // already >= 21:9, nothing to crop
    }
    let cut = h - new_h;
    let mut y = match mode {
        CropMode::Centered => cut / 2,
        CropMode::OneThird => ((cut as f64) / 3.0).round() as i64,
        CropMode::None => return None,
    };
    y -= y % 2; // even offset
    Some(format!("{w}:{new_h}:0:{y}"))
}

/// A job ready to run: name, informational label and the ffmpeg args.
pub struct BuiltJob {
    pub name: String,
    pub info: String,
    pub args: Vec<String>,
    pub streams: StreamSummary, // input (source)
    pub target: StreamSummary,  // output (target)
}

/// Result of probing an input (local file or the shim's http URL).
pub struct ProbeResult {
    pub inf: VideoInfo,
    pub streams: StreamSummary,
    pub src_kbps: i64,
    pub channels: i64,
}

/// Probes resolution/HDR/bits, streams, bitrate and channels of `input`
/// (may be a local path or the shim's http URL). None if it can't be read.
pub fn probe_all(ffprobe: &Path, input: &str) -> Option<ProbeResult> {
    let p = Path::new(input);
    let inf = probe_video(ffprobe, p);
    if inf.w <= 0 || inf.h <= 0 {
        return None;
    }
    let src_kbps = probe_bitrate(ffprobe, p);
    let channels = probe_channels(ffprobe, p);
    let mut streams = probe_streams(ffprobe, p);
    if streams.v_bitrate.is_empty() && src_kbps > 0 {
        streams.v_bitrate = format!("{src_kbps} kbps");
    }
    Some(ProbeResult {
        inf,
        streams,
        src_kbps,
        channels,
    })
}

/// Builds the ffmpeg args `input -> output`. Returns (args, info, target_summary).
/// `input` may be a local path or an http URL (the daemon's shim).
pub fn build_args(
    input: &str,
    output: &str,
    p: &ProbeResult,
    crop_mode: CropMode,
    preset: &str,
    high_kbps: i64,
    low_kbps: i64,
    enc: VideoEncoder,
) -> (Vec<String>, String, StreamSummary) {
    let inf = &p.inf;
    // bitrate by resolution (longest side, so vertical 4K counts) capped by the source
    let target_kbps = if inf.w.max(inf.h) >= 3000 {
        high_kbps
    } else {
        low_kbps
    };
    let src = p.src_kbps;
    let capped = src > 0 && src < target_kbps;
    let bitrate = if capped { src } else { target_kbps };
    let maxrate = (bitrate as f64 * 1.5).round() as i64;
    let bufsize = bitrate * 2;

    let crop = calc_crop(inf.w, inf.h, crop_mode);
    let abr = std::cmp::max(192, p.channels * 64);

    let mut args: Vec<String> = Vec::new();
    // base. No -nostdin: this lets ffmpeg read 'q' from stdin and close the file cleanly.
    args.extend(["-hide_banner", "-y"].map(String::from));
    // GPU decode
    args.extend(["-hwaccel", "cuda"].map(String::from));
    args.push("-i".into());
    args.push(input.to_string());

    if let Some(ref c) = crop {
        args.push("-vf".into());
        args.push(format!("crop={c}"));
    }

    args.extend(["-map", "0:0", "-map", "0:1"].map(String::from));

    match enc {
        VideoEncoder::Nvenc(name) => {
            // NVENC (h264/hevc/av1): INVERTED preset (see nvenc_preset: p1=quality, p7=speed) + VBR
            args.extend(["-c:v", name, "-preset"].map(String::from));
            args.push(nvenc_preset(preset_num(preset)));
            args.extend(["-rc", "vbr"].map(String::from));
            args.push("-b:v".into());
            args.push(format!("{bitrate}k"));
            args.push("-maxrate".into());
            args.push(format!("{maxrate}k"));
            args.push("-bufsize".into());
            args.push(format!("{bufsize}k"));
        }
        VideoEncoder::X26x(name) => {
            // libx264/libx265: named preset + bitrate control
            args.extend(["-c:v", name].map(String::from));
            args.push("-preset".into());
            args.push(x26x_preset(preset_num(preset)).into());
            args.push("-b:v".into());
            args.push(format!("{bitrate}k"));
            args.push("-maxrate".into());
            args.push(format!("{maxrate}k"));
            args.push("-bufsize".into());
            args.push(format!("{bufsize}k"));
        }
        VideoEncoder::Svt => {
            // numeric preset 0(slow)-13(fast). pN -> N (p1->1 best ... p7->7).
            // It used to map to 10 (very fast): bad quality and undershot the bitrate.
            let svt = preset_num(preset).clamp(0, 13);
            args.extend(["-c:v", "libsvtav1"].map(String::from));
            args.push("-preset".into());
            args.push(svt.to_string());
            // SVT-AV1 in bitrate (VBR) mode does NOT accept -maxrate (only in CRF)
            args.push("-b:v".into());
            args.push(format!("{bitrate}k"));
        }
        VideoEncoder::Aom => {
            // cpu-used 0(slow)-8(fast); pN -> N-1 (p1->0 best ... p7->6)
            let cpu = (preset_num(preset) - 1).clamp(0, 8);
            args.extend(["-c:v", "libaom-av1", "-row-mt", "1"].map(String::from));
            args.push("-cpu-used".into());
            args.push(cpu.to_string());
            args.push("-b:v".into());
            args.push(format!("{bitrate}k"));
            args.push("-maxrate".into());
            args.push(format!("{maxrate}k"));
            args.push("-bufsize".into());
            args.push(format!("{bufsize}k"));
        }
    }

    // output format/bits based on source + encoder (h264_nvenc doesn't support 10-bit)
    let (tgt_pf, tgt_bits): (&str, &str) = if inf.ten_bit {
        match enc {
            VideoEncoder::Nvenc("h264_nvenc") => ("nv12", "8-bit"),
            VideoEncoder::Nvenc(_) => ("p010le", "10-bit"),
            _ => ("yuv420p10le", "10-bit"),
        }
    } else {
        match enc {
            VideoEncoder::Nvenc(_) => ("nv12", "8-bit"),
            _ => ("yuv420p", "8-bit"),
        }
    };
    // force pix_fmt only when preserving 10 bits (otherwise the encoder uses its default)
    if tgt_bits == "10-bit" {
        args.push("-pix_fmt".into());
        args.push(tgt_pf.into());
    }

    if inf.hdr {
        args.push("-color_primaries".into());
        args.push(inf.primaries.clone());
        args.push("-color_trc".into());
        args.push(inf.trc.clone());
        args.push("-colorspace".into());
        args.push(inf.space.clone());
        args.push("-color_range".into());
        args.push(inf.range.clone());
    }

    args.push("-af".into());
    args.push("aformat=channel_layouts=7.1|5.1|stereo|mono".into());

    args.extend(["-c:a", "aac"].map(String::from));
    args.push("-b:a".into());
    args.push(format!("{abr}k"));
    args.extend(["-movflags", "+faststart"].map(String::from));
    args.push(output.to_string());

    let keeps_10 = inf.ten_bit && !matches!(enc, VideoEncoder::Nvenc("h264_nvenc"));
    let info = format!(
        "{}x{} -> {}k{} · {}{}{}{}",
        inf.w,
        inf.h,
        bitrate,
        if capped { " (orig)" } else { "" },
        enc.label(),
        if keeps_10 {
            " 10-bit"
        } else if inf.ten_bit {
            " 8-bit(!)"
        } else {
            ""
        },
        crop.as_ref().map(|c| format!(" crop[{c}]")).unwrap_or_default(),
        if inf.hdr { " HDR" } else { "" }
    );

    let streams = &p.streams;
    // output resolution (the crop changes it)
    let tgt_res = match &crop {
        Some(c) => {
            let parts: Vec<&str> = c.split(':').collect();
            if parts.len() >= 2 {
                format!("{}x{}", parts[0], parts[1])
            } else {
                streams.v_res.clone()
            }
        }
        None => streams.v_res.clone(),
    };

    let target = StreamSummary {
        duration: streams.duration,
        dur_str: streams.dur_str.clone(),
        v_codec: enc.codec_name().to_string(),
        v_res: tgt_res,
        v_fps: streams.v_fps.clone(),
        v_bits: tgt_bits.to_string(),
        v_fmt: tgt_pf.to_string(),
        v_bitrate: format!("{bitrate} kbps"),
        a_codec: "AAC".to_string(),
        a_layout: streams.a_layout.clone(),
        a_rate: streams.a_rate.clone(),
        a_bitrate: format!("{abr} kbps"),
    };

    (args, info, target)
}

/// Builds a full job for a LOCAL FILE (used by the GUI). Output in `<dir>/reencoded/`.
pub fn build_job(
    ffprobe: &Path,
    file: &Path,
    crop_mode: CropMode,
    preset: &str,
    high_kbps: i64,
    low_kbps: i64,
    enc: VideoEncoder,
) -> Option<BuiltJob> {
    let probe = probe_all(ffprobe, &file.to_string_lossy())?;

    let stem = file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output")
        .to_string();
    let final_dir = file.parent().unwrap_or_else(|| Path::new(".")).join("reencoded");
    let _ = std::fs::create_dir_all(&final_dir);
    let out_path = final_dir.join(format!("{stem}.mp4"));

    let (args, info, target) = build_args(
        &file.to_string_lossy(),
        &out_path.to_string_lossy(),
        &probe,
        crop_mode,
        preset,
        high_kbps,
        low_kbps,
        enc,
    );

    Some(BuiltJob {
        name: stem,
        info,
        args,
        streams: probe.streams,
        target,
    })
}
