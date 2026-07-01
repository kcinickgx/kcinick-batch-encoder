// fast6 GUI: Rust port of fast6.php with egui.
// Shrinks large sources to AV1 (NVENC), optional 16:9 -> 21:9 crop, audio and HDR.
// Model: a QUEUE of files + 4 encoder SLOTS. The scheduler dispatches the next file
// to the first free slot while fewer are running than the chosen number of "encoders"
// (adjustable LIVE). Each slot is an embedded terminal (live output + keys to ffmpeg).
// A process is never killed: 'q'.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod encoder;

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

use encoder::CropMode;

#[allow(dead_code)] // proto is shared with the daemon; the client doesn't use all of it (write_result)
mod proto;
use proto::{C2D, D2C};

/// Working mode: local ffmpeg or remote daemon.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Local,
    Daemon,
}

/// Outbound message to the daemon client's writer.
enum OutMsg {
    Ctrl(C2D),
    Block(u64, Vec<u8>),
}

/// Reads a block from a local file (to serve the daemon's ReadBlocks).
fn read_file_block(path: &std::path::Path, offset: u64, len: u32) -> std::io::Result<Vec<u8>> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let want = len as usize;
    let mut buf = vec![0u8; want];
    let mut got = 0;
    while got < want {
        let n = f.read(&mut buf[got..])?;
        if n == 0 {
            break;
        }
        got += n;
    }
    buf.truncate(got);
    Ok(buf)
}

/// Sends a key to a slot: to the daemon (daemon mode) or to the local ffmpeg (local mode).
fn send_key(slot: &Slot, daemon_mode: bool, dtx: &Option<Sender<OutMsg>>, key: &str) {
    if daemon_mode {
        let jid = slot.job_id.lock().unwrap().clone();
        if let (Some(tx), Some(jid)) = (dtx, jid) {
            let _ = tx.send(OutMsg::Ctrl(C2D::Key {
                job_id: jid,
                key: key.to_string(),
            }));
        }
    } else {
        slot.send_bytes(key.as_bytes());
    }
}

/// Converts a daemon SummaryDto to a StreamSummary for display in the slots.
fn dto_to_summary(d: &proto::SummaryDto) -> encoder::StreamSummary {
    encoder::StreamSummary {
        duration: d.dur_sec,
        dur_str: d.dur.clone(),
        v_codec: d.v_codec.clone(),
        v_res: d.v_res.clone(),
        v_fps: d.v_fps.clone(),
        v_bits: d.v_bits.clone(),
        v_fmt: d.v_fmt.clone(),
        v_bitrate: d.v_bitrate.clone(),
        a_codec: d.a_codec.clone(),
        a_layout: d.a_layout.clone(),
        a_rate: d.a_rate.clone(),
        a_bitrate: d.a_bitrate.clone(),
    }
}

const PRESETS: [&str; 7] = ["p1", "p2", "p3", "p4", "p5", "p6", "p7"];
const SLOTS: usize = 4;
// (high_kbps, low_kbps, label): high = 4K (>=3000px), low = the rest
const BITRATES: [(i64, i64, &str); 4] = [
    (10000, 8000, "10k/8k"),
    (8000, 6000, "8k/6k"),
    (6000, 4000, "6k/4k"),
    (4000, 2000, "4k/2k"),
];

const DEFAULT_FFMPEG: &str = r"C:\Util\ffmpeg\bin\ffmpeg.exe";
const REG_SUBKEY: &str = r"Software\KciNicK Batch Encoder";
const CODECS: [encoder::Codec; 3] = [
    encoder::Codec::H264,
    encoder::Codec::H265,
    encoder::Codec::Av1,
];

/// Registry access via the Windows API (UTF-16). Avoids the code-page problem of
/// `reg.exe`, whose text output would mangle accents/ñ when parsed.
mod reg {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyW, RegGetValueW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, REG_SZ,
        RRF_RT_REG_SZ,
    };

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    pub fn load(subkey: &str, value: &str) -> Option<String> {
        let sk = wide(subkey);
        let v = wide(value);
        let mut buf = [0u16; 4096];
        let mut len: u32 = std::mem::size_of_val(&buf) as u32; // bytes available
        let rc = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                sk.as_ptr(),
                v.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut len,
            )
        };
        if rc != ERROR_SUCCESS || len < 2 {
            return None;
        }
        let n = (len as usize) / 2; // u16 count including trailing NUL
        let s = String::from_utf16_lossy(&buf[..n.saturating_sub(1)]);
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    pub fn save(subkey: &str, value: &str, data: &str) {
        let sk = wide(subkey);
        let v = wide(value);
        let d = wide(data); // includes NUL
        unsafe {
            let mut hkey: HKEY = std::ptr::null_mut();
            let rc = RegCreateKeyW(HKEY_CURRENT_USER, sk.as_ptr(), &mut hkey);
            if rc != ERROR_SUCCESS {
                return;
            }
            let bytes = d.len() * 2;
            RegSetValueExW(hkey, v.as_ptr(), 0, REG_SZ, d.as_ptr().cast(), bytes as u32);
            RegCloseKey(hkey);
        }
    }
}

/// Reads a string value from the registry (HKCU). None if it doesn't exist.
fn reg_load(val: &str) -> Option<String> {
    reg::load(REG_SUBKEY, val)
}

/// Saves a string value to the registry (HKCU). Silent on errors.
fn reg_save(val: &str, data: &str) {
    reg::save(REG_SUBKEY, val, data);
}

#[derive(Clone, PartialEq)]
enum SlotState {
    Idle,
    Running,
    Fail(i32),
}

impl SlotState {
    fn label(&self) -> String {
        match self {
            SlotState::Idle => "idle".into(),
            SlotState::Running => "running".into(),
            SlotState::Fail(c) => format!("FAIL {c}"),
        }
    }
    fn color(&self) -> egui::Color32 {
        match self {
            SlotState::Idle => egui::Color32::GRAY,
            SlotState::Running => egui::Color32::from_rgb(90, 170, 255),
            SlotState::Fail(_) => egui::Color32::from_rgb(230, 110, 110),
        }
    }
}

/// An encoder slot (encode1..4). State shared between UI and controller.
struct Slot {
    state: Arc<Mutex<SlotState>>,
    name: Arc<Mutex<String>>,    // file in progress (or last)
    info: Arc<Mutex<String>>,    // resolution/bitrate/etc
    cmdline: Arc<Mutex<String>>, // ffmpeg command
    output: Arc<Mutex<String>>,  // raw stderr (only shown on FAIL)
    streams: Arc<Mutex<encoder::StreamSummary>>, // input streams (source)
    target: Arc<Mutex<encoder::StreamSummary>>,  // output streams (target)
    progress: Arc<Mutex<Option<Progress>>>,      // last parsed progress line
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    pid: Arc<Mutex<Option<u32>>>, // PID of the running ffmpeg (which I launched)
    job_id: Arc<Mutex<Option<String>>>, // daemon job shown in this slot (daemon mode)
}

impl Slot {
    fn new() -> Self {
        Slot {
            state: Arc::new(Mutex::new(SlotState::Idle)),
            name: Arc::new(Mutex::new(String::new())),
            info: Arc::new(Mutex::new(String::new())),
            cmdline: Arc::new(Mutex::new(String::new())),
            output: Arc::new(Mutex::new(String::new())),
            streams: Arc::new(Mutex::new(encoder::StreamSummary::default())),
            target: Arc::new(Mutex::new(encoder::StreamSummary::default())),
            progress: Arc::new(Mutex::new(None)),
            stdin: Arc::new(Mutex::new(None)),
            pid: Arc::new(Mutex::new(None)),
            job_id: Arc::new(Mutex::new(None)),
        }
    }

    fn reset(&self) {
        *self.state.lock().unwrap() = SlotState::Idle;
        self.name.lock().unwrap().clear();
        self.info.lock().unwrap().clear();
        self.cmdline.lock().unwrap().clear();
        self.output.lock().unwrap().clear();
        *self.streams.lock().unwrap() = encoder::StreamSummary::default();
        *self.target.lock().unwrap() = encoder::StreamSummary::default();
        *self.progress.lock().unwrap() = None;
        *self.stdin.lock().unwrap() = None;
        *self.pid.lock().unwrap() = None;
        *self.job_id.lock().unwrap() = None;
    }

    fn send_bytes(&self, b: &[u8]) {
        if let Some(stdin) = self.stdin.lock().unwrap().as_mut() {
            let _ = stdin.write_all(b);
            let _ = stdin.flush();
        }
    }

