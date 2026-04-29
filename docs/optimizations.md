# Image Slider Video Creator — Optimization Methods

This document explains the 5 performance optimizations applied across the versions of this project,
from the original `main.rs` to the fastest `fast-slider-v2`.

---

## Overview

| Version | Methods Applied | Total Time (20 images) |
|---|---|---|
| `main.rs` (original) | none | not measured |
| `fast-slider` | #1 + #2 + #3 | ~12.46s |
| `fast-slider-v2` | #1 + #2 + #3 + #4 + #5 | ~4.15s |

---

## Method #1 — Skip the Intermediate File (Pipe to ffmpeg)

### The Problem (original)

```
[OpenCV VideoWriter] ──writes──▶ output.mp4 (4344×7728, mp4v codec, ~GBs)
                                      │
                                      ▼
                              [ffmpeg reads it]
                                      │
                                      ▼
                              output_2160p.mp4
```

The original code used OpenCV's `VideoWriter` to write every frame at **full resolution (4344×7728)**
into a temporary `output.mp4` file. This file could be several gigabytes.
After `VideoWriter` finished, `ffmpeg` opened that file and re-encoded it from scratch.

This means:
- Every frame is **written to disk twice** (once by VideoWriter, once read back by ffmpeg)
- The intermediate file uses gigabytes of disk space
- You wait for the full write before encoding even begins

### The Fix

```
[Rust code] ──stdin pipe──▶ [ffmpeg process] ──▶ output.mp4
```

Instead of writing a file, Rust spawns ffmpeg as a child process and writes raw BGR24 pixel data
directly to ffmpeg's `stdin`. ffmpeg receives frames and encodes them in real time.

**No intermediate file is ever written to disk.**

### Code (Rust)

```rust
let mut ffmpeg = Command::new("ffmpeg")
    .args(["-f", "rawvideo", "-pixel_format", "bgr24", "-i", "pipe:0", ...])
    .stdin(Stdio::piped())
    .spawn()?;

// Write raw frame pixels directly to ffmpeg
ffmpeg.stdin.write_all(frame.data_bytes()?)?;
```

### Why it's Faster

- Eliminates disk write of the full-res intermediate file (potentially GBs)
- ffmpeg starts encoding immediately — no waiting for the full file
- Single pass instead of two passes

---

## Method #2 — Resize Images During Load (Not After)

### The Problem (original)

The original code:
1. Wrote **4344×7728** (full resolution) frames into the intermediate file
2. Then ffmpeg applied `scale=-2:2160` during re-encoding

Every frame written was 4344 × 7728 × 3 bytes = **~96 MB per frame**.
For a 30fps video with 597 frames, that's over **57 GB** of pixel data processed.

### The Fix

Resize each image to 2160p **the moment it is loaded**, before writing a single frame:

```
Load 4344×7728 image
        │
        ▼
  INTER_AREA resize to 1212×2160  ◀── happens once per image
        │
        ▼
  Write 1212×2160 frames to ffmpeg pipe
```

1212 × 2160 × 3 bytes = **~7.5 MB per frame** — about **13× smaller**.

### Code (Rust)

```rust
fn decode_one(path: &PathBuf, frame_size: Size) -> Option<Mat> {
    let img = imgcodecs::imread(path, IMREAD_COLOR)?;
    let mut out = Mat::default();
    imgproc::resize(&img, &mut out, frame_size, 0.0, 0.0, INTER_AREA)?;
    Some(out)
}
```

### Why INTER_AREA?

| Algorithm | Best for | Speed |
|---|---|---|
| `INTER_NEAREST` | Pixel art | Fastest |
| `INTER_LINEAR` | General upscale | Fast |
| `INTER_AREA` | **Downscaling** | Fast, best quality for shrink |
| `INTER_LANCZOS4` | Upscaling, sharpness | Slowest |

We are downscaling (4344→1212), so `INTER_AREA` gives the best quality at good speed.

---

## Method #3 — 1-Image Background Thread Preload

### The Problem

Without preloading, the main thread does this sequentially:

```
[decode image 1] → [write 27 hold frames] → [decode image 2] → [write 27 hold frames] → ...
```

While hold frames are being written to ffmpeg, the CPU is idle (waiting for pipe writes to complete).
While image N+1 is being decoded, ffmpeg is idle (no frames coming in).

### The Fix

Spawn a background thread that decodes the **next** image while the main thread is busy writing
hold frames for the **current** image:

```
Main thread:   [write hold frames 1] ──────────▶ [write hold frames 2] ──────▶ ...
                        │                                  │
BG thread:     [decode image 2] ──done──▶        [decode image 3] ──done──▶ ...
```

Disk I/O (decode) overlaps with CPU/pipe work (writing frames).

### Code (Rust)

```rust
// Kick off next image decode on a background thread
let next_handle = thread::spawn(move || {
    decode_one(&next_path, frame_size)
});

// Write hold frames for current image (BG thread decodes next concurrently)
for _ in 0..hold_frames {
    ffmpeg_stdin.write_all(hold_data)?;
}

// Join the background thread — next image should be ready by now
let next = next_handle.join().unwrap();
```

### Why it Helps

- Decode time (~0.5–0.8s) is hidden inside the hold-frame write time
- CPU is never idle waiting for decode, and decode never waits for the pipe

---

## Method #4 — Apple Silicon VideoToolbox Hardware Encoder

### The Problem

