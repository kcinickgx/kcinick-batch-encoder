# KciNicK Batch Encoder

A fast batch video encoder for **AV1 / H.265 / H.264**, with a desktop GUI and an
optional **remote encoding daemon**. It encodes on the **GPU (NVENC)** and automatically
falls back to **CPU** when the card can't handle the chosen codec.

Written in Rust (egui/eframe). Windows-first.

---

## Features

- **Three codecs**: AV1, H.265 (HEVC) and H.264, with real GPU capability detection
  (e.g. an RTX 20-series can't do AV1 on NVENC → it falls back to `libsvtav1`/`libaom`).
- **Batch + recursive**: point it at a folder and it encodes every video in it and all
  its subfolders, writing each result to a `reencoded/` folder next to the source.
- **Parallel encodes**: run up to 4 jobs at once.
- **Smart bitrate**: per-resolution target tiers (4K vs the rest), capped by the source
  bitrate so it never inflates a file that's already smaller.
- **10-bit aware**: preserves 10-bit (P010 / `yuv420p10le`) instead of silently
  dropping to 8-bit.
- **Cinematic crop**: one-click 16:9 → 21:9 crop (centered, top-third, or none).
- **Live parsed output**: source/target stream info (codec, resolution, fps, bit depth,
  bitrate, duration) plus a real-time progress panel with a computed **ETA**.
- **Remote mode (daemon)**: run the encoder on a powerful machine (the one with the GPU)
  and drive it from anywhere. The client **doesn't upload the whole file** — the daemon
  reads the source **on demand**, and the encoded result is **streamed back as it's
  produced**. NAT-friendly (the client dials out).

---

## Components

| Binary | What it is |
|---|---|
| `fast6` (`fast6.exe`) | The **GUI client** (egui, Windows). Queue files, watch progress, run locally or against a remote daemon. |
| `fast6d` (`fast6d.exe`) | The **encoding daemon** (Windows): a queue + parallel workers + an on-demand read shim. Has a small GUI and a system-tray icon. |
| `fast6t` | The **TUI client** (ratatui, **Linux**). Same layout and features as the GUI, in the terminal, with mouse support. |

All three share the encoding logic (`src/encoder.rs`) and the client↔daemon protocol
(`src/proto.rs`).

---

## Requirements

- **Windows** (10/11).
- **ffmpeg** with `ffprobe` next to it. A recent build with NVENC support is recommended
  (e.g. from gyan.dev or BtbN).
- An **NVIDIA GPU** for hardware encoding (optional — it falls back to CPU).
- **Rust** (stable) to build.

## Build

```
cargo build --release --workspace
```

or run `build.bat`, which builds both binaries and copies `fast6.exe` and `fast6d.exe`
to the project root. Binaries land in `target/release/`.

---

## Running on a VM / headless (software OpenGL)

The GUI renders with OpenGL (egui/glow). On a normal Windows machine with a GPU this
just works — it uses the system's hardware OpenGL. **On a VM with no 3D acceleration**
(e.g. VMware Horizon, some RDP sessions) there's no OpenGL driver and the window won't
open; you'll see `egui_glow requires opengl 2.0+` in `fast6_error.log` (written next to
the exe and in `%TEMP%`).

Fix: drop **Mesa's software-OpenGL DLLs** next to `fast6.exe` / `fast6d.exe`:

- `opengl32.dll`
- `libgallium_wgl.dll`

Windows loads DLLs from the exe's folder before the system ones, so these shadow the
system `opengl32.dll`. The app then sets `GALLIUM_DRIVER=llvmpipe` automatically to get
CPU-rendered OpenGL, and the window opens. NVENC encoding is done by ffmpeg in a separate
process, so it's **not** affected by software rendering.

Where to get them: prebuilt Mesa3D for Windows — the
[mesa-dist-win](https://github.com/pal1000/mesa-dist-win/releases) releases (the
"desktop-gl" / MSVC package ships `opengl32.dll` + `libgallium_wgl.dll`, x64).

Notes:
- **Not needed on normal Windows** — the DLLs are only required where hardware OpenGL is
  missing. If you leave them next to the exe on a normal PC it still works, just rendered
  in software; set `GALLIUM_DRIVER=d3d12` to force hardware, or simply don't ship the DLLs.
- These DLLs are **not tracked in this repo** (`.gitignore`) because of their size
  (`libgallium_wgl.dll` is ~59 MB). Download them from the link above when you need them.