    fn send_quit(&self) {
        self.send_bytes(b"q");
    }
}

/// A parsed ffmpeg progress line (values ready to display).
#[derive(Clone, Default)]
struct Progress {
    frame: String,
    fps: String,
    q: String,
    size: String,
    time: String,
    bitrate: String,
    speed: String,
    time_sec: f64,  // current position in seconds (for ETA)
    speed_val: f64, // speed factor (for ETA)
}

/// Parses the line "frame=.. fps=.. q=.. size=.. time=.. bitrate=.. speed=..".
/// Handles ffmpeg's padding (e.g. "size= 3825664KiB" with a space).
fn parse_progress(line: &str) -> Option<Progress> {
    if !line.contains("frame=") || !line.contains("speed=") {
        return None;
    }
    let mut map: HashMap<&str, String> = HashMap::new();
    let mut it = line.split_whitespace().peekable();
    while let Some(tok) = it.next() {
        if let Some((k, v)) = tok.split_once('=') {
            let val = if v.is_empty() {
                it.next().unwrap_or("").to_string()
            } else {
                v.to_string()
            };
            map.insert(k, val);
        }
    }
    let get = |k: &str| map.get(k).cloned().unwrap_or_default();

    let time = get("time");
    let speed = get("speed");
    let time_sec = parse_clock(&time);
    let speed_val = speed.trim_end_matches('x').parse::<f64>().unwrap_or(0.0);

    Some(Progress {
        frame: get("frame"),
        fps: get("fps"),
        q: get("q"),
        size: fmt_size(&get("size")),
        time: encoder::fmt_hms(time_sec),
        bitrate: fmt_bitrate(&get("bitrate")),
        speed: if speed.is_empty() { "—".into() } else { speed },
        time_sec,
        speed_val,
    })
}

/// "01:18:20.43" -> seconds.
fn parse_clock(t: &str) -> f64 {
    let p: Vec<&str> = t.split(':').collect();
    if p.len() == 3 {
        let h: f64 = p[0].parse().unwrap_or(0.0);
        let m: f64 = p[1].parse().unwrap_or(0.0);
        let s: f64 = p[2].parse().unwrap_or(0.0);
        h * 3600.0 + m * 60.0 + s
    } else {
        0.0
    }
}

/// "3825664KiB" -> "3.65 GiB" / "3736 MiB".
fn fmt_size(raw: &str) -> String {
    let digits: String = raw
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let kib: f64 = digits.parse().unwrap_or(0.0);
    if kib <= 0.0 {
        return "—".into();
    }
    let mib = kib / 1024.0;
    if mib >= 1024.0 {
        format!("{:.2} GiB", mib / 1024.0)
    } else {
        format!("{mib:.0} MiB")
    }
}

/// "6667.4kbits/s" -> "6667 kbps".
fn fmt_bitrate(raw: &str) -> String {
    let digits: String = raw
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let kbps: f64 = digits.parse().unwrap_or(0.0);
    if kbps <= 0.0 {
        return "—".into();
    }
    format!("{kbps:.0} kbps")
}

/// Config captured when starting (fixed for the whole queue).
#[derive(Clone)]
struct RunCfg {
    ffmpeg: String,
    ffprobe: PathBuf,
    cwd: String,
    crop: CropMode,
    preset: String,
    high_kbps: i64,
    low_kbps: i64,
    codec: encoder::Codec,
}

struct App {
    mode: Mode,
    ffmpeg: String,
    daemon_addr: String,
    token: String,
    src_dir: String,
    crop: CropMode,
    parallel: usize, // chosen number of encoders (UI)
    preset_idx: usize,
    bitrate_idx: usize,
    codec_idx: usize,

    // daemon client
    daemon_tx: Arc<Mutex<Option<Sender<OutMsg>>>>,
    daemon_conn: Arc<Mutex<Option<TcpStream>>>, // clone to close the socket (disconnect)
    daemon_files: Arc<Mutex<HashMap<String, PathBuf>>>, // job_id -> local file
    slot_of: Arc<Mutex<HashMap<String, usize>>>,        // job_id -> slot
    job_counter: usize,

    files: Vec<PathBuf>,
    slots: Arc<Vec<Slot>>,
    queue: Arc<Mutex<VecDeque<PathBuf>>>,
    target: Arc<AtomicUsize>, // active encoders (live)
    running: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,

    selected_tab: usize,
    status_msg: String,
    needs_scan: bool,
    box_h: f32,
    sized_w: bool,    // width fixed once
    measured_h: f32,  // real content height (measured)
    confirm_exit: bool,
    closing: bool,
    force_close: bool,
}

impl Default for App {
    fn default() -> Self {
        let src_dir = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".into());
        App {
            mode: match reg_load("mode").as_deref() {
                Some("daemon") => Mode::Daemon,
                _ => Mode::Local,
            },
            ffmpeg: reg_load("ffmpeg").unwrap_or_else(|| DEFAULT_FFMPEG.into()),
            daemon_addr: reg_load("daemon_addr").unwrap_or_else(|| "127.0.0.1:7878".into()),
            token: reg_load("token").unwrap_or_else(|| "changeme".into()),
            src_dir,
            crop: CropMode::None,
            parallel: 1,
            preset_idx: 0, // p1 = best quality (default). With the p1=quality, p7=speed
            //                convention this default gives the same max-quality output as before.
            bitrate_idx: 2, // 6k/4k
            codec_idx: reg_load("codec")
                .and_then(|s| CODECS.iter().position(|c| c.label() == s))
                .unwrap_or(2), // AV1 by default
            daemon_tx: Arc::new(Mutex::new(None)),
            daemon_conn: Arc::new(Mutex::new(None)),
            daemon_files: Arc::new(Mutex::new(HashMap::new())),
            slot_of: Arc::new(Mutex::new(HashMap::new())),
            job_counter: 0,
            files: Vec::new(),
            slots: Arc::new((0..SLOTS).map(|_| Slot::new()).collect()),
            queue: Arc::new(Mutex::new(VecDeque::new())),
            target: Arc::new(AtomicUsize::new(1)),
            running: Arc::new(AtomicBool::new(false)),
            cancel: Arc::new(AtomicBool::new(false)),
            selected_tab: 0,
            status_msg: String::new(),
            needs_scan: true,
            box_h: 50.0,
            sized_w: false,
            measured_h: 0.0,
            confirm_exit: false,
            closing: false,
            force_close: false,
        }
    }
}

impl App {
    fn scan(&mut self) {
        let dir = PathBuf::from(&self.src_dir);
        self.files = encoder::find_videos(&dir);
    }