libx264 is a **software encoder** — it uses all available CPU cores to compute H.264 compression.
This competes directly with the decode/resize threads for CPU time.

On Apple Silicon (M1/M2/M3/M4), the chip contains a **dedicated hardware media engine**
that encodes H.264 independently of the CPU cores. It sits idle unless explicitly used.

### The Fix

Replace `-vcodec libx264` with `-vcodec h264_videotoolbox` in the ffmpeg command.

```
libx264:           [CPU core 1][CPU core 2]...[CPU core N] → encode H.264
                        ▲ competes with decode threads ▲

h264_videotoolbox: [Hardware Media Engine] → encode H.264   (CPU = 0%)
                   [CPU core 1][CPU core 2]...[CPU core N] → free for decode
```

### Quality Setting

VideoToolbox uses `-q:v` (0–100, higher = better quality):

| `-q:v` value | Quality |
|---|---|
| 30–50 | Good (visible compression) |
| 70 | High |
| **85** | **Near-lossless (used here)** |
| 95–100 | Lossless (very large files) |

libx264 uses `-crf` (0–51, lower = better):

| `-crf` value | Quality |
|---|---|
| 18 | Near-lossless (used in fast-slider) |
| 23 | Default |
| 28 | Lower quality |

### Code (Rust)

```rust
fn videotoolbox_available() -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("h264_videotoolbox"))
        .unwrap_or(false)
}

// In spawn_ffmpeg:
if use_hwenc {
    args.extend(["-vcodec", "h264_videotoolbox", "-q:v", "85"]);
} else {
    args.extend(["-vcodec", "libx264", "-crf", "18", "-preset", "medium"]);
}
```

### Why it's the Biggest Win

Offloading encoding to dedicated silicon means:
- All 8 CPU cores are free for image decode
- The hardware engine runs in parallel with everything else
- ffmpeg finalization time dropped from 0.70s → 0.24s
- Per-image time dropped from ~0.6s → ~0.18s

---

## Method #5 — 2-Image Lookahead Buffer

### The Problem with Method #3

With only 1 image preloading at a time, if a large image takes **0.88s** to decode but hold-frame
writes only take **0.5s**, the main thread still stalls 0.38s waiting for decode to finish.

```
Main:      [write hold 0.5s] [STALL 0.38s] [write fade] [write hold 0.5s] [STALL ...
BG thread:                 [decode 0.88s]               [decode 0.88s]
```

### The Fix

Use a dedicated **decode worker thread** with a **bounded channel (capacity=2)**.

The worker runs continuously, always staying 2 images ahead:

```
Decode thread:  [img1]──[img2]──[img3]──[img4]──...
                  │       │       │
Main thread:    reads   reads   reads (always has next image ready)
```

The channel with capacity=2 acts as a buffer:
- If the channel is full (main thread is slow), the decode thread **blocks** — memory stays bounded
- If the channel has space, the decode thread immediately starts the next image

### Code (Rust)

```rust
// Unbounded work queue (all paths sent instantly, no blocking)
let (work_tx, work_rx) = mpsc::channel::<Option<PathBuf>>();

// Bounded decoded queue (backpressure: max 2 decoded images buffered)
let (decoded_tx, decoded_rx) = mpsc::sync_channel::<Option<Mat>>(2);

// Send all paths to the work queue
for p in &image_paths {
    work_tx.send(Some(p.clone())).unwrap();
}
work_tx.send(None).unwrap(); // sentinel to stop the thread

// Spawn the decode worker
thread::spawn(move || {
    while let Ok(Some(path)) = work_rx.recv() {
        let mat = decode_one(&path, frame_size);
        decoded_tx.send(mat).unwrap(); // blocks if 2 images already buffered
    }
});

// Main loop — next image is always pre-decoded
for i in 0..image_paths.len() {
    // ... write hold frames ...
    let next = decoded_rx.recv().unwrap(); // almost always instant
    // ... crossfade ...
}
```

### Why It's Better than a Single Thread

| | Method #3 (1 preload) | Method #5 (2 lookahead) |
|---|---|---|
| Images ahead | 1 | 2 |
| Stall if decode > write | Yes | Rare |
| Memory usage | 1 extra frame | 2 extra frames |
| Per-image time | ~0.6s | ~0.18s |

---

## Final Result

```
original main.rs
    │  #1 pipe to ffmpeg
    │  #2 resize during load
    │  #3 1-image thread preload
    ▼
fast-slider: 12.46s
    │  #4 VideoToolbox hardware encoder
    │  #5 2-image lookahead buffer
    ▼
fast-slider-v2: 4.15s  ◀── fastest practical version
```

**3× speedup** from 12.46s → 4.15s using only the Apple Silicon hardware that was already in the machine.

---

## What's Left (Future Optimization)

**Option #8 — Fully native Apple pipeline**

Replace OpenCV + ffmpeg entirely with Apple's own frameworks via Objective-C FFI from Rust:

| Task | Current (OpenCV/ffmpeg) | Native Apple |
|---|---|---|
| JPEG decode | OpenCV imread (CPU) | ImageIO (hardware) |
| Resize | imgproc::resize (CPU) | vImage (SIMD/hardware) |
| H.264 encode | ffmpeg → VideoToolbox | VideoToolbox direct (no pipe) |

The stdin pipe is currently the bottleneck (~3.6s). Eliminating the pipe entirely by writing
directly to VideoToolbox would push total time to under 1s — but requires Objective-C FFI bindings,
which is a significantly larger project.
