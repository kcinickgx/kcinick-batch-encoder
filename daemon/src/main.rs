// fast6d — encoding daemon.
// The client (CLI/GUI) does NOT send the whole file: the daemon reads it on demand.
// ffmpeg reads the input from a local HTTP shim (127.0.0.1) that serves it by ranges;
// each range is resolved by reading a local file (owner/test mode) or by requesting the
// block from the client that submitted it (remote mode), all over the same TCP connection.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[path = "../../src/encoder.rs"]
#[allow(dead_code)] // encoder.rs shares logic with the GUI; the daemon doesn't use all of it
mod encoder;
use encoder::{Codec, CropMode, VideoEncoder};

// (high_kbps, low_kbps) by index — same as the GUI
const BITRATES: [(i64, i64); 4] = [(10000, 8000), (8000, 6000), (6000, 4000), (4000, 2000)];

// protocol shared with the GUI/CLI
#[path = "../../src/proto.rs"]
#[allow(dead_code)] // proto has helpers the client uses (write_block), not the daemon
mod proto;
use proto::{read_frame, write_json, write_result, SummaryDto, C2D, D2C, TAG_BLOCK, TAG_CTRL};

impl From<&encoder::StreamSummary> for SummaryDto {
    fn from(s: &encoder::StreamSummary) -> Self {
        SummaryDto {
            dur: s.dur_str.clone(),
            dur_sec: s.duration,
            v_codec: s.v_codec.clone(),
            v_res: s.v_res.clone(),
            v_fps: s.v_fps.clone(),
            v_bits: s.v_bits.clone(),
            v_fmt: s.v_fmt.clone(),
            v_bitrate: s.v_bitrate.clone(),
            a_codec: s.a_codec.clone(),
            a_layout: s.a_layout.clone(),
            a_rate: s.a_rate.clone(),
            a_bitrate: s.a_bitrate.clone(),
        }
    }
}

// ===================== state =====================
enum Source {
    Local(Mutex<File>), // local file on the daemon (owner / --test)
    Remote(usize),      // id of the client that serves the blocks
}

struct Job {
    id: String,
    client: usize, // 0 = no client (test mode)
    filename: String,
    size: u64,
    source: Source,
    crop: CropMode,
    codec: Codec,
    preset: String,
    bitrate_idx: usize,
    state: Mutex<String>,
    cancel: AtomicBool,
    stdin: Mutex<Option<ChildStdin>>,
}

/// Result file stream (LOW priority, FIFO channel). It's kept separate from control so
/// the `ReadBlock`s (which feed ffmpeg's INPUT) don't get queued behind megabytes of the
/// result → otherwise ffmpeg starves for input and drops to 1x.
enum ChunkMsg {
    Begin(String),
    Data(String, Vec<u8>),
    End(String, bool),
}

struct ClientHandle {
    ctrl: Sender<D2C>,        // HIGH priority: ReadBlock, State, Progress, Streams…
    chunk: Sender<ChunkMsg>,  // LOW priority: bytes of the resulting .mp4
    pending: Arc<Mutex<HashMap<u64, Sender<Vec<u8>>>>>,
    next_req: AtomicU64,
}

struct Server {
    token: String,
    ffmpeg: String,
    ffprobe: PathBuf,
    out_dir: PathBuf,
    shim_port: u16,
    clients: Mutex<HashMap<usize, Arc<ClientHandle>>>,
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    queue: Mutex<VecDeque<String>>,
    running: Mutex<HashMap<String, ()>>,
    target: AtomicUsize,
    enc_cache: Mutex<HashMap<String, VideoEncoder>>,
    next_client: AtomicUsize,
    listening: AtomicBool, // false => the accept loops and the controller exit
    out_names: Mutex<HashSet<String>>, // output names already reserved (avoids clobbering)
}

fn crop_from(c: u8) -> CropMode {
    match c {
        1 => CropMode::Centered,
        2 => CropMode::OneThird,
        _ => CropMode::None,
    }
}

fn codec_from(s: &str) -> Codec {
    match s.to_ascii_uppercase().as_str() {
        "H264" => Codec::H264,
        "H265" | "HEVC" => Codec::H265,
        _ => Codec::Av1,
    }
}

/// Sanitized output name (basename only, .mp4 extension) — avoids path traversal.
fn out_name(filename: &str) -> String {
    let base = Path::new(filename)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    let stem = Path::new(base)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    format!("{stem}.mp4")
}

/// Reserves a UNIQUE output path. If the name is already taken by another job
/// (e.g. the same movie in different subfolders) it appends " (2)", " (3)"… so the
/// outputs don't clobber each other.
fn reserve_out(srv: &Server, filename: &str) -> PathBuf {
    let base = out_name(filename);
    let stem = Path::new(&base)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output")
        .to_string();
    let mut used = srv.out_names.lock().unwrap();
    let mut candidate = base;
    let mut n = 2;
    while used.contains(&candidate) {
        candidate = format!("{stem} ({n}).mp4");
        n += 1;
    }
    used.insert(candidate.clone());
    srv.out_dir.join(candidate)
}

/// Shortened command for display: "ffmpeg … {source} … {target}" (the input URL and the
/// output path are replaced so the client shows a clean line, like it does in Local mode).
fn display_cmd(args: &[String]) -> String {
    let mut disp = String::from("ffmpeg");
    let n = args.len();
    for (idx, a) in args.iter().enumerate() {
        let token = if idx > 0 && args[idx - 1] == "-i" {
            "{source}"
        } else if idx == n - 1 {
            "{target}"
        } else {
            a.as_str()
        };
        disp.push(' ');
        disp.push_str(token);
    }
    disp
}