    /// How many slots are running right now.
    fn running_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| matches!(*s.state.lock().unwrap(), SlotState::Running))
            .count()
    }

    /// Clean shutdown: empties the queue and sends 'q' to the running ffmpegs (only mine).
    fn send_quit_all(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        for s in self.slots.iter() {
            if matches!(*s.state.lock().unwrap(), SlotState::Running) {
                s.send_quit();
            }
        }
    }

    fn start(&mut self) {
        if self.running.load(Ordering::SeqCst) {
            return;
        }
        if self.files.is_empty() {
            self.scan();
        }
        if self.files.is_empty() {
            self.status_msg = "No files to encode.".into();
            return;
        }

        if self.mode == Mode::Daemon {
            self.start_daemon();
            return;
        }

        // clean up accidental quotes/spaces in the ffmpeg path
        let clean = self.ffmpeg.trim().trim_matches('"').to_string();
        if clean != self.ffmpeg {
            self.ffmpeg = clean;
        }
        // validate that ffmpeg and ffprobe exist (otherwise a clear error, not a silent skip)
        let ffprobe = encoder::ffprobe_from_ffmpeg(&self.ffmpeg);
        if !std::path::Path::new(&self.ffmpeg).exists() {
            self.status_msg = format!("ffmpeg not found at: {}", self.ffmpeg);
            return;
        }
        if !ffprobe.exists() {
            self.status_msg = format!("ffprobe missing next to ffmpeg: {}", ffprobe.display());
            return;
        }

        // each file creates its own "reencoded" folder when the job is built
        self.status_msg.clear();
        // persist the chosen ffmpeg and codec for next time
        reg_save("ffmpeg", &self.ffmpeg);
        reg_save("codec", CODECS[self.codec_idx].label());

        // reset slots and fill the queue with all the files
        for s in self.slots.iter() {
            s.reset();
        }
        {
            let mut q = self.queue.lock().unwrap();
            q.clear();
            q.extend(self.files.iter().cloned());
        }

        let (high_kbps, low_kbps, _) = BITRATES[self.bitrate_idx];
        let cfg = RunCfg {
            ffmpeg: self.ffmpeg.clone(),
            ffprobe,
            cwd: self.src_dir.clone(),
            crop: self.crop,
            preset: PRESETS[self.preset_idx].to_string(),
            high_kbps,
            low_kbps,
            codec: CODECS[self.codec_idx],
        };

        self.target.store(self.parallel.max(1), Ordering::SeqCst);
        self.cancel.store(false, Ordering::SeqCst);
        self.running.store(true, Ordering::SeqCst);
        self.selected_tab = 0;

        spawn_controller(
            Arc::clone(&self.slots),
            Arc::clone(&self.queue),
            Arc::clone(&self.target),
            Arc::clone(&self.running),
            Arc::clone(&self.cancel),
            cfg,
        );
    }

    /// Connects to the daemon (Hello + auth) and starts the writer/reader threads.
    /// Returns true if it ended up connected.
    fn connect_daemon(&mut self) -> bool {
        if self.daemon_tx.lock().unwrap().is_some() {
            return true; // already connected
        }
        self.status_msg.clear(); // clear the previous error before retrying
        let addr = self.daemon_addr.trim().to_string();
        let mut rd = match TcpStream::connect(&addr) {
            Ok(s) => s,
            Err(e) => {
                self.status_msg = format!("Couldn't connect to {addr}: {e}");
                return false;
            }
        };
        // TCP_NODELAY: without this, Nagle+delayed-ACK stalls the ReadBlock ping-pong over the
        // network (VM<->host) and the daemon's encode drops to ~2x. Over loopback it's unnoticeable.
        rd.set_nodelay(true).ok();
        let mut wr = match rd.try_clone() {
            Ok(w) => w,
            Err(e) => {
                self.status_msg = format!("clone socket: {e}");
                return false;
            }
        };

        // Hello
        let hello = C2D::Hello {
            token: self.token.clone(),
            name: "fast6-gui".into(),
        };
        if proto::write_json(&mut wr, &hello).is_err() {
            self.status_msg = "failed sending Hello".into();
            return false;
        }
        // wait for Welcome / Denied
        let (_, payload) = match proto::read_frame(&mut rd) {
            Ok(x) => x,
            Err(_) => {
                self.status_msg = "no response from the daemon".into();
                return false;
            }
        };
        match serde_json::from_slice::<D2C>(&payload) {
            Ok(D2C::Welcome { .. }) => {}
            Ok(D2C::Denied { msg }) => {
                self.status_msg = format!("daemon refused: {msg}");
                return false;
            }
            _ => {
                self.status_msg = "unexpected response from the daemon".into();
                return false;
            }
        }

        // writer thread
        let (tx, rx) = std::sync::mpsc::channel::<OutMsg>();
        std::thread::spawn(move || {
            for m in rx {
                let r = match m {
                    OutMsg::Ctrl(c) => proto::write_json(&mut wr, &c),
                    OutMsg::Block(req, data) => proto::write_block(&mut wr, req, &data),
                };
                if r.is_err() {
                    break;
                }
            }
        });
        *self.daemon_tx.lock().unwrap() = Some(tx.clone());
        // keep a clone of the socket so Disconnect can close it
        *self.daemon_conn.lock().unwrap() = rd.try_clone().ok();

        // reader thread
        let slots = Arc::clone(&self.slots);
        let files = Arc::clone(&self.daemon_files);
        let slot_of = Arc::clone(&self.slot_of);
        let running = Arc::clone(&self.running);
        let dtx = Arc::clone(&self.daemon_tx);
        let dconn = Arc::clone(&self.daemon_conn);
        std::thread::spawn(move || {
            daemon_reader(rd, &slots, &files, &slot_of, &tx);
            // on disconnect
            *dtx.lock().unwrap() = None;
            *dconn.lock().unwrap() = None;
            running.store(false, Ordering::SeqCst);
        });
        true
    }

    /// Closes the connection to the daemon (Disconnect).
    fn disconnect_daemon(&mut self) {
        if let Some(sock) = self.daemon_conn.lock().unwrap().take() {
            let _ = sock.shutdown(std::net::Shutdown::Both);
        }
        *self.daemon_tx.lock().unwrap() = None;
        self.running.store(false, Ordering::SeqCst);
        // clear the UI: once the socket is closed, no more states arrive to reset the slots
        for s in self.slots.iter() {
            s.reset();
        }
        self.slot_of.lock().unwrap().clear();
        self.daemon_files.lock().unwrap().clear();
    }

    /// Starts a daemon-mode session: connects, sets parallelism and submits everything.
    fn start_daemon(&mut self) {
        if !self.connect_daemon() {
            return;
        }
        reg_save("mode", "daemon");
        reg_save("daemon_addr", &self.daemon_addr);
        reg_save("token", &self.token);
        reg_save("codec", CODECS[self.codec_idx].label());

        for s in self.slots.iter() {
            s.reset();
        }
        self.slot_of.lock().unwrap().clear();
        self.daemon_files.lock().unwrap().clear();
        self.selected_tab = 0;
        self.status_msg.clear();

        let tx = self.daemon_tx.lock().unwrap().clone();
        let Some(tx) = tx else {
            return;
        };
        let _ = tx.send(OutMsg::Ctrl(C2D::SetParallel { n: self.parallel.max(1) }));

        for path in self.files.clone() {
            let job_id = format!("j{}", self.job_counter);
            self.job_counter += 1;
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let filename = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("file")
                .to_string();
            self.daemon_files.lock().unwrap().insert(job_id.clone(), path.clone());
            let _ = tx.send(OutMsg::Ctrl(C2D::Submit {
                job_id,
                filename,
                size,
                crop: self.crop as u8,
                codec: CODECS[self.codec_idx].label().to_string(),
                preset: PRESETS[self.preset_idx].to_string(),
                bitrate_idx: self.bitrate_idx,
                local_path: None, // we serve the blocks on demand from the client
            }));
        }
        self.running.store(true, Ordering::SeqCst);
    }

    /// In daemon mode: sends Cancel to all pending jobs.
    fn daemon_cancel_all(&self) {
        if let Some(tx) = self.daemon_tx.lock().unwrap().clone() {
            let ids: Vec<String> = self.daemon_files.lock().unwrap().keys().cloned().collect();
            for id in ids {
                let _ = tx.send(OutMsg::Ctrl(C2D::Cancel { job_id: id }));
            }
        }
        // free the UI: you can hit Play or Disconnect again without disconnecting
        self.running.store(false, Ordering::SeqCst);
    }
}

/// Assigns (or returns) a job's slot. None if there's no free slot.
fn ensure_slot(slot_of: &Mutex<HashMap<String, usize>>, job_id: &str) -> Option<usize> {
    let mut map = slot_of.lock().unwrap();
    if let Some(&i) = map.get(job_id) {
        return Some(i);
    }
    let used: std::collections::HashSet<usize> = map.values().copied().collect();
    for i in 0..SLOTS {
        if !used.contains(&i) {
            map.insert(job_id.to_string(), i);
            return Some(i);
        }
    }
    None
}

/// Where the client saves the result: `<source folder>/reencoded/<name>.mp4`.
fn client_out_path(src: &std::path::Path) -> PathBuf {
    let dir = src
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("reencoded");
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("output");
    dir.join(format!("{stem}.mp4"))
}

