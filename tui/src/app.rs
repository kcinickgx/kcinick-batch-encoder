//! Central app state (mirror of the egui GUI's `App`) + the TUI's UI state.
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::ChildStdin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::client::{self, OutMsg};
use crate::config;
use crate::controller::{self, RunCfg};
use crate::encoder::{self, CropMode};
use crate::proto::{C2D, D2C};
use crate::theme;

pub const PRESETS: [&str; 7] = ["p1", "p2", "p3", "p4", "p5", "p6", "p7"];
pub const SLOTS: usize = 4;
// (high_kbps, low_kbps, label): high = 4K (>=3000px), low = the rest
pub const BITRATES: [(i64, i64, &str); 4] = [
    (10000, 8000, "10k/8k"),
    (8000, 6000, "8k/6k"),
    (6000, 4000, "6k/4k"),
    (4000, 2000, "4k/2k"),
];
pub const DEFAULT_FFMPEG: &str = "/usr/bin/ffmpeg";
pub const CODECS: [encoder::Codec; 3] = [
    encoder::Codec::H264,
    encoder::Codec::H265,
    encoder::Codec::Av1,
];

/// Working mode: local ffmpeg or remote daemon.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Local,
    Daemon,
}

#[derive(Clone, PartialEq)]
pub enum SlotState {
    Idle,
    Running,
    Fail(i32),
}

impl SlotState {
    pub fn label(&self) -> String {
        match self {
            SlotState::Idle => "idle".into(),
            SlotState::Running => "running".into(),
            SlotState::Fail(c) => format!("FAIL {c}"),
        }
    }
    pub fn color(&self) -> Color {
        match self {
            SlotState::Idle => theme::DIM,
            SlotState::Running => theme::ACCENT,
            SlotState::Fail(_) => theme::FAIL,
        }
    }
}

/// An encoder slot (encode1..4). State shared between UI and controller.
pub struct Slot {
    pub state: Arc<Mutex<SlotState>>,
    pub name: Arc<Mutex<String>>,
    pub info: Arc<Mutex<String>>,
    pub cmdline: Arc<Mutex<String>>,
    pub output: Arc<Mutex<String>>,
    pub streams: Arc<Mutex<encoder::StreamSummary>>,
    pub target: Arc<Mutex<encoder::StreamSummary>>,
    pub progress: Arc<Mutex<Option<Progress>>>,
    pub stdin: Arc<Mutex<Option<ChildStdin>>>,
    pub pid: Arc<Mutex<Option<u32>>>,
    pub job_id: Arc<Mutex<Option<String>>>,
}

impl Slot {
    pub fn new() -> Self {
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

    pub fn reset(&self) {
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

    pub fn send_bytes(&self, b: &[u8]) {
        use std::io::Write;
        if let Some(stdin) = self.stdin.lock().unwrap().as_mut() {
            let _ = stdin.write_all(b);
            let _ = stdin.flush();
        }
    }

    pub fn send_quit(&self) {
        self.send_bytes(b"q");
    }
}

/// A parsed ffmpeg progress line.
#[derive(Clone, Default)]
pub struct Progress {
    pub frame: String,
    pub fps: String,
    pub q: String,
    pub size: String,
    pub time: String,
    pub bitrate: String,
    pub speed: String,
    pub time_sec: f64,
    pub speed_val: f64,
}

// ===================== UI state (mouse/hover/focus) =====================

/// Identifies each interactive element for mouse hit-testing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ElemId {
    TitleClose,
    ModeLocal,
    ModeRemote,
    FieldFfmpeg,
    BrowseFfmpeg,
    FieldServer,
    FieldToken,
    ConnectBtn,
    FieldSrc,
    BrowseSrc,
    Play,
    Stop,
    Crop(u8),
    Codec(usize),
    Encoders(usize),
    Preset(usize),
    Bitrate(usize),
    Tab(usize),
    KeyZone,
    KeyBtn(u8),
    ExRow(usize),
    ExCheck(usize),
    ExUp,
    ExOpen,
    ExCancel,
    ConfirmYes,
    ConfirmNo,
}

/// Which text field (or zone) has keyboard focus.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    None,
    Ffmpeg,
    Server,
    Token,
    Src,
    KeyZone,
}

pub enum ExKind {
    File, // pick ffmpeg
    Dir,  // pick source folder
}

/// State of the modal file explorer ("…").
pub struct Explorer {
    pub kind: ExKind,
    pub cwd: PathBuf,
    pub entries: Vec<(String, bool)>, // (name, is_dir)
    pub selected: usize,
    pub scroll: usize,
}