// ===================== block reads =====================
fn read_block(srv: &Server, job: &Job, offset: u64, len: u32) -> io::Result<Vec<u8>> {
    match &job.source {
        Source::Local(f) => {
            let mut f = f.lock().unwrap();
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
        Source::Remote(cid) => {
            let client = srv.clients.lock().unwrap().get(cid).cloned();
            let Some(client) = client else {
                return Err(io::Error::new(io::ErrorKind::NotConnected, "client disconnected"));
            };
            let req_id = client.next_req.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = mpsc::channel::<Vec<u8>>();
            client.pending.lock().unwrap().insert(req_id, tx);
            let _ = client.ctrl.send(D2C::ReadBlock {
                req_id,
                job_id: job.id.clone(),
                offset,
                len,
            });
            match rx.recv_timeout(Duration::from_secs(30)) {
                Ok(data) => Ok(data),
                Err(_) => {
                    client.pending.lock().unwrap().remove(&req_id);
                    Err(io::Error::new(io::ErrorKind::TimedOut, "timeout waiting for block"))
                }
            }
        }
    }
}

/// Requests a block from the remote client WITHOUT waiting for the reply: registers the
/// pending entry and sends the ReadBlock. Returns (req_id, receiver) to await later. Lets us
/// keep several requests IN FLIGHT (pipelining) instead of a full round-trip per block.
fn issue_remote_block(
    client: &ClientHandle,
    job_id: &str,
    offset: u64,
    len: u32,
) -> (u64, mpsc::Receiver<Vec<u8>>) {
    let req_id = client.next_req.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    client.pending.lock().unwrap().insert(req_id, tx);
    let _ = client.ctrl.send(D2C::ReadBlock {
        req_id,
        job_id: job_id.to_string(),
        offset,
        len,
    });
    (req_id, rx)
}

// ===================== HTTP shim =====================
fn handle_shim(srv: Arc<Server>, mut sock: TcpStream) {
    sock.set_read_timeout(Some(Duration::from_secs(30))).ok();

    // read headers up to \r\n\r\n
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        match sock.read(&mut tmp) {
            Ok(0) => return,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if buf.len() > 16 * 1024 {
                    return;
                }
            }
            Err(_) => return,
        }
    }

    let head = String::from_utf8_lossy(&buf);
    let mut lines = head.split("\r\n");
    let req_line = lines.next().unwrap_or("");
    let mut parts = req_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/");
    let job_id = path.trim_start_matches('/').to_string();

    let mut has_range = false;
    let mut range_start: u64 = 0;
    let mut range_end: Option<u64> = None;
    for l in lines {
        let low = l.to_ascii_lowercase();
        if let Some(v) = low.strip_prefix("range:") {
            if let Some(r) = v.trim().strip_prefix("bytes=") {
                has_range = true;
                let mut it = r.split('-');
                range_start = it.next().unwrap_or("0").trim().parse().unwrap_or(0);
                if let Some(e) = it.next() {
                    let e = e.trim();
                    if !e.is_empty() {
                        range_end = e.parse().ok();
                    }
                }
            }
        }
    }

    let job = srv.jobs.lock().unwrap().get(&job_id).cloned();
    let Some(job) = job else {
        let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    };
    let size = job.size;

    let end = range_end
        .unwrap_or(size.saturating_sub(1))
        .min(size.saturating_sub(1));
    let start = range_start;
    if size == 0 || start > end {
        let _ = sock.write_all(
            format!(
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{size}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        );
        return;
    }

    let body_len = end - start + 1;
    let status = if has_range { "206 Partial Content" } else { "200 OK" };
    let mut hdr = format!(
        "HTTP/1.1 {status}\r\nAccept-Ranges: bytes\r\nContent-Type: application/octet-stream\r\nContent-Length: {body_len}\r\nConnection: close\r\n"
    );
    if has_range {
        hdr.push_str(&format!("Content-Range: bytes {start}-{end}/{size}\r\n"));
    }
    hdr.push_str("\r\n");
    if sock.write_all(hdr.as_bytes()).is_err() {
        return;
    }
    if method.eq_ignore_ascii_case("HEAD") {
        return;
    }

    // body: on-demand streaming by blocks
    let chunk = 256 * 1024u64;
    match &job.source {
        // Local: direct sequential seek+read. No network → no pipeline needed.
        Source::Local(_) => {
            let mut off = start;
            while off <= end {
                let want = chunk.min(end - off + 1) as u32;
                match read_block(&srv, &job, off, want) {
                    Ok(data) if !data.is_empty() => {
                        if sock.write_all(&data).is_err() {
                            return;
                        }
                        off += data.len() as u64;
                    }
                    _ => return,
                }
            }
        }
        // Remote: a sliding window of READAHEAD blocks IN FLIGHT. We request ahead and
        // write IN ORDER as they arrive, so the input doesn't wait for a round-trip per
        // block → it fills the pipe even with high RTT (a remote friend over Tailscale). On
        // a LAN TCP_NODELAY is already enough; this makes it robust against internet latency too.
        Source::Remote(cid) => {
            const READAHEAD: usize = 16; // 16 × 256KB = 4MB max in flight per job
            let Some(client) = srv.clients.lock().unwrap().get(cid).cloned() else {
                return;
            };
            let mut inflight: VecDeque<(u64, mpsc::Receiver<Vec<u8>>)> = VecDeque::new();
            let mut next_off = start;
            // prime the window
            while next_off <= end && inflight.len() < READAHEAD {
                let want = chunk.min(end - next_off + 1) as u32;
                inflight.push_back(issue_remote_block(&client, &job.id, next_off, want));
                next_off += chunk;
            }
            // drain in order; for each block written, request the next (keep the window full)
            while let Some((req_id, rx)) = inflight.pop_front() {
                let data = match rx.recv_timeout(Duration::from_secs(30)) {
                    Ok(d) => d,
                    Err(_) => {
                        // clean up our own pending + those still in flight
                        let mut pend = client.pending.lock().unwrap();
                        pend.remove(&req_id);
                        for (rid, _) in &inflight {
                            pend.remove(rid);
                        }
                        return;
                    }
                };
                if data.is_empty() || sock.write_all(&data).is_err() {
                    let mut pend = client.pending.lock().unwrap();
                    for (rid, _) in &inflight {
                        pend.remove(rid);
                    }
                    return;
                }
                if next_off <= end {
                    let want = chunk.min(end - next_off + 1) as u32;
                    inflight.push_back(issue_remote_block(&client, &job.id, next_off, want));
                    next_off += chunk;
                }
            }
        }
    }
    let _ = sock.flush();
}