/// Reads the daemon's messages and pours them into the GUI slots.
fn daemon_reader(
    mut rd: TcpStream,
    slots: &Arc<Vec<Slot>>,
    files: &Arc<Mutex<HashMap<String, PathBuf>>>,
    slot_of: &Arc<Mutex<HashMap<String, usize>>>,
    tx: &Sender<OutMsg>,
) {
    // open result files, by job_id (written as the chunks arrive)
    let mut result_files: HashMap<String, File> = HashMap::new();
    loop {
        let (tag, payload) = match proto::read_frame(&mut rd) {
            Ok(x) => x,
            Err(_) => break,
        };
        // chunk of the result file → write to the local .mp4
        if tag == proto::TAG_RESULT {
            if let Some((job_id, data)) = proto::parse_result(&payload) {
                if let Some(f) = result_files.get_mut(&job_id) {
                    let _ = f.write_all(data);
                }
            }
            continue;
        }
        if tag != proto::TAG_CTRL {
            continue;
        }
        let msg: D2C = match serde_json::from_slice(&payload) {
            Ok(m) => m,
            Err(_) => continue,
        };
        match msg {
            D2C::ReadBlock { req_id, job_id, offset, len } => {
                let path = files.lock().unwrap().get(&job_id).cloned();
                let data = path
                    .and_then(|p| read_file_block(&p, offset, len).ok())
                    .unwrap_or_default();
                let _ = tx.send(OutMsg::Block(req_id, data));
            }
            D2C::State { job_id, state, detail } => {
                let fname = files
                    .lock()
                    .unwrap()
                    .get(&job_id)
                    .and_then(|p| p.file_name().and_then(|s| s.to_str()).map(String::from))
                    .unwrap_or_default();
                if state == "done" || state == "cancelled" {
                    if let Some(i) = slot_of.lock().unwrap().remove(&job_id) {
                        slots[i].reset();
                    }
                    files.lock().unwrap().remove(&job_id);
                } else if state == "queued" {
                    // still without a slot
                } else if let Some(i) = ensure_slot(slot_of, &job_id) {
                    let slot = &slots[i];
                    *slot.job_id.lock().unwrap() = Some(job_id.clone());
                    *slot.name.lock().unwrap() = fname;
                    match state.as_str() {
                        "probing" => {
                            *slot.state.lock().unwrap() = SlotState::Running;
                            *slot.info.lock().unwrap() = "probing…".into();
                        }
                        "encoding" => {
                            *slot.state.lock().unwrap() = SlotState::Running;
                            *slot.info.lock().unwrap() = detail.clone();
                        }
                        "failed" => {
                            *slot.state.lock().unwrap() = SlotState::Fail(-1);
                            *slot.info.lock().unwrap() = detail.clone();
                            slot_of.lock().unwrap().remove(&job_id);
                            files.lock().unwrap().remove(&job_id);
                        }
                        _ => {}
                    }
                }
            }
            D2C::Streams { job_id, source, target, cmd } => {
                if let Some(i) = slot_of.lock().unwrap().get(&job_id).copied() {
                    *slots[i].streams.lock().unwrap() = dto_to_summary(&source);
                    *slots[i].target.lock().unwrap() = dto_to_summary(&target);
                    if !cmd.is_empty() {
                        *slots[i].cmdline.lock().unwrap() = cmd;
                    }
                }
            }
            D2C::Progress {
                job_id,
                frame,
                fps,
                q,
                size,
                time,
                bitrate,
                speed,
                time_sec,
                ..
            } => {
                if let Some(i) = slot_of.lock().unwrap().get(&job_id).copied() {
                    let speed_val = speed.trim_end_matches('x').parse::<f64>().unwrap_or(0.0);
                    *slots[i].progress.lock().unwrap() = Some(Progress {
                        frame,
                        fps,
                        q,
                        size,
                        time,
                        bitrate,
                        speed,
                        time_sec,
                        speed_val,
                    });
                }
            }
            D2C::ResultBegin { job_id } => {
                // the daemon is about to start sending the .mp4 → open the local file
                if let Some(src) = files.lock().unwrap().get(&job_id).cloned() {
                    let out = client_out_path(&src);
                    if let Some(parent) = out.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if let Ok(f) = File::create(&out) {
                        result_files.insert(job_id, f);
                    }
                }
            }
            D2C::ResultEnd { job_id, .. } => {
                if let Some(mut f) = result_files.remove(&job_id) {
                    let _ = f.flush();
                }
            }
            _ => {}
        }
    }
}

/// Scheduler: dispatches from the queue to the slots honoring `target` (live).
fn spawn_controller(
    slots: Arc<Vec<Slot>>,
    queue: Arc<Mutex<VecDeque<PathBuf>>>,
    target: Arc<AtomicUsize>,
    running: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    cfg: RunCfg,
) {
    std::thread::spawn(move || {
        // detect once whether the GPU encodes the chosen codec; if not, CPU
        let enc = encoder::detect_encoder(&cfg.ffmpeg, cfg.codec);
        let mut children: Vec<Option<Child>> = (0..SLOTS).map(|_| None).collect();

        loop {
            // 1) reap finished ones
            for i in 0..SLOTS {
                if let Some(child) = &mut children[i] {
                    if let Ok(Some(st)) = child.try_wait() {
                        let code = st.code().unwrap_or(-1);
                        if code == 0 {
                            // finished OK: clean up and go back to "idle"
                            slots[i].reset();
                        } else {
                            *slots[i].state.lock().unwrap() = SlotState::Fail(code);
                            *slots[i].stdin.lock().unwrap() = None;
                            *slots[i].pid.lock().unwrap() = None;
                        }
                        children[i] = None;
                    }
                }
            }

            let cancelling = cancel.load(Ordering::SeqCst);

            // 2) if cancelling: empty the queue and send 'q' to the live ones (clean shutdown)
            if cancelling {
                queue.lock().unwrap().clear();
                for i in 0..SLOTS {
                    if children[i].is_some() {
                        slots[i].send_quit();
                    }
                }
            }

            // 3) dispatch while fewer than `target` are running and there's a free slot < target
            if !cancelling {
                loop {
                    let tgt = target.load(Ordering::SeqCst).clamp(1, SLOTS);
                    let running_count = children.iter().filter(|c| c.is_some()).count();
                    if running_count >= tgt {
                        break;
                    }
                    let Some(idx) = (0..tgt).find(|&i| children[i].is_none()) else {
                        break;
                    };
                    let Some(file) = queue.lock().unwrap().pop_front() else {
                        break;
                    };
                    match encoder::build_job(
                        &cfg.ffprobe,
                        &file,
                        cfg.crop,
                        &cfg.preset,
                        cfg.high_kbps,
                        cfg.low_kbps,
                        enc,
                    ) {
                        Some(built) => match start_on_slot(&slots[idx], &cfg, built) {
                            Ok(child) => children[idx] = Some(child),
                            Err(e) => {
                                *slots[idx].state.lock().unwrap() = SlotState::Fail(-1);
                                slots[idx]
                                    .output
                                    .lock()
                                    .unwrap()
                                    .push_str(&format!("\n[error launching ffmpeg] {e}\n"));
                            }
                        },
                        None => {
                            // unreadable file (ffprobe returned no resolution): show FAIL
                            let nm = file
                                .file_name()
                                .and_then(|s| s.to_str())
                                .unwrap_or("???")
                                .to_string();
                            *slots[idx].name.lock().unwrap() = nm;
                            *slots[idx].info.lock().unwrap() =
                                "couldn't read (ffprobe)".into();
                            *slots[idx].cmdline.lock().unwrap() = String::new();
                            *slots[idx].state.lock().unwrap() = SlotState::Fail(-2);
                            continue;
                        }
                    }
                }
            }

            // 4) done: queue empty and nothing running
            let any_running = children.iter().any(|c| c.is_some());
            let queue_empty = queue.lock().unwrap().is_empty();
            if queue_empty && !any_running {
                break;
            }

            std::thread::sleep(Duration::from_millis(150));
        }

        running.store(false, Ordering::SeqCst);
    });
}