impl Explorer {
    pub fn open(kind: ExKind, start: &str) -> Self {
        let mut cwd = PathBuf::from(start);
        if !cwd.is_dir() {
            cwd = cwd.parent().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        }
        if !cwd.is_dir() {
            cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        }
        cwd = cwd.canonicalize().unwrap_or(cwd); // canonical paths = match the scan
        let mut ex = Explorer {
            kind,
            cwd,
            entries: Vec::new(),
            selected: 0,
            scroll: 0,
        };
        ex.refresh();
        ex
    }

    pub fn refresh(&mut self) {
        let mut dirs: Vec<(String, bool)> = Vec::new();
        let mut files: Vec<(String, bool)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.cwd) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                let is_dir = e.path().is_dir();
                if is_dir {
                    dirs.push((name, true));
                } else {
                    // File: any file; Dir: only videos (to check/uncheck)
                    let keep = match self.kind {
                        ExKind::File => true,
                        ExKind::Dir => std::path::Path::new(&name)
                            .extension()
                            .and_then(|e| e.to_str())
                            .map(|e| encoder::EXTENSIONS.contains(&e.to_lowercase().as_str()))
                            .unwrap_or(false),
                    };
                    if keep {
                        files.push((name, false));
                    }
                }
            }
        }
        dirs.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
        files.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
        self.entries = dirs;
        self.entries.extend(files);
        self.selected = 0;
        self.scroll = 0;
    }

    pub fn enter(&mut self, idx: usize) {
        if let Some((name, is_dir)) = self.entries.get(idx).cloned() {
            if is_dir {
                self.cwd.push(name);
                self.cwd = self.cwd.canonicalize().unwrap_or_else(|_| self.cwd.clone());
                self.refresh();
            }
        }
    }

    pub fn go_up(&mut self) {
        if let Some(p) = self.cwd.parent() {
            self.cwd = p.to_path_buf();
            self.refresh();
        }
    }
}

pub struct App {
    pub mode: Mode,
    pub ffmpeg: String,
    pub daemon_addr: String,
    pub token: String,
    pub src_dir: String,
    pub crop: CropMode,
    pub parallel: usize,
    pub preset_idx: usize,
    pub bitrate_idx: usize,
    pub codec_idx: usize,

    // daemon client
    pub daemon_tx: Arc<Mutex<Option<Sender<OutMsg>>>>,
    pub daemon_conn: Arc<Mutex<Option<TcpStream>>>,
    pub daemon_files: Arc<Mutex<HashMap<String, PathBuf>>>,
    pub slot_of: Arc<Mutex<HashMap<String, usize>>>,
    pub job_counter: usize,

    pub files: Vec<PathBuf>,
    pub excluded: HashSet<PathBuf>,      // unchecked videos (canonical)
    pub excluded_dirs: HashSet<PathBuf>, // unchecked folders (recursive, canonical)
    pub slots: Arc<Vec<Slot>>,
    pub queue: Arc<Mutex<VecDeque<PathBuf>>>,
    pub target: Arc<AtomicUsize>,
    pub running: Arc<AtomicBool>,
    pub cancel: Arc<AtomicBool>,

    pub selected_tab: usize,
    pub status_msg: String,
    pub needs_scan: bool,

    // UI
    pub hotspots: Vec<(Rect, ElemId)>,
    pub hover: Option<ElemId>,
    pub focus: Focus,
    pub edit_cursor: usize,
    pub cmd_scroll: u16,
    pub explorer: Option<Explorer>,
    pub confirm_exit: bool,
    pub closing: bool,
    pub should_quit: bool,
}

impl Default for App {
    fn default() -> Self {
        let src_dir = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".into());
        App {
            mode: match config::load("mode").as_deref() {
                Some("daemon") => Mode::Daemon,
                _ => Mode::Local,
            },
            ffmpeg: config::load("ffmpeg").unwrap_or_else(|| DEFAULT_FFMPEG.into()),
            daemon_addr: config::load("daemon_addr").unwrap_or_else(|| "127.0.0.1:7878".into()),
            token: config::load("token").unwrap_or_else(|| "changeme".into()),
            src_dir,
            crop: CropMode::None,
            parallel: 1,
            preset_idx: 6,
            bitrate_idx: 2,
            codec_idx: config::load("codec")
                .and_then(|s| CODECS.iter().position(|c| c.label() == s))
                .unwrap_or(2),
            daemon_tx: Arc::new(Mutex::new(None)),
            daemon_conn: Arc::new(Mutex::new(None)),
            daemon_files: Arc::new(Mutex::new(HashMap::new())),
            slot_of: Arc::new(Mutex::new(HashMap::new())),
            job_counter: 0,
            files: Vec::new(),
            excluded: HashSet::new(),
            excluded_dirs: HashSet::new(),
            slots: Arc::new((0..SLOTS).map(|_| Slot::new()).collect()),
            queue: Arc::new(Mutex::new(VecDeque::new())),
            target: Arc::new(AtomicUsize::new(1)),
            running: Arc::new(AtomicBool::new(false)),
            cancel: Arc::new(AtomicBool::new(false)),
            selected_tab: 0,
            status_msg: String::new(),
            needs_scan: true,
            hotspots: Vec::new(),
            hover: None,
            focus: Focus::None,
            edit_cursor: 0,
            cmd_scroll: 0,
            explorer: None,
            confirm_exit: false,
            closing: false,
            should_quit: false,
        }
    }
}