// ===================== progress parsing =====================
struct Prog {
    frame: String,
    fps: String,
    q: String,
    size: String,
    time: String,
    bitrate: String,
    speed: String,
    time_sec: f64,
    eta: String,
    percent: f64,
}

fn parse_progress(line: &str, dur: f64) -> Option<Prog> {
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
    let eta = if speed_val > 0.0 && dur > 0.0 {
        encoder::fmt_hms((dur - time_sec).max(0.0) / speed_val)
    } else {
        "—".into()
    };
    let percent = if dur > 0.0 {
        (time_sec / dur * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };

    Some(Prog {
        frame: get("frame"),
        fps: get("fps"),
        q: get("q"),
        size: fmt_size(&get("size")),
        time: encoder::fmt_hms(time_sec),
        bitrate: fmt_bitrate(&get("bitrate")),
        speed: if speed.is_empty() { "—".into() } else { speed },
        time_sec,
        eta,
        percent,
    })
}

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

fn fmt_size(raw: &str) -> String {
    let digits: String = raw.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
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

fn fmt_bitrate(raw: &str) -> String {
    let digits: String = raw.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let kbps: f64 = digits.parse().unwrap_or(0.0);
    if kbps <= 0.0 {
        return "—".into();
    }
    format!("{kbps:.0} kbps")
}

fn pump_progress(mut reader: impl Read, job_id: String, dur: f64, tx: Option<Sender<D2C>>) {
    let mut buf = [0u8; 4096];
    let mut cur = String::new();
    let mut in_esc = false;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        for &b in &buf[..n] {
            if in_esc {
                // discard the ANSI sequence up to a final letter
                if (0x40..=0x7e).contains(&b) {
                    in_esc = false;
                }
                continue;
            }
            match b {
                0x1b => in_esc = true,
                b'\r' | b'\n' => {
                    if let Some(p) = parse_progress(&cur, dur) {
                        match &tx {
                            Some(t) => {
                                let _ = t.send(D2C::Progress {
                                    job_id: job_id.clone(),
                                    frame: p.frame,
                                    fps: p.fps,
                                    q: p.q,
                                    size: p.size,
                                    time: p.time,
                                    bitrate: p.bitrate,
                                    speed: p.speed,
                                    time_sec: p.time_sec,
                                    eta: p.eta,
                                    percent: p.percent,
                                });
                            }
                            None => {
                                // test mode: print to stderr
                                eprintln!(
                                    "[{job_id}] {:>3.0}%  t={} / eta={}  fps={} speed={} size={} br={}",
                                    p.percent, p.time, p.eta, p.fps, p.speed, p.size, p.bitrate
                                );
                            }
                        }
                    }
                    cur.clear();
                }
                _ => cur.push(b as char),
            }
        }
    }
}

// ===================== worker =====================
fn set_state(srv: &Server, job: &Job, st: &str, detail: &str) {
    *job.state.lock().unwrap() = st.to_string();
    eprintln!("[{}] state={st} {detail}", job.id);
    if job.client != 0 {
        if let Some(c) = srv.clients.lock().unwrap().get(&job.client) {
            let _ = c.ctrl.send(D2C::State {
                job_id: job.id.clone(),
                state: st.to_string(),
                detail: detail.to_string(),
            });
        }
    }
}

fn run_job(srv: &Arc<Server>, job_id: &str) {
    let job = match srv.jobs.lock().unwrap().get(job_id).cloned() {
        Some(j) => j,
        None => return,
    };
    let (ctx, chunk_tx): (Option<Sender<D2C>>, Option<Sender<ChunkMsg>>) = if job.client != 0 {
        let clients = srv.clients.lock().unwrap();
        match clients.get(&job.client) {
            Some(c) => (Some(c.ctrl.clone()), Some(c.chunk.clone())),
            None => (None, None),
        }
    } else {
        (None, None)
    };

    // if it was cancelled while waiting in the queue, don't run it
    if job.cancel.load(Ordering::SeqCst) {
        set_state(srv, &job, "cancelled", "");
        return;
    }

    set_state(srv, &job, "probing", "");

    // detect encoder (cached per codec)
    let enc = {
        let label = job.codec.label().to_string();
        let mut cache = srv.enc_cache.lock().unwrap();
        *cache
            .entry(label)
            .or_insert_with(|| encoder::detect_encoder(&srv.ffmpeg, job.codec))
    };

    let shim_url = format!("http://127.0.0.1:{}/{}", srv.shim_port, job_id);

    let probe = match encoder::probe_all(&srv.ffprobe, &shim_url) {
        Some(p) => p,
        None => {
            set_state(srv, &job, "failed", "probe failed (ffprobe couldn't read the input)");
            return;
        }
    };

    let (high, low) = BITRATES[job.bitrate_idx.min(BITRATES.len() - 1)];
    let _ = std::fs::create_dir_all(&srv.out_dir);
    let out = reserve_out(srv, &job.filename);
    let (mut args, info, target) = encoder::build_args(
        &shim_url,
        &out.to_string_lossy(),
        &probe,
        job.crop,
        &job.preset,
        high,
        low,
        enc,
    );

    // For the remote client we're going to send the .mp4 as it grows → it has to be
    // fragmented (empty moov up front, fragments that are APPENDED, no seeks backwards).
    let remote = matches!(job.source, Source::Remote(_)) && chunk_tx.is_some();
    if remote {
        let out_idx = args.len().saturating_sub(1); // insert before the output path
        args.insert(out_idx, "-movflags".into());
        args.insert(out_idx + 1, "+frag_keyframe+empty_moov+default_base_moof".into());
    }

    if let Some(t) = &ctx {
        let _ = t.send(D2C::Streams {
            job_id: job_id.into(),
            source: (&probe.streams).into(),
            target: (&target).into(),
            cmd: display_cmd(&args),
        });
    }

    set_state(srv, &job, "encoding", &info);

    let mut cmd = Command::new(&srv.ffmpeg);
    cmd.args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            set_state(srv, &job, "failed", &format!("couldn't launch ffmpeg: {e}"));
            return;
        }
    };
    *job.stdin.lock().unwrap() = child.stdin.take();

    let dur = probe.streams.duration;
    if let Some(stderr) = child.stderr.take() {
        let jid = job_id.to_string();
        let tx = ctx.clone();
        std::thread::spawn(move || pump_progress(stderr, jid, dur, tx));
    }

    // thread that reads the .mp4 as it grows and sends the new bytes to the client
    let done = Arc::new(AtomicBool::new(false));
    let tail = if remote {
        chunk_tx.clone().map(|t| {
            let out2 = out.clone();
            let jid = job_id.to_string();
            let done2 = done.clone();
            std::thread::spawn(move || stream_result(out2, jid, t, done2))
        })
    } else {
        None
    };

    let status = child.wait();
    *job.stdin.lock().unwrap() = None;
    done.store(true, Ordering::SeqCst); // tell the tail thread ffmpeg finished
    if let Some(h) = tail {
        let _ = h.join(); // wait until it sends the last byte
    }

    let was_cancelled = job.cancel.load(Ordering::SeqCst);
    let ok = matches!(status, Ok(ref s) if s.success()) && !was_cancelled;
    match status {
        _ if was_cancelled => set_state(srv, &job, "cancelled", &out.to_string_lossy()),
        Ok(s) if s.success() => set_state(srv, &job, "done", &out.to_string_lossy()),
        Ok(s) => set_state(srv, &job, "failed", &format!("ffmpeg exit {}", s.code().unwrap_or(-1))),
        Err(e) => set_state(srv, &job, "failed", &e.to_string()),
    }

    if remote {
        // End goes over the chunk channel (FIFO): that way it lands AFTER the last byte
        if let Some(t) = &chunk_tx {
            let _ = t.send(ChunkMsg::End(job_id.into(), ok));
        }
        // the result already traveled to the client: don't leave anything on the server
        let _ = std::fs::remove_file(&out);
        if let Some(name) = out.file_name().and_then(|s| s.to_str()) {
            srv.out_names.lock().unwrap().remove(name);
        }
        let _ = std::fs::remove_dir(&srv.out_dir); // only removed if it ended up empty
    }
}