/// Prepares the slot and launches ffmpeg with pipes; starts the stderr reader.
fn start_on_slot(slot: &Slot, cfg: &RunCfg, built: encoder::BuiltJob) -> std::io::Result<Child> {
    slot.output.lock().unwrap().clear();
    *slot.progress.lock().unwrap() = None;
    *slot.name.lock().unwrap() = built.name;
    *slot.info.lock().unwrap() = built.info;
    *slot.streams.lock().unwrap() = built.streams;
    *slot.target.lock().unwrap() = built.target;
    // shortened command: "ffmpeg.exe ..." with {source}/{target} instead of the long paths
    let mut disp = String::from("ffmpeg.exe");
    let n = built.args.len();
    for (idx, a) in built.args.iter().enumerate() {
        let token = if idx > 0 && built.args[idx - 1] == "-i" {
            "{source}"
        } else if idx == n - 1 {
            "{target}"
        } else {
            a.as_str()
        };
        disp.push(' ');
        disp.push_str(token);
    }
    *slot.cmdline.lock().unwrap() = disp;
    *slot.state.lock().unwrap() = SlotState::Running;

    let mut cmd = Command::new(&cfg.ffmpeg);
    cmd.args(&built.args)
        .current_dir(&cfg.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let mut child = cmd.spawn()?;
    *slot.pid.lock().unwrap() = Some(child.id());
    *slot.stdin.lock().unwrap() = child.stdin.take();
    if let Some(stderr) = child.stderr.take() {
        let out = Arc::clone(&slot.output);
        let prog = Arc::clone(&slot.progress);
        std::thread::spawn(move || pump_output(stderr, out, prog));
    }
    Ok(child)
}

/// State of the mini ANSI-sequence parser.
enum Ansi {
    Normal,
    Esc,
    Csi,
    Osc,
}

/// Reads ffmpeg's stderr, drops ANSI, handles '\r' and parses the progress lines.
fn pump_output<R: Read>(
    mut reader: R,
    out: Arc<Mutex<String>>,
    prog: Arc<Mutex<Option<Progress>>>,
) {
    // Minimal terminal emulation: already-committed lines + current line with a cursor.
    // '\r' moves the cursor to the start (overwrites), it does NOT erase the line.
    let mut committed = String::new();
    let mut line: Vec<char> = Vec::new();
    let mut cursor = 0usize;
    let mut state = Ansi::Normal;
    let mut buf = [0u8; 4096];

    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let chunk = String::from_utf8_lossy(&buf[..n]);
                for ch in chunk.chars() {
                    match state {
                        Ansi::Normal => match ch {
                            '\x1b' => state = Ansi::Esc,
                            '\n' => {
                                // parse progress if this line is one (ffmpeg with \n)
                                let s: String = line.iter().collect();
                                if let Some(pr) = parse_progress(&s) {
                                    *prog.lock().unwrap() = Some(pr);
                                }
                                committed.extend(line.iter());
                                committed.push('\n');
                                line.clear();
                                cursor = 0;
                            }
                            '\r' => cursor = 0,
                            c if (c as u32) < 0x20 && c != '\t' => {}
                            c => {
                                if cursor < line.len() {
                                    line[cursor] = c;
                                } else {
                                    line.push(c);
                                }
                                cursor += 1;
                            }
                        },
                        Ansi::Esc => {
                            state = match ch {
                                '[' => Ansi::Csi,
                                ']' => Ansi::Osc,
                                _ => Ansi::Normal,
                            }
                        }
                        Ansi::Csi => {
                            // 'K' = erase to end of line (from the cursor)
                            if ch == 'K' {
                                line.truncate(cursor);
                                state = Ansi::Normal;
                            } else if ('@'..='~').contains(&ch) {
                                state = Ansi::Normal;
                            }
                        }
                        Ansi::Osc => match ch {
                            '\x07' => state = Ansi::Normal,
                            '\x1b' => state = Ansi::Esc,
                            _ => {}
                        },
                    }
                }

                // memory trimming (on a char boundary)
                if committed.len() > 200_000 {
                    let mut idx = committed.len() - 150_000;
                    while idx < committed.len() && !committed.is_char_boundary(idx) {
                        idx += 1;
                    }
                    committed = committed.split_off(idx);
                }

                // parse progress from the current line (ffmpeg with \r)
                let cur: String = line.iter().collect();
                if let Some(pr) = parse_progress(&cur) {
                    *prog.lock().unwrap() = Some(pr);
                }

                let mut full = committed.clone();
                full.extend(line.iter());
                *out.lock().unwrap() = full;
            }
            Err(_) => break,
        }
    }
}

/// A box with a title ON the border line (Windows GroupBox style).
/// Returns the box height.
fn titled_group(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) -> f32 {
    let strong = ui.visuals().strong_text_color();
    let bg = ui.visuals().panel_fill;

    let inner = egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.add_space(3.0);
        add(ui);
    });

    let rect = inner.response.rect;
    let painter = ui.painter().clone();
    let galley = painter.layout_no_wrap(title.to_string(), egui::FontId::proportional(12.5), strong);
    let sz = galley.size();
    let pos = egui::pos2(rect.left() + 8.0, rect.top() - sz.y * 0.5);
    let pad = 4.0;
    painter.rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(pos.x - pad, pos.y),
            egui::pos2(pos.x + sz.x + pad, pos.y + sz.y),
        ),
        0.0,
        bg,
    );
    painter.galley(pos, galley, strong);
    rect.height()
}

/// "label: value" row with the label right-aligned (fixed width)
/// and the value on the left, colored.
fn kv_row(ui: &mut egui::Ui, label_w: f32, key: &str, val: &str, color: egui::Color32) {
    let lbl = egui::Color32::from_rgb(150, 150, 160);
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(label_w, 18.0),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                ui.label(egui::RichText::new(format!("{key}:")).color(lbl));
            },
        );
        let v = if val.is_empty() { "—" } else { val };
        ui.label(egui::RichText::new(v).color(color).monospace());
    });
}

