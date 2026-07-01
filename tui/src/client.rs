//! fast6d daemon client (port of the non-egui logic from `src/main.rs`).
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use crate::app::{Progress, Slot, SlotState, SLOTS};
use crate::encoder;
use crate::proto::{self, C2D, D2C};

/// Outbound message to the daemon client's writer.
pub enum OutMsg {
    Ctrl(C2D),
    Block(u64, Vec<u8>),
}

/// Reads a block from a local file (to serve the daemon's ReadBlocks).
pub fn read_file_block(path: &std::path::Path, offset: u64, len: u32) -> std::io::Result<Vec<u8>> {
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

/// Sends a key to a slot: to the daemon (daemon mode) or to the local ffmpeg.
pub fn send_key(slot: &Slot, daemon_mode: bool, dtx: &Option<Sender<OutMsg>>, key: &str) {
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
pub fn dto_to_summary(d: &proto::SummaryDto) -> encoder::StreamSummary {
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

/// Assigns (or returns) a job's slot. None if there's no free slot.
pub fn ensure_slot(slot_of: &Mutex<HashMap<String, usize>>, job_id: &str) -> Option<usize> {
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
pub fn client_out_path(src: &std::path::Path) -> PathBuf {
    let dir = src
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("reencoded");
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("output");
    dir.join(format!("{stem}.mp4"))
}

/// Reads the daemon's messages and pours them into the slots.
pub fn daemon_reader(
    mut rd: TcpStream,
    slots: &Arc<Vec<Slot>>,
    files: &Arc<Mutex<HashMap<String, PathBuf>>>,
    slot_of: &Arc<Mutex<HashMap<String, usize>>>,
    tx: &Sender<OutMsg>,
) {
    let mut result_files: HashMap<String, File> = HashMap::new();
    loop {
        let (tag, payload) = match proto::read_frame(&mut rd) {
            Ok(x) => x,
            Err(_) => break,
        };
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
                    // no slot yet
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
            D2C::Streams { job_id, source, target } => {
                if let Some(i) = slot_of.lock().unwrap().get(&job_id).copied() {
                    *slots[i].streams.lock().unwrap() = dto_to_summary(&source);
                    *slots[i].target.lock().unwrap() = dto_to_summary(&target);
                }
            }
            D2C::Progress {
                job_id, frame, fps, q, size, time, bitrate, speed, time_sec, ..
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
