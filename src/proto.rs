//! Wire protocol shared between the client (GUI/CLI) and the daemon (fast6d).
//! Framing: [tag:u8][len:u32 BE][payload].  tag=1 JSON, tag=2 binary block.

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

pub const TAG_CTRL: u8 = 1; // payload = JSON (C2D or D2C)
pub const TAG_BLOCK: u8 = 2; // payload = [req_id:u64 BE][bytes]  (client -> daemon)
pub const TAG_RESULT: u8 = 3; // payload = [jid_len:u16 BE][jid][bytes]  (daemon -> client)
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

pub fn write_frame(w: &mut impl Write, tag: u8, payload: &[u8]) -> io::Result<()> {
    let mut hdr = [0u8; 5];
    hdr[0] = tag;
    hdr[1..5].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    w.write_all(&hdr)?;
    w.write_all(payload)?;
    w.flush()
}

pub fn read_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 5];
    r.read_exact(&mut hdr)?;
    let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok((hdr[0], buf))
}

/// Writes a control message (JSON) as a TAG_CTRL frame.
pub fn write_json<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    write_frame(w, TAG_CTRL, &serde_json::to_vec(msg).unwrap_or_default())
}

/// Writes a data block (the reply to a ReadBlock) as a TAG_BLOCK frame.
pub fn write_block(w: &mut impl Write, req_id: u64, data: &[u8]) -> io::Result<()> {
    let mut payload = Vec::with_capacity(8 + data.len());
    payload.extend_from_slice(&req_id.to_be_bytes());
    payload.extend_from_slice(data);
    write_frame(w, TAG_BLOCK, &payload)
}

/// Writes a chunk of the result file (daemon -> client) as a TAG_RESULT frame.
/// payload = [jid_len:u16 BE][jid utf8][file bytes]
pub fn write_result(w: &mut impl Write, job_id: &str, data: &[u8]) -> io::Result<()> {
    let jid = job_id.as_bytes();
    let mut payload = Vec::with_capacity(2 + jid.len() + data.len());
    payload.extend_from_slice(&(jid.len() as u16).to_be_bytes());
    payload.extend_from_slice(jid);
    payload.extend_from_slice(data);
    write_frame(w, TAG_RESULT, &payload)
}

/// Parses the payload of a TAG_RESULT frame into (job_id, bytes).
pub fn parse_result(payload: &[u8]) -> Option<(String, &[u8])> {
    if payload.len() < 2 {
        return None;
    }
    let jl = u16::from_be_bytes([payload[0], payload[1]]) as usize;
    if payload.len() < 2 + jl {
        return None;
    }
    let jid = String::from_utf8_lossy(&payload[2..2 + jl]).into_owned();
    Some((jid, &payload[2 + jl..]))
}

// ===================== messages =====================
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "t")]
pub enum C2D {
    Hello {
        token: String,
        name: String,
    },
    Submit {
        job_id: String,
        filename: String,
        size: u64,
        crop: u8,           // 1=centered 2=1/3 3=none
        codec: String,      // H264/H265/AV1
        preset: String,     // p1..p7
        bitrate_idx: usize, // 0..3
        local_path: Option<String>,
    },
    Cancel {
        job_id: String,
    },
    SetParallel {
        n: usize,
    },
    Key {
        job_id: String,
        key: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct SummaryDto {
    pub dur: String,
    pub dur_sec: f64,
    pub v_codec: String,
    pub v_res: String,
    pub v_fps: String,
    pub v_bits: String,
    pub v_fmt: String,
    pub v_bitrate: String,
    pub a_codec: String,
    pub a_layout: String,
    pub a_rate: String,
    pub a_bitrate: String,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "t")]
pub enum D2C {
    Welcome { msg: String },
    Denied { msg: String },
    ReadBlock { req_id: u64, job_id: String, offset: u64, len: u32 },
    State { job_id: String, state: String, detail: String },
    Streams {
        job_id: String,
        source: SummaryDto,
        target: SummaryDto,
        #[serde(default)]
        cmd: String, // shortened ffmpeg command ("ffmpeg … {source} … {target}")
    },
    Progress {
        job_id: String,
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
    },
    /// The daemon is about to start sending the result file (via TAG_RESULT).
    ResultBegin { job_id: String },
    /// Finished sending the result. ok=false if the encode failed/was cancelled.
    ResultEnd { job_id: String, ok: bool },
}