/// Drawn crop button: a 16:9 frame with the kept 21:9 band in light and the
/// cropped bars in dark, per the mode. Returns true if clicked.
fn crop_button(
    ui: &mut egui::Ui,
    w: f32,
    h: f32,
    selected: bool,
    mode: CropMode,
    tooltip: &str,
) -> bool {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click());

    let enabled = ui.is_enabled();
    let sel_bg = ui.visuals().selection.bg_fill;
    let hover_bg = ui.visuals().widgets.hovered.bg_fill;
    let painter = ui.painter();

    // button background (like a SelectableLabel)
    if selected {
        painter.rect_filled(rect, 4.0, sel_bg);
    } else if resp.hovered() {
        painter.rect_filled(rect, 4.0, hover_bg);
    }

    // centered 16:9 frame
    let gh = (h - 12.0).max(14.0);
    let gw = gh * 16.0 / 9.0;
    let gr = egui::Rect::from_center_size(rect.center(), egui::vec2(gw, gh));

    let dim = |c: egui::Color32| if enabled { c } else { c.gamma_multiply(0.4) };
    let removed = dim(egui::Color32::from_rgb(50, 50, 60));
    let kept = dim(if selected {
        egui::Color32::from_rgb(240, 244, 255)
    } else {
        egui::Color32::from_rgb(150, 195, 245)
    });
    let border = dim(egui::Color32::from_rgb(180, 180, 190));

    // the whole frame starts as the cropped area (dark)
    painter.rect_filled(gr, 2.0, removed);

    // kept band (21:9 over 16:9 ≈ 74.4% of the height; crop ≈ 25.6%)
    let total = gr.height();
    let (top_frac, keep_frac) = match mode {
        CropMode::Centered => (0.128, 0.744), // 12.8% top / 12.8% bottom
        CropMode::OneThird => (0.0853, 0.744), // 1/3 of the crop on top, 2/3 on the bottom
        CropMode::None => (0.0, 1.0),          // crops nothing
    };
    let keep_rect = egui::Rect::from_min_size(
        egui::pos2(gr.left(), gr.top() + total * top_frac),
        egui::vec2(gr.width(), total * keep_frac),
    );
    painter.rect_filled(keep_rect, if mode == CropMode::None { 2.0 } else { 0.0 }, kept);

    // frame border
    painter.rect_stroke(
        gr,
        2.0,
        egui::Stroke::new(1.0, border),
        egui::StrokeKind::Inside,
    );

    resp.on_hover_text(tooltip).clicked()
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let is_running = self.running.load(Ordering::SeqCst);

        // --- confirmed close + clean shutdown (Q) ---
        if ctx.input(|i| i.viewport().close_requested()) && !self.force_close {
            // ALWAYS persist on close (covers changing the path and quitting)
            reg_save("ffmpeg", &self.ffmpeg);
            reg_save("codec", CODECS[self.codec_idx].label());
            reg_save("mode", if self.mode == Mode::Daemon { "daemon" } else { "local" });
            reg_save("daemon_addr", &self.daemon_addr);
            reg_save("token", &self.token);
            if self.closing {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose); // already closing
            } else if self.running_count() > 0 {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.confirm_exit = true;
            }
            // no processes: close normally
        }

        if self.confirm_exit {
            egui::Modal::new(egui::Id::new("confirm_exit")).show(ctx, |ui| {
                ui.set_width(380.0);
                ui.heading("Quit?");
                ui.add_space(6.0);
                ui.label(format!(
                    "There are {} ffmpeg process(es) running. If you quit they'll be closed \
                     cleanly (Q) — it may take a few seconds.",
                    self.running_count()
                ));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(egui::RichText::new("Yes, quit").size(15.0))
                        .clicked()
                    {
                        self.send_quit_all();
                        self.confirm_exit = false;
                        self.closing = true;
                    }
                    if ui
                        .button(egui::RichText::new("No, keep going").size(15.0))
                        .clicked()
                    {
                        self.confirm_exit = false;
                    }
                });
            });
        }

        if self.closing {
            let n = self.running_count();
            egui::Modal::new(egui::Id::new("closing")).show(ctx, |ui| {
                ui.set_width(340.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.heading("Closing…");
                });
                ui.add_space(6.0);
                ui.label(format!(
                    "Waiting for {n} process(es) to close (Q, clean shutdown)…"
                ));
            });
            if n == 0 {
                self.force_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            ctx.request_repaint_after(Duration::from_millis(150));
        }

        if self.needs_scan && !is_running {
            self.scan();
            self.needs_scan = false;
        }

        let mut do_start = false;
        let mut row_w = 0.0f32;

        egui::TopBottomPanel::top("config").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.heading("KciNicK Batch Encoder");
            ui.add_space(6.0);

            const LABEL_W: f32 = 110.0;
            const BTN_W: f32 = 28.0;
            const ROW_H: f32 = 22.0;
            let input_w = (ui.available_width() - LABEL_W - BTN_W - 16.0).max(120.0);

            // Mode: Local / Remote
            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(LABEL_W, ROW_H),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        ui.label("Mode:");
                    },
                );
                ui.add_enabled_ui(!is_running, |ui| {
                    if ui
                        .selectable_label(self.mode == Mode::Local, "Local")
                        .clicked()
                    {
                        self.mode = Mode::Local;
                        reg_save("mode", "local");
                    }
                    if ui
                        .selectable_label(self.mode == Mode::Daemon, "Remote")
                        .clicked()
                    {
                        self.mode = Mode::Daemon;
                        reg_save("mode", "daemon");
                    }
                });
                if self.mode == Mode::Daemon {
                    if self.daemon_tx.lock().unwrap().is_some() {
                        ui.colored_label(egui::Color32::from_rgb(120, 220, 140), "● connected");
                    } else {
                        ui.label(egui::RichText::new("○ not connected").weak());
                    }
                    if !self.status_msg.is_empty() {
                        ui.colored_label(
                            egui::Color32::from_rgb(230, 110, 110),
                            &self.status_msg,
                        );
                    }
                }
            });
            ui.add_space(4.0);

            if self.mode == Mode::Local {
                // ffmpeg.exe
                ui.horizontal(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(LABEL_W, ROW_H),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.label("ffmpeg.exe:");
                        },
                    );
                    let resp = ui.add_enabled(
                        !is_running,
                        egui::TextEdit::singleline(&mut self.ffmpeg).desired_width(input_w),
                    );
                    if resp.lost_focus() {
                        reg_save("ffmpeg", &self.ffmpeg);
                    }
                    if ui.add_enabled(!is_running, egui::Button::new("…")).clicked() {
                        if let Some(p) = rfd::FileDialog::new()
                            .add_filter("ffmpeg", &["exe"])
                            .pick_file()
                        {
                            self.ffmpeg = p.to_string_lossy().into_owned();
                            reg_save("ffmpeg", &self.ffmpeg);
                        }
                    }
                });
            } else {
                // daemon: label aligned with ffmpeg/folder; addr + token 50/50 + connect button
                ui.horizontal(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(LABEL_W, ROW_H),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.label("server:");
                        },
                    );
                    let connected = self.daemon_tx.lock().unwrap().is_some();
                    let sp = ui.spacing().item_spacing.x;
                    let tok_lbl = 46.0;
                    let btn_w = 92.0;
                    let half =
                        ((ui.available_width() - tok_lbl - btn_w - 4.0 * sp) / 2.0).max(60.0);
                    let r1 = ui.add_enabled(
                        !connected,
                        egui::TextEdit::singleline(&mut self.daemon_addr).desired_width(half),
                    );
                    if r1.lost_focus() {
                        reg_save("daemon_addr", &self.daemon_addr);
                    }
                    ui.label("token:");
                    let r2 = ui.add_enabled(
                        !connected,
                        egui::TextEdit::singleline(&mut self.token).desired_width(half),
                    );
                    if r2.lost_focus() {
                        reg_save("token", &self.token);
                    }
                    if connected {
                        if ui
                            .add(egui::Button::new("Disconnect").min_size(egui::vec2(btn_w, ROW_H)))
                            .clicked()
                        {
                            self.disconnect_daemon();
                        }
                    } else if ui
                        .add(egui::Button::new("Connect").min_size(egui::vec2(btn_w, ROW_H)))
                        .clicked()
                    {
                        reg_save("daemon_addr", &self.daemon_addr);
                        reg_save("token", &self.token);
                        self.connect_daemon();
                    }
                });
            }
            ui.add_space(4.0);

            // Source folder
            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(LABEL_W, ROW_H),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        ui.label("Source folder:");
                    },
                );
                let resp = ui.add_enabled(
                    !is_running,
                    egui::TextEdit::singleline(&mut self.src_dir).desired_width(input_w),
                );
                if resp.changed() {
                    self.needs_scan = true;
                }
                if ui.add_enabled(!is_running, egui::Button::new("…")).clicked() {
                    if let Some(p) = rfd::FileDialog::new().pick_folder() {
                        self.src_dir = p.to_string_lossy().into_owned();
                        self.needs_scan = true;
                    }
                }
            });
            ui.add_space(8.0);

            // big buttons inside the boxes
            const FS: f32 = 22.0;
            const BH: f32 = 34.0;
            let sel = |ui: &mut egui::Ui, w: f32, on: bool, txt: &str| -> bool {
                ui.add_sized(
                    [w, BH],
                    egui::SelectableLabel::new(on, egui::RichText::new(txt).size(FS)),
                )
                .clicked()
            };

            let row1 = ui.horizontal_top(|ui| {
                // Play / Stop up front (same height as the boxes, from the previous frame)
                let btn = egui::vec2(self.box_h, self.box_h);
                ui.add_enabled_ui(!is_running, |ui| {
                    if ui
                        .add_sized(btn, egui::Button::new(egui::RichText::new("▶").size(24.0)))
                        .on_hover_text("Start")
                        .clicked()
                    {
                        do_start = true;
                    }
                });
                ui.add_enabled_ui(is_running, |ui| {
                    if ui
                        .add_sized(btn, egui::Button::new(egui::RichText::new("■").size(22.0)))
                        .on_hover_text("Cancel all")
                        .clicked()
                    {
                        if self.mode == Mode::Daemon {
                            self.daemon_cancel_all();
                        } else {
                            self.cancel.store(true, Ordering::SeqCst);
                        }
                    }
                });
                ui.add_space(8.0);

                let gh = titled_group(ui, "CROP", |ui| {
                    ui.add_enabled_ui(!is_running, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 5.0;
                            let cw = 64.0;
                            if crop_button(
                                ui,
                                cw,
                                BH,
                                self.crop == CropMode::Centered,
                                CropMode::Centered,
                                "Centered (equal bars)",
                            ) {
                                self.crop = CropMode::Centered;
                            }
                            if crop_button(
                                ui,
                                cw,
                                BH,
                                self.crop == CropMode::OneThird,
                                CropMode::OneThird,
                                "1/3 top, 2/3 bottom",
                            ) {
                                self.crop = CropMode::OneThird;
                            }
                            if crop_button(
                                ui,
                                cw,
                                BH,
                                self.crop == CropMode::None,
                                CropMode::None,
                                "No crop",
                            ) {
                                self.crop = CropMode::None;
                            }
                        });
                    });
                });
                self.box_h = gh;

                titled_group(ui, "CODEC", |ui| {
                    ui.add_enabled_ui(!is_running, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            for (i, c) in CODECS.iter().enumerate() {
                                if sel(ui, 58.0, self.codec_idx == i, c.label()) {
                                    self.codec_idx = i;
                                    reg_save("codec", c.label());
                                }
                            }
                        });
                    });
                });

                // ENCODERS: ALWAYS enabled (changed live)
                titled_group(ui, "ENCODERS", |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        for n in 1..=SLOTS {
                            if sel(ui, 38.0, self.parallel == n, &n.to_string()) {
                                self.parallel = n;
                                self.target.store(n, Ordering::SeqCst);
                                // in daemon mode, adjust parallelism live
                                if self.mode == Mode::Daemon {
                                    if let Some(tx) = self.daemon_tx.lock().unwrap().clone() {
                                        let _ = tx.send(OutMsg::Ctrl(C2D::SetParallel { n }));
                                    }
                                }
                            }
                        }
                    });
                });
            });

            ui.add_space(6.0);

            let row2 = ui.horizontal_top(|ui| {
                titled_group(ui, "PRESET", |ui| {
                    ui.add_enabled_ui(!is_running, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            for (i, p) in PRESETS.iter().enumerate() {
                                if sel(ui, 40.0, self.preset_idx == i, p) {
                                    self.preset_idx = i;
                                }
                            }
                        });
                    });
                });

                titled_group(ui, "BITRATE", |ui| {
                    ui.add_enabled_ui(!is_running, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            for (i, (_, _, label)) in BITRATES.iter().enumerate() {
                                if sel(ui, 76.0, self.bitrate_idx == i, label) {
                                    self.bitrate_idx = i;
                                }
                            }
                        });
                    });
                });

                // QUEUE: pending (local queue in local mode, jobs in daemon mode)
                let qn = if self.mode == Mode::Daemon {
                    self.daemon_files.lock().unwrap().len()
                } else {
                    self.queue.lock().unwrap().len()
                };
                titled_group(ui, "QUEUE", |ui| {
                    ui.add_sized(
                        [54.0, BH],
                        egui::Label::new(egui::RichText::new(qn.to_string()).size(FS)),
                    );
                });
            });

            row_w = row1.response.rect.width().max(row2.response.rect.width());

            // status line: errors (in Remote mode the message goes up top, next to the indicator)
            if !self.status_msg.is_empty() && self.mode == Mode::Local {
                ui.add_space(4.0);
                ui.colored_label(egui::Color32::from_rgb(230, 110, 110), &self.status_msg);
            }
            ui.add_space(4.0);
        });

        if do_start {
            self.start();
        }

        // daemon: if no jobs remain, free the Play button
        if self.mode == Mode::Daemon
            && self.running.load(Ordering::SeqCst)
            && self.daemon_files.lock().unwrap().is_empty()
        {
            self.running.store(false, Ordering::SeqCst);
        }

        // show_slots measures the real content height into self.measured_h
        self.show_slots(ctx);

        // sizing: width once (to the controls), height to the real content
        let cur = ctx.screen_rect().size();
        let want_w = if self.sized_w { cur.x } else { row_w + 14.0 };
        let want_h = self.measured_h + 8.0;
        if row_w > 1.0 && self.measured_h > 1.0 {
            let dw = (want_w - cur.x).abs();
            let dh = (want_h - cur.y).abs();
            if dw > 4.0 || dh > 6.0 {
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                    want_w, want_h,
                )));
            }
            self.sized_w = true;
        }

        if is_running {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }
}