---

## Usage

### Local mode

1. Launch `fast6.exe`.
2. Make sure the **ffmpeg** path is correct (it's remembered in the registry).
3. Pick a source folder, codec, preset, bitrate tier and crop.
4. Hit **▶**. Each file is encoded to `<its folder>/reencoded/<name>.mp4`.

### Remote mode (client + daemon)

On the **encoding machine** (GPU + ffmpeg):

1. Launch `fast6d.exe`. Set a port and token, hit **Start**. It minimizes to the tray.

On the **client machine**:

1. Launch `fast6.exe`, switch the mode selector to **Remote**.
2. Enter the daemon's `server:` address (`host:port`) and `token:`, hit **Connect**.
3. Pick a source folder and settings, hit **▶**.

The client serves the source bytes on demand and the daemon streams the encoded `.mp4`
back into `<source folder>/reencoded/` while it encodes — nothing is left on the server.

> Over the internet, put both machines on a mesh VPN like **Tailscale** to sidestep NAT.

### TUI client (Linux)

`fast6t` is a **Linux** terminal client with the same layout and features as the GUI
(Local and Remote modes, live streams/progress/ETA, crop/codec/preset/bitrate, per-slot
keys), rendered with [ratatui](https://ratatui.rs) and full mouse support (click, hover,
scroll) plus a built-in file explorer with per-file/per-folder include checkboxes.

Build it (on Linux):

```
cargo build --release -p fast6tui     # binary: target/release/fast6t
```

It reuses the same `encoder.rs` and `proto.rs` as the GUI/daemon, so it talks to the
**same daemon**. Config is stored in `~/.config/fast6/config.json` (the Linux equivalent
of the GUI's Windows-registry settings). Typical use: run `fast6d` on the Windows box with
the GPU, run `fast6t` on a Linux machine, switch it to **Remote**, and encode your local
files against the remote GPU. `ffmpeg`/`ffprobe` must be on `PATH` (or set the path) only
for **Local** mode.

> `fast6t` also compiles on Windows (crossterm is cross-platform), but it's built and
> intended for Linux terminals.

---

## How encoding is decided

- **Codec → encoder**: `detect_encoder` runs a tiny probe encode to `null` with the
  codec's NVENC encoder; if it fails, it falls back to CPU.
  - H.264 → `h264_nvenc` / `libx264`
  - H.265 → `hevc_nvenc` / `libx265`
  - AV1 → `av1_nvenc` / `libsvtav1` → `libaom-av1`
- **Bitrate tiers**: choose a high/low pair (e.g. `6k/4k`). *High* is used for 4K
  (largest side ≥ 3000 px, so a vertical 4K counts too), *low* for everything else.
  `maxrate = ×1.5`, `bufsize = ×2`. The target is capped at the source's own bitrate.
- **10-bit**: detected from the source; forces `-pix_fmt p010le` (NVENC) or
  `yuv420p10le` (CPU). Note: `h264_nvenc` doesn't support 10-bit, so H.264 stays 8-bit.
- **Audio**: re-encoded to AAC, channel layout preserved (7.1 / 5.1 / stereo / mono),
  bitrate scaled with channel count.

---

## Remote architecture (on-demand read + live result)

The client never uploads the whole file up front. The daemon runs a local HTTP **shim**
that serves the input by byte ranges; ffmpeg/ffprobe read from
`http://127.0.0.1:<shim>/<job>`. Each range is resolved by asking the client for that
block over the same TCP connection (`ReadBlock`), so it works through NAT and starts
instantly.

The encoded output is muxed as **fragmented MP4** (`-movflags
+frag_keyframe+empty_moov+default_base_moof`) so it can be **tailed and streamed back to
the client as it grows**, without waiting for the encode to finish. On the daemon, a
single writer thread multiplexes control messages and result chunks over one socket,
**prioritizing control** so the input-feeding `ReadBlock` requests never get starved by
the outgoing result stream.

---

## Project layout

```
src/main.rs        GUI client (egui, Windows)
src/encoder.rs     shared: probe, codec/encoder selection, ffmpeg arg building
src/proto.rs       shared: client<->daemon wire protocol
daemon/src/main.rs encoding daemon (workers, shim, tray GUI)
tui/src/           TUI client (ratatui, Linux) — app/client/controller/ui/widgets/…
```