/// Reads `path` while ffmpeg writes it and sends the new bytes to the client.
/// Stops when `done` is true and there are no more bytes (real EOF).
fn stream_result(path: PathBuf, job_id: String, tx: Sender<ChunkMsg>, done: Arc<AtomicBool>) {
    // wait for ffmpeg to create the file
    let mut f = loop {
        match File::open(&path) {
            Ok(f) => break f,
            Err(_) => {
                if done.load(Ordering::SeqCst) {
                    return; // finished without creating output (failed)
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    };

    let _ = tx.send(ChunkMsg::Begin(job_id.clone()));
    let mut off: u64 = 0;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        // read `done` BEFORE the read: if it was done and the read returns 0, it's real EOF
        let fin = done.load(Ordering::SeqCst);
        if f.seek(SeekFrom::Start(off)).is_err() {
            break;
        }
        match f.read(&mut buf) {
            Ok(0) => {
                if fin {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(n) => {
                if tx.send(ChunkMsg::Data(job_id.clone(), buf[..n].to_vec())).is_err() {
                    return; // the client disconnected
                }
                off += n as u64;
            }
            Err(_) => break,
        }
    }
}

fn run_controller(srv: Arc<Server>) {
    loop {
        if !srv.listening.load(Ordering::SeqCst) {
            break;
        }
        let target = srv.target.load(Ordering::SeqCst).max(1);
        let running = srv.running.lock().unwrap().len();
        if running < target {
            let next = srv.queue.lock().unwrap().pop_front();
            if let Some(job_id) = next {
                srv.running.lock().unwrap().insert(job_id.clone(), ());
                let srv2 = srv.clone();
                std::thread::spawn(move || {
                    run_job(&srv2, &job_id);
                    srv2.running.lock().unwrap().remove(&job_id);
                });
                continue;
            }
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

// ===================== client connections =====================
fn handle_c2d(srv: &Arc<Server>, client_id: usize, client: &Arc<ClientHandle>, msg: C2D) {
    match msg {
        C2D::Submit {
            job_id,
            filename,
            size,
            crop,
            codec,
            preset,
            bitrate_idx,
            local_path,
        } => {
            let (source, real_size) = match local_path {
                Some(p) => match File::open(&p) {
                    Ok(f) => {
                        let sz = f.metadata().map(|m| m.len()).unwrap_or(0);
                        (Source::Local(Mutex::new(f)), sz)
                    }
                    Err(e) => {
                        let _ = client.ctrl.send(D2C::State {
                            job_id,
                            state: "failed".into(),
                            detail: format!("couldn't open local file: {e}"),
                        });
                        return;
                    }
                },
                None => (Source::Remote(client_id), size),
            };
            let job = Arc::new(Job {
                id: job_id.clone(),
                client: client_id,
                filename,
                size: real_size,
                source,
                crop: crop_from(crop),
                codec: codec_from(&codec),
                preset,
                bitrate_idx,
                state: Mutex::new("queued".into()),
                cancel: AtomicBool::new(false),
                stdin: Mutex::new(None),
            });
            srv.jobs.lock().unwrap().insert(job_id.clone(), job);
            srv.queue.lock().unwrap().push_back(job_id.clone());
            let _ = client.ctrl.send(D2C::State {
                job_id,
                state: "queued".into(),
                detail: String::new(),
            });
        }
        C2D::Cancel { job_id } => {
            // remove it from the queue in case it hasn't started yet (so it isn't dispatched)
            srv.queue.lock().unwrap().retain(|q| q != &job_id);
            if let Some(job) = srv.jobs.lock().unwrap().get(&job_id) {
                job.cancel.store(true, Ordering::SeqCst);
                if let Some(stdin) = job.stdin.lock().unwrap().as_mut() {
                    let _ = stdin.write_all(b"q");
                    let _ = stdin.flush();
                }
            }
        }
        C2D::SetParallel { n } => srv.target.store(n.clamp(1, 64), Ordering::SeqCst),
        C2D::Key { job_id, key } => {
            if let Some(job) = srv.jobs.lock().unwrap().get(&job_id) {
                if let Some(stdin) = job.stdin.lock().unwrap().as_mut() {
                    let _ = stdin.write_all(key.as_bytes());
                    let _ = stdin.flush();
                }
            }
        }
        C2D::Hello { .. } => {}
    }
}

fn handle_client(srv: Arc<Server>, mut rd: TcpStream) {
    let peer = rd.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let mut wr = match rd.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };

    // 1) Hello + auth
    let (tag, payload) = match read_frame(&mut rd) {
        Ok(x) => x,
        Err(_) => return,
    };
    if tag != TAG_CTRL {
        return;
    }
    let name = match serde_json::from_slice::<C2D>(&payload) {
        Ok(C2D::Hello { token, name }) if token == srv.token => name,
        _ => {
            let _ = write_json(&mut wr, &D2C::Denied { msg: "auth".into() });
            return;
        }
    };

    let id = srv.next_client.fetch_add(1, Ordering::SeqCst);
    let (ctrl_tx, ctrl_rx) = mpsc::channel::<D2C>();
    let (chunk_tx, chunk_rx) = mpsc::channel::<ChunkMsg>();
    let pending: Arc<Mutex<HashMap<u64, Sender<Vec<u8>>>>> = Arc::new(Mutex::new(HashMap::new()));
    let client = Arc::new(ClientHandle {
        ctrl: ctrl_tx.clone(),
        chunk: chunk_tx,
        pending: pending.clone(),
        next_req: AtomicU64::new(1),
    });
    srv.clients.lock().unwrap().insert(id, client.clone());
    eprintln!("[net] client '{name}' connected ({peer}) id={id}");

    // writer thread: the ONLY one that writes to the socket. It prioritizes control
    // (ReadBlock, etc.) over the result chunks → ffmpeg's input doesn't drown in the output.
    std::thread::spawn(move || loop {
        let mut idle = true;
        // 1) drain ALL available control first
        while let Ok(c) = ctrl_rx.try_recv() {
            idle = false;
            if write_json(&mut wr, &c).is_err() {
                return;
            }
        }
        // 2) a single chunk message (low priority)
        if let Ok(m) = chunk_rx.try_recv() {
            idle = false;
            let r = match m {
                ChunkMsg::Begin(j) => write_json(&mut wr, &D2C::ResultBegin { job_id: j }),
                ChunkMsg::Data(j, d) => write_result(&mut wr, &j, &d),
                ChunkMsg::End(j, ok) => write_json(&mut wr, &D2C::ResultEnd { job_id: j, ok }),
            };
            if r.is_err() {
                return;
            }
        }
        // 3) if there was nothing, block briefly on control (exits if the client left)
        if idle {
            match ctrl_rx.recv_timeout(Duration::from_millis(25)) {
                Ok(c) => {
                    if write_json(&mut wr, &c).is_err() {
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    });
    let _ = ctrl_tx.send(D2C::Welcome {
        msg: format!("hello {name}"),
    });

    // read loop
    loop {
        let (tag, payload) = match read_frame(&mut rd) {
            Ok(x) => x,
            Err(_) => break,
        };
        match tag {
            TAG_BLOCK => {
                if payload.len() >= 8 {
                    let req_id = u64::from_be_bytes(payload[..8].try_into().unwrap());
                    let data = payload[8..].to_vec();
                    if let Some(s) = pending.lock().unwrap().remove(&req_id) {
                        let _ = s.send(data);
                    }
                }
            }
            TAG_CTRL => {
                if let Ok(msg) = serde_json::from_slice::<C2D>(&payload) {
                    handle_c2d(&srv, id, &client, msg);
                }
            }
            _ => {}
        }
    }

    srv.clients.lock().unwrap().remove(&id);
    eprintln!("[net] client '{name}' disconnected id={id}");
}

// ===================== local test =====================
fn inject_test(srv: &Arc<Server>, file: &str, codec: &str, preset: &str, bitrate_idx: usize) {
    let f = match File::open(file) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("couldn't open {file}: {e}");
            std::process::exit(1);
        }
    };
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    let job_id = "test".to_string();
    let filename = Path::new(file)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("test")
        .to_string();
    let job = Arc::new(Job {
        id: job_id.clone(),
        client: 0,
        filename,
        size,
        source: Source::Local(Mutex::new(f)),
        crop: CropMode::None,
        codec: codec_from(codec),
        preset: preset.to_string(),
        bitrate_idx,
        state: Mutex::new("queued".into()),
        cancel: AtomicBool::new(false),
        stdin: Mutex::new(None),
    });
    srv.jobs.lock().unwrap().insert(job_id.clone(), job);
    srv.queue.lock().unwrap().push_back(job_id);
    eprintln!("[test] queued {file} ({size} bytes) codec={codec} preset={preset}");
}

fn arg_val(args: &[String], key: &str) -> Option<String> {
    args.iter().position(|a| a == key).and_then(|i| args.get(i + 1)).cloned()
}

/// The daemon's temporary output folder, inside the SYSTEM temp dir
/// (e.g. `%TEMP%\fast6d` on Windows, `/tmp/fast6d` on Linux). Kept separate from the
/// client's own `reencoded/` so running the daemon and a client in the same folder
/// doesn't clobber anything. Overridable with `--out` / `FAST6_OUT`.
fn default_out_dir() -> String {
    std::env::temp_dir().join("fast6d").to_string_lossy().into_owned()
}

// ===================== server start/stop =====================
fn make_server(token: String, ffmpeg: String, out_dir: String, shim_port: u16) -> Arc<Server> {
    let ffprobe = encoder::ffprobe_from_ffmpeg(&ffmpeg);
    Arc::new(Server {
        token,
        ffmpeg,
        ffprobe,
        out_dir: PathBuf::from(out_dir),
        shim_port,
        clients: Mutex::new(HashMap::new()),
        jobs: Mutex::new(HashMap::new()),
        queue: Mutex::new(VecDeque::new()),
        running: Mutex::new(HashMap::new()),
        target: AtomicUsize::new(2),
        enc_cache: Mutex::new(HashMap::new()),
        next_client: AtomicUsize::new(1),
        listening: AtomicBool::new(false),
        out_names: Mutex::new(HashSet::new()),
    })
}

/// Non-blocking accept loop that exits when `listening` turns false.
fn accept_loop(srv: Arc<Server>, listener: TcpListener, is_shim: bool) {
    listener.set_nonblocking(true).ok();
    loop {
        if !srv.listening.load(Ordering::SeqCst) {
            break;
        }
        match listener.accept() {
            Ok((sock, _)) => {
                sock.set_nonblocking(false).ok();
                // TCP_NODELAY: the client<->daemon connection carries the ReadBlocks in a serial
                // ping-pong (request a 256KB block → wait for the reply → request the next).
                // With Nagle on, over a real network (VM<->host) each round-trip hits the classic
                // Nagle+delayed-ACK stall (~40ms) → ffmpeg's input drops to ~6 MB/s = 2x. Over
                // loopback (client on the same host) it's unnoticeable → 5x.
                sock.set_nodelay(true).ok();
                let srv = srv.clone();
                if is_shim {
                    std::thread::spawn(move || handle_shim(srv, sock));
                } else {
                    std::thread::spawn(move || handle_client(srv, sock));
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => break,
        }
    }
}

/// Binds control + shim and starts the accept loops + controller. Err if it can't bind.
fn start_server(srv: &Arc<Server>, port: u16) -> io::Result<()> {
    let control = TcpListener::bind(("0.0.0.0", port))?;
    let shim = TcpListener::bind(("127.0.0.1", srv.shim_port))?;
    srv.listening.store(true, Ordering::SeqCst);
    {
        let srv = srv.clone();
        std::thread::spawn(move || accept_loop(srv, control, false));
    }
    {
        let srv = srv.clone();
        std::thread::spawn(move || accept_loop(srv, shim, true));
    }
    {
        let srv = srv.clone();
        std::thread::spawn(move || run_controller(srv));
    }
    Ok(())
}

// ===================== console mode (--headless / --test) =====================
fn run_console(args: &[String]) {
    let ffmpeg = arg_val(args, "--ffmpeg")
        .or_else(|| std::env::var("FAST6_FFMPEG").ok())
        .unwrap_or_else(|| r"C:\Util\ffmpeg\bin\ffmpeg.exe".into());
    let token = std::env::var("FAST6_TOKEN").unwrap_or_else(|_| "changeme".into());
    let port: u16 = arg_val(args, "--port")
        .or_else(|| std::env::var("FAST6_PORT").ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(7878);
    let shim_port: u16 = std::env::var("FAST6_SHIM_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7879);
    let out_dir = arg_val(args, "--out")
        .or_else(|| std::env::var("FAST6_OUT").ok())
        .unwrap_or_else(default_out_dir);

    let srv = make_server(token, ffmpeg, out_dir, shim_port);
    if let Err(e) = start_server(&srv, port) {
        eprintln!("couldn't bind (port in use?): {e}");
        std::process::exit(1);
    }
    eprintln!(
        "fast6d listening on 0.0.0.0:{port}  shim=127.0.0.1:{shim_port}  out={}  ffmpeg={}",
        srv.out_dir.display(),
        srv.ffmpeg
    );

    if let Some(f) = arg_val(args, "--test") {
        let codec = arg_val(args, "--codec").unwrap_or_else(|| "AV1".into());
        let preset = arg_val(args, "--preset").unwrap_or_else(|| "p1".into()); // p1 = best quality (UI convention)
        let bidx: usize = arg_val(args, "--bitrate").and_then(|s| s.parse().ok()).unwrap_or(2);
        inject_test(&srv, &f, &codec, &preset, bidx);
        loop {
            std::thread::sleep(Duration::from_millis(300));
            let st = srv
                .jobs
                .lock()
                .unwrap()
                .get("test")
                .map(|j| j.state.lock().unwrap().clone())
                .unwrap_or_default();
            if st == "done" || st == "failed" {
                eprintln!("[test] finished: {st}");
                std::process::exit(if st == "done" { 0 } else { 1 });
            }
        }
    }

    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

// ===================== GUI (eframe) + system tray =====================
// Get the HWND from an object with HasWindowHandle (CreationContext or Frame).
fn hwnd_of(h: &impl raw_window_handle::HasWindowHandle) -> isize {
    h.window_handle()
        .ok()
        .and_then(|wh| match wh.as_raw() {
            raw_window_handle::RawWindowHandle::Win32(w) => Some(w.hwnd.get()),
            _ => None,
        })
        .unwrap_or(0)
}

// "Hide"/show the window to/from the tray.
//
// IMPORTANT: we do NOT use SW_HIDE or minimize. On Windows, a hidden/minimized winit
// window makes eframe's event loop busy-spin a full CPU core (measured ~3% of a 32-core
// box = 100% of one core), and it happens BELOW our update() (which isn't even called
// while hidden), so we can't throttle it. Instead we keep the window SHOWN but move it
// far off-screen: invisible to the user, but winit is happy and the loop idles at ~0%.
// A brief SW_HIDE is only used to toggle WS_EX_TOOLWINDOW so no taskbar button lingers.
#[cfg(windows)]
static SAVED_POS: AtomicU64 = AtomicU64::new(0);

#[cfg(windows)]
fn win_hide(hwnd: isize) {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, GetWindowRect, SetWindowLongPtrW, SetWindowPos, ShowWindow, GWL_EXSTYLE,
        SW_HIDE, SW_SHOWNA, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, WS_EX_TOOLWINDOW,
    };
    if hwnd == 0 {
        return;
    }
    unsafe {
        let h = hwnd as *mut core::ffi::c_void;
        // remember where it was so we can put it back on show
        let mut r: RECT = std::mem::zeroed();
        if GetWindowRect(h, &mut r) != 0 && r.left > -30000 {
            let packed = (((r.left as u32) as u64) << 32) | (r.top as u32) as u64;
            SAVED_POS.store(packed, Ordering::SeqCst);
        }
        // toggle tool-window (drops the taskbar button); needs a hide to re-register
        ShowWindow(h, SW_HIDE);
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        SetWindowLongPtrW(h, GWL_EXSTYLE, ex | WS_EX_TOOLWINDOW as isize);
        // move far off-screen, then show WITHOUT activating (visible to winit -> no spin)
        SetWindowPos(h, std::ptr::null_mut(), -32000, -32000, 0, 0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        ShowWindow(h, SW_SHOWNA);
    }
}
#[cfg(windows)]
fn win_show(hwnd: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow,
        GWL_EXSTYLE, SW_HIDE, SW_SHOW, SWP_NOSIZE, SWP_NOZORDER, WS_EX_TOOLWINDOW,
    };
    if hwnd == 0 {
        return;
    }
    unsafe {
        let h = hwnd as *mut core::ffi::c_void;
        let packed = SAVED_POS.load(Ordering::SeqCst);
        let (x, y) = if packed != 0 {
            ((packed >> 32) as u32 as i32, (packed & 0xffff_ffff) as u32 as i32)
        } else {
            (200, 200)
        };
        // drop the tool-window flag (restore taskbar button)
        ShowWindow(h, SW_HIDE);
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        SetWindowLongPtrW(h, GWL_EXSTYLE, ex & !(WS_EX_TOOLWINDOW as isize));
        SetWindowPos(h, std::ptr::null_mut(), x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER);
        ShowWindow(h, SW_SHOW);
        SetForegroundWindow(h);
    }
}
#[cfg(not(windows))]
fn win_hide(_hwnd: isize) {}
#[cfg(not(windows))]
fn win_show(_hwnd: isize) {}

fn make_icon() -> tray_icon::Icon {
    let size = 32u32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    for px in rgba.chunks_exact_mut(4) {
        px[0] = 60;
        px[1] = 130;
        px[2] = 220;
        px[3] = 255;
    }
    tray_icon::Icon::from_rgba(rgba, size, size).expect("icon")
}

fn build_tray() -> (
    Option<tray_icon::TrayIcon>,
    tray_icon::menu::MenuId,
    tray_icon::menu::MenuId,
) {
    use tray_icon::menu::{Menu, MenuItem};
    let menu = Menu::new();
    let show = MenuItem::new("Show", true, None);
    let quit = MenuItem::new("Quit", true, None);
    let _ = menu.append(&show);
    let _ = menu.append(&quit);
    let tray = tray_icon::TrayIconBuilder::new()
        .with_tooltip("KciNicK Encoder Server")
        .with_menu(Box::new(menu))
        .with_icon(make_icon())
        .build()
        .ok();
    (tray, show.id().clone(), quit.id().clone())
}

struct DaemonApp {
    port: String,
    token: String,
    ffmpeg: String,
    shim_port: u16,
    server: Option<Arc<Server>>,
    status: String,
    hwnd: Arc<AtomicIsize>,
    // True while "in the tray" (moved off-screen by win_hide). Used to skip repaints so we
    // don't render the off-screen window; the tray callbacks clear it and wake us on Show.
    hidden: Arc<AtomicBool>,
    _tray: Option<tray_icon::TrayIcon>,
}

impl DaemonApp {
    fn new(cc: &eframe::CreationContext<'_>, ffmpeg: String) -> Self {
        let (tray, show_id, quit_id) = build_tray();

        // Window HWND (from CreationContext if it's already there; otherwise taken in update()).
        let hwnd = Arc::new(AtomicIsize::new(hwnd_of(cc)));

        let hidden = Arc::new(AtomicBool::new(false));

        // Tray events go through a CALLBACK (runs on the message pump, NOT in update()),
        // so it works even when the window is hidden. Show/close via Win32. On show we
        // clear `hidden` and request a repaint to restart the (stopped) repaint loop.
        {
            let hw = Arc::clone(&hwnd);
            let hd = Arc::clone(&hidden);
            let ectx = cc.egui_ctx.clone();
            tray_icon::menu::MenuEvent::set_event_handler(Some(move |ev: tray_icon::menu::MenuEvent| {
                if ev.id == show_id {
                    hd.store(false, Ordering::SeqCst);
                    win_show(hw.load(Ordering::SeqCst));
                    ectx.request_repaint();
                } else if ev.id == quit_id {
                    std::process::exit(0);
                }
            }));
        }
        {
            let hw = Arc::clone(&hwnd);
            let hd = Arc::clone(&hidden);
            let ectx = cc.egui_ctx.clone();
            tray_icon::TrayIconEvent::set_event_handler(Some(move |ev: tray_icon::TrayIconEvent| {
                if let tray_icon::TrayIconEvent::Click {
                    button: tray_icon::MouseButton::Left,
                    ..
                } = ev
                {
                    hd.store(false, Ordering::SeqCst);
                    win_show(hw.load(Ordering::SeqCst));
                    ectx.request_repaint();
                }
            }));
        }

        DaemonApp {
            port: "7878".into(),
            token: std::env::var("FAST6_TOKEN").unwrap_or_else(|_| "changeme".into()),
            ffmpeg,
            shim_port: 7879,
            server: None,
            status: "stopped".into(),
            hwnd,
            hidden,
            _tray: tray,
        }
    }

    fn start(&mut self) {
        if self.server.is_some() {
            return;
        }
        let port: u16 = self.port.trim().parse().unwrap_or(7878);
        let srv = make_server(
            self.token.clone(),
            self.ffmpeg.clone(),
            default_out_dir(),
            self.shim_port,
        );
        match start_server(&srv, port) {
            Ok(()) => {
                self.server = Some(srv);
                self.status = format!("listening on :{port}");
            }
            Err(e) => self.status = format!("didn't start (port in use?): {e}"),
        }
    }

    fn stop(&mut self) {
        if let Some(srv) = self.server.take() {
            srv.listening.store(false, Ordering::SeqCst);
        }
        self.status = "stopped".into();
    }
}

impl eframe::App for DaemonApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // grab the HWND the first time (if CreationContext didn't have it)
        if self.hwnd.load(Ordering::SeqCst) == 0 {
            let h = hwnd_of(frame);
            if h != 0 {
                self.hwnd.store(h, Ordering::SeqCst);
            }
        }
        let hwnd = self.hwnd.load(Ordering::SeqCst);

        // close: if it's running -> hide to the tray; if it's stopped -> quit.
        if ctx.input(|i| i.viewport().close_requested()) {
            if self.server.is_some() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.hidden.store(true, Ordering::SeqCst);
                win_hide(hwnd);
            } else {
                std::process::exit(0);
            }
        }
        if ctx.input(|i| i.viewport().minimized == Some(true)) {
            self.hidden.store(true, Ordering::SeqCst);
            win_hide(hwnd);
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            const LABEL_W: f32 = 90.0;
            const ROW_H: f32 = 26.0;

            ui.add_space(4.0);
            ui.heading("KciNicK Encoder Server");
            ui.add_space(10.0);

            let running = self.server.is_some();

            // helper: right-aligned fixed-width label + field on the left
            let field_row = |ui: &mut egui::Ui, label: &str, value: &mut String, enabled: bool| {
                let mut resp = None;
                ui.horizontal(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(LABEL_W, ROW_H),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.label(label);
                        },
                    );
                    let w = ui.available_width().max(120.0);
                    resp = Some(ui.add_enabled(
                        enabled,
                        egui::TextEdit::singleline(value)
                            .desired_width(w)
                            .margin(egui::vec2(6.0, 5.0)),
                    ));
                });
                resp.unwrap()
            };

            field_row(ui, "Port:", &mut self.port, !running);
            ui.add_space(6.0);
            field_row(ui, "Token:", &mut self.token, !running);

            ui.add_space(14.0);

            // big Start / Stop buttons, aligned with the fields
            ui.horizontal(|ui| {
                ui.add_space(LABEL_W + ui.spacing().item_spacing.x);
                let btn = egui::vec2(120.0, 34.0);
                if ui
                    .add_enabled(
                        !running,
                        egui::Button::new(egui::RichText::new("▶  Start").size(16.0))
                            .min_size(btn),
                    )
                    .clicked()
                {
                    self.start();
                }
                if ui
                    .add_enabled(
                        running,
                        egui::Button::new(egui::RichText::new("■  Stop").size(16.0))
                            .min_size(btn),
                    )
                    .clicked()
                {
                    self.stop();
                }
            });

            ui.add_space(14.0);

            // status
            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(LABEL_W, ROW_H),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        ui.label("Status:");
                    },
                );
                let col = if running {
                    egui::Color32::from_rgb(120, 220, 140)
                } else {
                    egui::Color32::GRAY
                };
                ui.colored_label(col, egui::RichText::new(&self.status).size(14.0));
            });

            if let Some(srv) = &self.server {
                let clients = srv.clients.lock().unwrap().len();
                let jobs = srv.running.lock().unwrap().len();
                let queue = srv.queue.lock().unwrap().len();
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(LABEL_W, ROW_H),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.label("");
                        },
                    );
                    ui.label(
                        egui::RichText::new(format!(
                            "clients {clients}   ·   encoding {jobs}   ·   queue {queue}"
                        ))
                        .weak(),
                    );
                });
            }
        });

        // Repaint policy: none while in the tray (off-screen), so we don't waste CPU
        // rendering an invisible window with llvmpipe. While visible: 1s tick when idle
        // (static UI), 250ms only when there's real activity to show.
        if !self.hidden.load(Ordering::SeqCst) {
            let busy = self.server.as_ref().is_some_and(|s| {
                !s.clients.lock().unwrap().is_empty()
                    || !s.running.lock().unwrap().is_empty()
                    || !s.queue.lock().unwrap().is_empty()
            });
            ctx.request_repaint_after(if busy {
                Duration::from_millis(250)
            } else {
                Duration::from_secs(1)
            });
        }
    }
}