impl App {
    fn show_slots(&mut self, ctx: &egui::Context) {
        let slots = Arc::clone(&self.slots);
        let daemon_mode = self.mode == Mode::Daemon;
        let dtx: Option<Sender<OutMsg>> = if daemon_mode {
            self.daemon_tx.lock().unwrap().clone()
        } else {
            None
        };

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.selected_tab >= SLOTS {
                self.selected_tab = 0;
            }

            // Tab bar (4 fixed slots, fixed width each, tab style).
            egui::ScrollArea::horizontal()
                .id_salt("tabs")
                .show(ui, |ui| {
                    const TAB_W: f32 = 150.0;
                    const TAB_H: f32 = 30.0;
                    let accent = egui::Color32::from_rgb(90, 170, 255);
                    let border = egui::Color32::from_rgb(95, 95, 108);
                    let baseline = border;
                    let panel = ui.visuals().panel_fill;
                    let strong = ui.visuals().strong_text_color();
                    let top_round = egui::CornerRadius { nw: 6, ne: 6, sw: 0, se: 0 };

                    let mut strip_left = f32::MAX;
                    let mut base_y = 0.0;
                    let mut sel_rect: Option<egui::Rect> = None;

                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 3.0;
                        for i in 0..SLOTS {
                            let st = slots[i].state.lock().unwrap().clone();
                            let selected = self.selected_tab == i;
                            let (rect, resp) = ui.allocate_exact_size(
                                egui::vec2(TAB_W, TAB_H),
                                egui::Sense::click(),
                            );
                            if resp.clicked() {
                                self.selected_tab = i;
                            }
                            strip_left = strip_left.min(rect.left());
                            base_y = rect.bottom();
                            if selected {
                                sel_rect = Some(rect);
                            }

                            let bg = if selected {
                                panel
                            } else if resp.hovered() {
                                egui::Color32::from_rgb(48, 48, 55)
                            } else {
                                egui::Color32::from_rgb(34, 34, 40)
                            };
                            let painter = ui.painter();
                            painter.rect_filled(rect, top_round, bg);
                            let (bw, bc) = if selected {
                                (2.0, accent)
                            } else {
                                (1.0, border)
                            };
                            painter.rect_stroke(
                                rect,
                                top_round,
                                egui::Stroke::new(bw, bc),
                                egui::StrokeKind::Inside,
                            );
                            let name_col = if selected {
                                strong
                            } else {
                                egui::Color32::from_rgb(165, 165, 170)
                            };
                            painter.text(
                                egui::pos2(rect.left() + 10.0, rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                format!("encode{}", i + 1),
                                egui::FontId::proportional(13.0),
                                name_col,
                            );
                            painter.text(
                                egui::pos2(rect.right() - 9.0, rect.center().y),
                                egui::Align2::RIGHT_CENTER,
                                st.label(),
                                egui::FontId::proportional(10.5),
                                st.color(),
                            );
                        }
                    });

                    // baseline across the width, "opened" under the active tab
                    let right_edge = ui.max_rect().right();
                    let y = base_y - 0.5;
                    let painter = ui.painter();
                    let stroke = egui::Stroke::new(1.0, baseline);
                    if let Some(sr) = sel_rect {
                        painter.hline(strip_left..=sr.left(), y, stroke);
                        painter.hline(sr.right()..=right_edge, y, stroke);
                        // open the active tab's bottom: erase the bottom border + baseline
                        painter.rect_filled(
                            egui::Rect::from_min_max(
                                egui::pos2(sr.left() + 0.5, sr.bottom() - 2.0),
                                egui::pos2(sr.right() - 0.5, sr.bottom() + 1.5),
                            ),
                            0.0,
                            panel,
                        );
                    } else {
                        painter.hline(strip_left..=right_edge, y, stroke);
                    }
                });

            ui.add_space(8.0);

            let slot = &slots[self.selected_tab];
            let st = slot.state.lock().unwrap().clone();
            let name = slot.name.lock().unwrap().clone();
            let info = slot.info.lock().unwrap().clone();
            let streams = slot.streams.lock().unwrap().clone();
            let target = slot.target.lock().unwrap().clone();
            let progress = slot.progress.lock().unwrap().clone();

            let vid_col = egui::Color32::from_rgb(120, 200, 255);
            let aud_col = egui::Color32::from_rgb(140, 220, 150);
            let val_col = egui::Color32::from_rgb(225, 225, 235);
            let accent = egui::Color32::from_rgb(240, 200, 120);
            let eta_col = egui::Color32::from_rgb(120, 230, 160);