impl App {
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn connected(&self) -> bool {
        self.daemon_tx.lock().unwrap().is_some()
    }

    pub fn scan(&mut self) {
        let dir = PathBuf::from(&self.src_dir);
        self.files = encoder::find_videos(&dir)
            .into_iter()
            .filter(|p| {
                let c = p.canonicalize().unwrap_or_else(|_| p.clone());
                !self.file_excluded(&c)
            })
            .collect();
    }

    /// Is the video (canonical path) unchecked, directly or via an ancestor folder?
    pub fn file_excluded(&self, canon: &std::path::Path) -> bool {
        self.excluded.contains(canon)
            || canon.ancestors().skip(1).any(|a| self.excluded_dirs.contains(a))
    }

    /// Is the folder (canonical path) unchecked, itself or an ancestor?
    pub fn dir_excluded(&self, canon: &std::path::Path) -> bool {
        canon.ancestors().any(|a| self.excluded_dirs.contains(a))
    }

    pub fn running_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| matches!(*s.state.lock().unwrap(), SlotState::Running))
            .count()
    }

    /// Clean shutdown: cancels and sends 'q' to my own running ffmpegs.
    pub fn send_quit_all(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        for s in self.slots.iter() {
            if matches!(*s.state.lock().unwrap(), SlotState::Running) {
                s.send_quit();
            }
        }
    }

    /// Returns the string of the currently focused field (for editing).
    pub fn focused_field_mut(&mut self) -> Option<&mut String> {
        match self.focus {
            Focus::Ffmpeg => Some(&mut self.ffmpeg),
            Focus::Server => Some(&mut self.daemon_addr),
            Focus::Token => Some(&mut self.token),
            Focus::Src => Some(&mut self.src_dir),
            _ => None,
        }
    }

    /// On losing focus of a field, persist it (like `lost_focus` in the GUI).
    pub fn blur(&mut self) {
        match self.focus {
            Focus::Ffmpeg => config::save("ffmpeg", &self.ffmpeg),
            Focus::Server => config::save("daemon_addr", &self.daemon_addr),
            Focus::Token => config::save("token", &self.token),
            // Focus::Src does NOT rescan: the QUEUE count updates on Play
            _ => {}
        }
        self.focus = Focus::None;
    }

    pub fn focus_field(&mut self, f: Focus) {
        if self.focus != f {
            self.blur();
        }
        self.focus = f;
        let len = match f {
            Focus::Ffmpeg => self.ffmpeg.chars().count(),
            Focus::Server => self.daemon_addr.chars().count(),
            Focus::Token => self.token.chars().count(),
            Focus::Src => self.src_dir.chars().count(),
            _ => 0,
        };
        self.edit_cursor = len;
    }

    pub fn start(&mut self) {
        if self.is_running() {
            return;
        }
        // on Play always rescan the current folder (so QUEUE updates here)
        self.scan();
        if self.files.is_empty() {
            self.status_msg = "No files to encode.".into();
            return;
        }

        if self.mode == Mode::Daemon {
            self.start_daemon();
            return;
        }

        let clean = self.ffmpeg.trim().trim_matches('"').to_string();
        if clean != self.ffmpeg {
            self.ffmpeg = clean;
        }
        let ffprobe = encoder::ffprobe_from_ffmpeg(&self.ffmpeg);
        if !std::path::Path::new(&self.ffmpeg).exists() {
            self.status_msg = format!("ffmpeg not found at: {}", self.ffmpeg);
            return;
        }
        if !ffprobe.exists() {
            self.status_msg = format!("ffprobe missing next to ffmpeg: {}", ffprobe.display());
            return;
        }

        self.status_msg.clear();
        config::save("ffmpeg", &self.ffmpeg);
        config::save("codec", CODECS[self.codec_idx].label());

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

        controller::spawn_controller(
            Arc::clone(&self.slots),
            Arc::clone(&self.queue),
            Arc::clone(&self.target),
            Arc::clone(&self.running),
            Arc::clone(&self.cancel),
            cfg,
        );
    }

    /// Connects to the daemon (Hello + auth) and starts the writer/reader. true if connected.
    pub fn connect_daemon(&mut self) -> bool {
        if self.daemon_tx.lock().unwrap().is_some() {
            return true;
        }
        self.status_msg.clear();
        let addr = self.daemon_addr.trim().to_string();
        let mut rd = match TcpStream::connect(&addr) {
            Ok(s) => s,
            Err(e) => {
                self.status_msg = format!("Couldn't connect to {addr}: {e}");
                return false;
            }
        };
        let mut wr = match rd.try_clone() {
            Ok(w) => w,
            Err(e) => {
                self.status_msg = format!("clone socket: {e}");
                return false;
            }
        };

        let hello = C2D::Hello {
            token: self.token.clone(),
            name: "fast6-tui".into(),
        };
        if crate::proto::write_json(&mut wr, &hello).is_err() {
            self.status_msg = "failed sending Hello".into();
            return false;
        }
        let (_, payload) = match crate::proto::read_frame(&mut rd) {
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

        let (tx, rx) = std::sync::mpsc::channel::<OutMsg>();
        std::thread::spawn(move || {
            for m in rx {
                let r = match m {
                    OutMsg::Ctrl(c) => crate::proto::write_json(&mut wr, &c),
                    OutMsg::Block(req, data) => crate::proto::write_block(&mut wr, req, &data),
                };
                if r.is_err() {
                    break;
                }
            }
        });
        *self.daemon_tx.lock().unwrap() = Some(tx.clone());
        *self.daemon_conn.lock().unwrap() = rd.try_clone().ok();

        let slots = Arc::clone(&self.slots);
        let files = Arc::clone(&self.daemon_files);
        let slot_of = Arc::clone(&self.slot_of);
        let running = Arc::clone(&self.running);
        let dtx = Arc::clone(&self.daemon_tx);
        let dconn = Arc::clone(&self.daemon_conn);
        std::thread::spawn(move || {
            client::daemon_reader(rd, &slots, &files, &slot_of, &tx);
            *dtx.lock().unwrap() = None;
            *dconn.lock().unwrap() = None;
            running.store(false, Ordering::SeqCst);
        });
        true
    }

    pub fn disconnect_daemon(&mut self) {
        if let Some(sock) = self.daemon_conn.lock().unwrap().take() {
            let _ = sock.shutdown(std::net::Shutdown::Both);
        }
        *self.daemon_tx.lock().unwrap() = None;
        self.running.store(false, Ordering::SeqCst);
        for s in self.slots.iter() {
            s.reset();
        }
        self.slot_of.lock().unwrap().clear();
        self.daemon_files.lock().unwrap().clear();
    }

    pub fn start_daemon(&mut self) {
        if !self.connect_daemon() {
            return;
        }
        config::save("mode", "daemon");
        config::save("daemon_addr", &self.daemon_addr);
        config::save("token", &self.token);
        config::save("codec", CODECS[self.codec_idx].label());

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
                local_path: None,
            }));
        }
        self.running.store(true, Ordering::SeqCst);
    }

    pub fn daemon_cancel_all(&self) {
        if let Some(tx) = self.daemon_tx.lock().unwrap().clone() {
            let ids: Vec<String> = self.daemon_files.lock().unwrap().keys().cloned().collect();
            for id in ids {
                let _ = tx.send(OutMsg::Ctrl(C2D::Cancel { job_id: id }));
            }
        }
        self.running.store(false, Ordering::SeqCst);
    }

    /// QUEUE box counter:
    /// - idle: total files to encode (refreshed on scan: startup and Play).
    /// - processing: the ones left (goes down as they finish).
    pub fn queue_len(&self) -> usize {
        if self.is_running() {
            if self.mode == Mode::Daemon {
                self.daemon_files.lock().unwrap().len()
            } else {
                self.queue.lock().unwrap().len() + self.running_count()
            }
        } else {
            self.files.len()
        }
    }

    /// Adjusts parallelism live (and tells the daemon if applicable).
    pub fn set_parallel(&mut self, n: usize) {
        self.parallel = n;
        self.target.store(n, Ordering::SeqCst);
        if self.mode == Mode::Daemon {
            if let Some(tx) = self.daemon_tx.lock().unwrap().clone() {
                let _ = tx.send(OutMsg::Ctrl(C2D::SetParallel { n }));
            }
        }
    }

    pub fn daemon_key(&self, slot_idx: usize, key: &str) {
        let daemon_mode = self.mode == Mode::Daemon;
        let dtx = if daemon_mode {
            self.daemon_tx.lock().unwrap().clone()
        } else {
            None
        };
        client::send_key(&self.slots[slot_idx], daemon_mode, &dtx, key);
    }
}