fn run_gui(args: &[String]) {
    let ffmpeg = arg_val(args, "--ffmpeg")
        .or_else(|| std::env::var("FAST6_FFMPEG").ok())
        .unwrap_or_else(|| r"C:\Util\ffmpeg\bin\ffmpeg.exe".into());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([420.0, 250.0])
            .with_min_inner_size([380.0, 230.0])
            // No minimize button: minimizing a winit window busy-spins a CPU core on Windows,
            // and eframe stops calling update() while minimized so we can't react to it.
            // The close (X) button is the way to the tray (it fires update() reliably).
            .with_minimize_button(false),
        ..Default::default()
    };
    let res = eframe::run_native(
        "KciNicK Encoder Server",
        options,
        Box::new(move |cc| Ok(Box::new(DaemonApp::new(cc, ffmpeg)))),
    );
    // The release GUI has no console: if run_native fails (e.g. no OpenGL) we leave the
    // reason in a log instead of dying silently like the previous `let _ =` did.
    if let Err(e) = &res {
        log_fatal(&format!("{e}"));
    }
}

/// Writes the reason for a fatal exit to a log (next to the exe and, as a fallback, in %TEMP%).
fn log_fatal(msg: &str) {
    let line = format!("fast6d fatal: {msg}\r\n");
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let _ = std::fs::write(dir.join("fast6d_error.log"), &line);
        }
    }
    if let Ok(tmp) = std::env::var("TEMP") {
        let _ = std::fs::write(std::path::Path::new(&tmp).join("fast6d_error.log"), &line);
    }
}

fn main() {
    // The daemon's mini-GUI is egui/glow and shares its folder with Mesa's opengl32.dll.
    // Like fast6, we force the llvmpipe software driver (unless the environment already
    // provides one) to get CPU OpenGL so the window opens both on a GPU-less VM and on the
    // host, where that bundled Mesa opengl32.dll now shadows the system one. NVENC encoding
    // is done by ffmpeg separately: it's not affected.
    #[cfg(windows)]
    {
        if std::env::var_os("GALLIUM_DRIVER").is_none() {
            std::env::set_var("GALLIUM_DRIVER", "llvmpipe");
        }
    }
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--headless") || arg_val(&args, "--test").is_some() {
        run_console(&args);
    } else {
        run_gui(&args);
    }
}
