//! Local scheduler: launches ffmpeg on this machine (port of the logic in `src/main.rs`).
use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::app::{Progress, Slot, SlotState, SLOTS};
use crate::encoder::{self, CropMode};

/// Config captured when starting (fixed for the whole queue).
#[derive(Clone)]
pub struct RunCfg {
    pub ffmpeg: String,
    pub ffprobe: PathBuf,
    pub cwd: String,
    pub crop: CropMode,
    pub preset: String,
    pub high_kbps: i64,
    pub low_kbps: i64,
    pub codec: encoder::Codec,
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

/// Parses the line "frame=.. fps=.. q=.. size=.. time=.. bitrate=.. speed=..".
pub fn parse_progress(line: &str) -> Option<Progress> {
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

/// Scheduler: dispatches from the queue to the slots honoring `target` (live).
pub fn spawn_controller(
    slots: Arc<Vec<Slot>>,
    queue: Arc<Mutex<VecDeque<PathBuf>>>,
    target: Arc<AtomicUsize>,
    running: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    cfg: RunCfg,
) {
    std::thread::spawn(move || {
        let enc = encoder::detect_encoder(&cfg.ffmpeg, cfg.codec);
        let mut children: Vec<Option<Child>> = (0..SLOTS).map(|_| None).collect();

        loop {
            // 1) reap finished ones
            for i in 0..SLOTS {
                if let Some(child) = &mut children[i] {
                    if let Ok(Some(st)) = child.try_wait() {
                        let code = st.code().unwrap_or(-1);
                        if code == 0 {
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

            // 2) if cancelling: empty the queue and send 'q' to the live ones
            if cancelling {
                queue.lock().unwrap().clear();
                for i in 0..SLOTS {
                    if children[i].is_some() {
                        slots[i].send_quit();
                    }
                }
            }

            // 3) dispatch while fewer than `target` are running and there's a free slot
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
                            let nm = file
                                .file_name()
                                .and_then(|s| s.to_str())
                                .unwrap_or("???")
                                .to_string();
                            *slots[idx].name.lock().unwrap() = nm;
                            *slots[idx].info.lock().unwrap() = "couldn't read (ffprobe)".into();
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
    // shortened command: "ffmpeg ..." with {source}/{target} instead of the long paths
    let mut disp = String::from("ffmpeg");
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

/// Reads ffmpeg's stderr, drops ANSI, handles '\r' and parses the progress.
fn pump_output<R: Read>(
    mut reader: R,
    out: Arc<Mutex<String>>,
    prog: Arc<Mutex<Option<Progress>>>,
) {
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

                if committed.len() > 200_000 {
                    let mut idx = committed.len() - 150_000;
                    while idx < committed.len() && !committed.is_char_boundary(idx) {
                        idx += 1;
                    }
                    committed = committed.split_off(idx);
                }

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