            // --- File + state ---
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("File:").strong());
                if name.is_empty() {
                    ui.colored_label(SlotState::Idle.color(), "idle");
                } else {
                    ui.monospace(&name);
                    if st != SlotState::Idle {
                        ui.colored_label(st.color(), st.label());
                    }
                }
            });
            // Output (always reserves the line so the height doesn't change)
            ui.add_visible_ui(!name.is_empty() && !info.is_empty(), |ui| {
                ui.label(egui::RichText::new(format!("Output: {info}")).weak());
            });

            // --- Command: fixed height, reserved even when empty ---
            let cmd = slot.cmdline.lock().unwrap().clone();
            ui.add_visible_ui(!cmd.is_empty(), |ui| {
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Command:").strong());
                let mut cmd_show = cmd.clone();
                egui::ScrollArea::vertical()
                    .id_salt("cmd")
                    .max_height(54.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut cmd_show)
                                .font(egui::TextStyle::Monospace)
                                .desired_width(f32::INFINITY)
                                .interactive(false)
                                .text_color(egui::Color32::from_rgb(170, 170, 175)),
                        );
                    });
            });

            // Streams + progress: only visible while encoding, but ALWAYS reserves the height
            ui.add_visible_ui(st == SlotState::Running, |ui| {
                ui.add_space(6.0);

                // --- Streams: 4 boxes at 25% — video src/tgt and audio src/tgt ---
                const SH: f32 = 126.0;
                const LW: f32 = 58.0;
                ui.columns(4, |cols| {
                    titled_group(&mut cols[0], "VIDEO · SOURCE", |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.set_min_height(SH);
                        kv_row(ui, LW, "Codec", &streams.v_codec, vid_col);
                        kv_row(ui, LW, "Res", &streams.v_res, vid_col);
                        kv_row(ui, LW, "FPS", &streams.v_fps, vid_col);
                        kv_row(ui, LW, "Bits", &streams.v_bits, vid_col);
                        kv_row(ui, LW, "Bitrate", &streams.v_bitrate, vid_col);
                        kv_row(ui, LW, "Dur", &streams.dur_str, accent);
                    });
                    titled_group(&mut cols[1], "VIDEO · TARGET", |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.set_min_height(SH);
                        kv_row(ui, LW, "Codec", &target.v_codec, vid_col);
                        kv_row(ui, LW, "Res", &target.v_res, vid_col);
                        kv_row(ui, LW, "FPS", &target.v_fps, vid_col);
                        kv_row(ui, LW, "Bits", &target.v_bits, vid_col);
                        kv_row(ui, LW, "Bitrate", &target.v_bitrate, accent);
                        kv_row(ui, LW, "Dur", &target.dur_str, accent);
                    });
                    titled_group(&mut cols[2], "AUDIO · SOURCE", |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.set_min_height(SH);
                        kv_row(ui, LW, "Codec", &streams.a_codec, aud_col);
                        kv_row(ui, LW, "Channels", &streams.a_layout, aud_col);
                        kv_row(ui, LW, "Sample", &streams.a_rate, aud_col);
                        kv_row(ui, LW, "Bitrate", &streams.a_bitrate, aud_col);
                    });
                    titled_group(&mut cols[3], "AUDIO · TARGET", |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.set_min_height(SH);
                        kv_row(ui, LW, "Codec", &target.a_codec, aud_col);
                        kv_row(ui, LW, "Channels", &target.a_layout, aud_col);
                        kv_row(ui, LW, "Sample", &target.a_rate, aud_col);
                        kv_row(ui, LW, "Bitrate", &target.a_bitrate, accent);
                    });
                });

                ui.add_space(6.0);

                // --- Parsed progress + ETA (always 8 rows, so the height is fixed) ---
                let p = progress.clone().unwrap_or_default();
                titled_group(ui, "PROGRESS", |ui| {
                    ui.set_min_width(ui.available_width());
                    let eta = if p.speed_val > 0.0 && streams.duration > 0.0 {
                        let rem = (streams.duration - p.time_sec).max(0.0);
                        encoder::fmt_hms(rem / p.speed_val)
                    } else {
                        "—".to_string()
                    };
                    let lw = 95.0;
                    kv_row(ui, lw, "Frame", &p.frame, val_col);
                    kv_row(ui, lw, "FPS", &p.fps, val_col);
                    kv_row(ui, lw, "Q", &p.q, val_col);
                    kv_row(ui, lw, "Size", &p.size, val_col);
                    let elapsed = if p.time.is_empty() {
                        format!("— / {}", streams.dur_str)
                    } else {
                        format!("{} / {}", p.time, streams.dur_str)
                    };
                    kv_row(ui, lw, "Time", &elapsed, val_col);
                    kv_row(ui, lw, "Bitrate", &p.bitrate, val_col);
                    kv_row(ui, lw, "Speed", &p.speed, accent);
                    kv_row(ui, lw, "ETA", &eta, eta_col);

                    let frac = if streams.duration > 0.0 {
                        (p.time_sec / streams.duration).clamp(0.0, 1.0) as f32
                    } else {
                        0.0
                    };
                    ui.add_space(6.0);
                    ui.add(egui::ProgressBar::new(frac).show_percentage());
                });
            });

            // --- interactive keys (visible while running; always reserves height) ---
            ui.add_visible_ui(st == SlotState::Running, |ui| {
                ui.add_space(6.0);
                let kb_id = egui::Id::new(("keys", self.selected_tab));
                ui.horizontal(|ui| {
                    // focusable area: click and then type (q / + / -)
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(300.0, 26.0), egui::Sense::hover());
                    let resp = ui.interact(rect, kb_id, egui::Sense::click());
                    if resp.clicked() {
                        resp.request_focus();
                    }
                    let focused = resp.has_focus();
                    let (bg, brd, txt) = if focused {
                        (
                            egui::Color32::from_rgb(18, 55, 30),
                            egui::Color32::from_rgb(120, 220, 140),
                            egui::Color32::from_rgb(175, 240, 185),
                        )
                    } else {
                        (
                            egui::Color32::from_rgb(35, 35, 42),
                            egui::Color32::from_rgb(95, 95, 108),
                            egui::Color32::from_rgb(170, 170, 175),
                        )
                    };
                    let painter = ui.painter();
                    painter.rect(
                        rect,
                        4.0,
                        bg,
                        egui::Stroke::new(1.0, brd),
                        egui::StrokeKind::Inside,
                    );
                    let msg = if focused {
                        "● keyboard active — press  q  +  -"
                    } else {
                        "click here and type  (q + -)"
                    };
                    painter.text(
                        egui::pos2(rect.left() + 8.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        msg,
                        egui::FontId::proportional(12.5),
                        txt,
                    );
                    // buttons for the mouse
                    for (k, tip) in [
                        ("q", "close this encoder (finalizes the file)"),
                        ("+", "more verbose"),
                        ("-", "less verbose"),
                    ] {
                        if ui.button(k).on_hover_text(tip).clicked() {
                            send_key(slot, daemon_mode, &dtx, k);
                        }
                    }
                });
                // if the area is focused, forward the typed keys to ffmpeg/daemon
                if ctx.memory(|m| m.has_focus(kb_id)) {
                    let typed = ctx.input(|i| {
                        let mut s = String::new();
                        for e in &i.events {
                            if let egui::Event::Text(t) = e {
                                s.push_str(t);
                            }
                        }
                        s
                    });
                    if !typed.is_empty() {
                        send_key(slot, daemon_mode, &dtx, &typed);
                    }
                }
            });

            // --- on FAIL: show the raw error (last lines) ---
            if matches!(st, SlotState::Fail(_)) {
                let out = slot.output.lock().unwrap().clone();
                if !out.is_empty() {
                    ui.add_space(6.0);
                    ui.colored_label(
                        egui::Color32::from_rgb(230, 110, 110),
                        "Error (ffmpeg output):",
                    );
                    let tail: String = out
                        .chars()
                        .rev()
                        .take(2000)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    egui::Frame::group(ui.style())
                        .fill(egui::Color32::from_rgb(0, 0, 40))
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .id_salt("err")
                                .max_height(180.0)
                                .auto_shrink([false, false])
                                .stick_to_bottom(true)
                                .show(ui, |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(tail)
                                                .monospace()
                                                .color(egui::Color32::from_rgb(235, 180, 180)),
                                        )
                                        .wrap(),
                                    );
                                });
                        });
                }
            }

            // real content height (to auto-size the window)
            self.measured_h = ui.min_rect().bottom();
        });
    }
}

fn main() -> eframe::Result<()> {
    // On a GPU-less VM (e.g. VMware Horizon) there's no hardware OpenGL and glow would die
    // with "egui_glow requires opengl 2.0+". If Mesa's opengl32.dll (+ libgallium_wgl.dll)
    // sits next to the exe, Windows uses it instead of the system one; we force its software
    // driver (llvmpipe) to get CPU OpenGL deterministically. A GALLIUM_DRIVER already coming
    // from the environment is respected (e.g. =d3d12 on a machine with a GPU).
    #[cfg(windows)]
    {
        if std::env::var_os("GALLIUM_DRIVER").is_none() {
            std::env::set_var("GALLIUM_DRIVER", "llvmpipe");
        }
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([780.0, 520.0])
            .with_min_inner_size([700.0, 300.0]),
        ..Default::default()
    };
    let res = eframe::run_native(
        "KciNicK Batch Encoder",
        options,
        Box::new(|_cc| Ok(Box::<App>::default())),
    );
    // The release runs with windows_subsystem="windows" (no console): if run_native fails
    // (e.g. a VM without OpenGL: "egui_glow requires opengl 2.0+"), the error is invisible
    // and it looks like it "won't open". We leave it in fast6_error.log for diagnosis from a
    // double-click. (VM fix: Mesa/llvmpipe opengl32.dll next to the exe → SW GL.)
    if let Err(e) = &res {
        log_fatal(&format!("{e}"));
    }
    res
}

/// Writes the reason for a fatal exit to a log (next to the exe and, as a fallback, in %TEMP%),
/// because the release GUI has no console to show it.
fn log_fatal(msg: &str) {
    let line = format!("fast6 fatal: {msg}\r\n");
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let _ = std::fs::write(dir.join("fast6_error.log"), &line);
        }
    }
    if let Ok(tmp) = std::env::var("TEMP") {
        let _ = std::fs::write(std::path::Path::new(&tmp).join("fast6_error.log"), &line);
    }
}
