// fast-slider-v2: adds two optimizations over fast-slider
//   1. Apple Silicon VideoToolbox hardware H.264 encoder (falls back to libx264)
//   2. 2-image lookahead — decode i+1 AND i+2 in parallel so the next image is
//      always ready before crossfade, with no stall even on large files.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, SyncSender};
use std::thread;
use std::time::Instant;

use opencv::{
  Result,
  core::{self, Mat, Size},
  imgcodecs, imgproc,
  prelude::*,
};

fn main() -> Result<()> {
  // ── Configuration ────────────────────────────────────────────────────────
  let images_dir = "images/bw";
  let output_path = "bw.mp4";
  let fps: f64 = 30.0;
  let seconds_per_image: f64 = 1.1;
  let seconds_fade: f64 = 0.1;
  let target_height: i32 = 2160;
  // ─────────────────────────────────────────────────────────────────────────

  let mut image_paths: Vec<PathBuf> = std::fs::read_dir(images_dir)
    .unwrap_or_else(|e| panic!("Cannot open '{}': {}", images_dir, e))
    .filter_map(|entry| {
      let path = entry.ok()?.path();
      let ext = path.extension()?.to_str()?.to_lowercase();
      matches!(
        ext.as_str(),
        "jpg" | "jpeg" | "png" | "bmp" | "tiff" | "tif" | "webp"
      )
      .then_some(path)
    })
    .collect();

  image_paths.sort();

  if image_paths.is_empty() {
    eprintln!("No images found in '{}'", images_dir);
    std::process::exit(1);
  }

  let first_raw =
    imgcodecs::imread(image_paths[0].to_str().unwrap(), imgcodecs::IMREAD_COLOR)?;
  if first_raw.empty() {
    eprintln!("Could not read first image: {}", image_paths[0].display());
    std::process::exit(1);
  }
  let frame_size = compute_size(first_raw.cols(), first_raw.rows(), target_height);
  let hold_frames = (fps * seconds_per_image).round() as i32;
  let fade_frames = (fps * seconds_fade).round() as i32;

  // Detect whether VideoToolbox is available on this machine
  let use_hwenc = videotoolbox_available();
  let encoder_name = if use_hwenc { "h264_videotoolbox (HW)" } else { "libx264 (SW)" };

  println!(
    "Output: {}x{} @ {} fps | {:.1}s hold + {:.1}s fade | {} images",
    frame_size.width, frame_size.height, fps,
    seconds_per_image, seconds_fade, image_paths.len()
  );
  println!("Encoder : {}", encoder_name);
  println!("Lookahead: 2 images");
  println!("Encoding directly via ffmpeg pipe (single pass, no intermediate file)...");
  println!();

  let total_start = Instant::now();

  let mut ffmpeg = spawn_ffmpeg(frame_size, fps, output_path, use_hwenc);
  let ffmpeg_stdin = ffmpeg.stdin.as_mut().expect("failed to get ffmpeg stdin");

  // ── 2-image lookahead pool ────────────────────────────────────────────────
  // We pre-decode up to 2 images ahead using a bounded channel.
  // The decoder thread continuously pulls paths from a work queue and sends
  // decoded Mats back through `decoded_rx`.
  //
  //   main thread:  sends paths → work_tx
  //   decode thread: reads paths ← work_rx, decodes, sends Mat → decoded_tx
  //   main thread:  receives Mat ← decoded_rx
  //
  // Channel bound = 2 keeps at most 2 decoded images in memory at once.

  // Unbounded work queue — main thread sends all paths instantly without blocking.
  // Backpressure comes from decoded_rx (bound=2): the decode thread stalls when
  // 2 images are already waiting, keeping memory bounded.
  let (work_tx, work_rx) = mpsc::channel::<Option<PathBuf>>();
  let (decoded_tx, decoded_rx) = mpsc::sync_channel::<Option<Mat>>(2);

  // Seed the work queue with ALL paths (None = sentinel to stop)
  for p in &image_paths {
    work_tx.send(Some(p.clone())).unwrap();
  }
  work_tx.send(None).unwrap(); // sentinel

  // Spawn decode thread
  thread::spawn(move || {
    decode_worker(work_rx, decoded_tx, frame_size);
  });

  // Pull first image from the decoded queue
  let mut current = match decoded_rx.recv().unwrap() {
    Some(mat) => mat,
    None => {
      eprintln!("Failed to decode first image");
      std::process::exit(1);
    }
  };

  for i in 0..image_paths.len() {
    let path = &image_paths[i];
    let image_start = Instant::now();

    if current.empty() {
      eprintln!("  [SKIP] Cannot read: {}", path.display());
      current = match decoded_rx.recv().unwrap() {
        Some(mat) => mat,
        None => break,
      };
      continue;
    }

    // Write hold frames while the decode thread works on i+2 in the background
    let hold_data = current.data_bytes()?;
    for _ in 0..hold_frames {
      ffmpeg_stdin.write_all(hold_data).unwrap();
    }

    // The next image should already be decoded (or very close to it)
    if i + 1 < image_paths.len() {
      if let Some(next) = decoded_rx.recv().unwrap() {
        for f in 0..fade_frames {
          let alpha = (f as f64 + 1.0) / (fade_frames as f64 + 1.0);
          let mut blended = Mat::default();
          core::add_weighted(
            &current, 1.0 - alpha,
            &next,    alpha,
            0.0, &mut blended, -1,
          )?;
          ffmpeg_stdin.write_all(blended.data_bytes()?).unwrap();
        }
        current = next;
      }
    }

    println!(
      "[{:>width$}/{}] {}  ({:.2}s)",
      i + 1,
      image_paths.len(),
      path.file_name().unwrap().to_str().unwrap(),
      image_start.elapsed().as_secs_f64(),
      width = image_paths.len().to_string().len()
    );
  }

  drop(ffmpeg.stdin.take());

  println!();
  print!("Waiting for ffmpeg to finish encoding...");
  let ffmpeg_start = Instant::now();
  let status = ffmpeg.wait().expect("ffmpeg did not run");
  if status.success() {
    println!(" done. ({:.2}s)", ffmpeg_start.elapsed().as_secs_f64());
    println!("Saved: {}", output_path);
    println!("Total time: {:.2}s", total_start.elapsed().as_secs_f64());
  } else {
    eprintln!("\nffmpeg exited with status: {}", status);
  }

  Ok(())
}

/// Decoder worker: reads PathBufs from `work_rx`, decodes + resizes each image,
/// sends the Mat to `decoded_tx`. A `None` on `work_rx` stops the loop.
fn decode_worker(
  work_rx: mpsc::Receiver<Option<PathBuf>>,
  decoded_tx: SyncSender<Option<Mat>>,
  frame_size: Size,
) {
  while let Ok(msg) = work_rx.recv() {
    let path = match msg {
      Some(p) => p,
      None => break,
    };

    let result = decode_one(&path, frame_size);
    // If the channel is full (main thread is busy writing frames) this will
    // block here — which is exactly the backpressure we want.
    if decoded_tx.send(result).is_err() {
      break; // main thread exited early
    }
  }
}

fn decode_one(path: &PathBuf, frame_size: Size) -> Option<Mat> {
  let img = imgcodecs::imread(path.to_str()?, imgcodecs::IMREAD_COLOR).ok()?;
  if img.empty() {
    return None;
  }
  if img.cols() == frame_size.width && img.rows() == frame_size.height {
    return Some(img);
  }
  let mut out = Mat::default();
  imgproc::resize(&img, &mut out, frame_size, 0.0, 0.0, imgproc::INTER_AREA).ok()?;
  Some(out)
}

/// Returns true if `ffmpeg -encoders` lists h264_videotoolbox.
fn videotoolbox_available() -> bool {
  Command::new("ffmpeg")
    .args(["-hide_banner", "-encoders"])
    .output()
    .map(|o| String::from_utf8_lossy(&o.stdout).contains("h264_videotoolbox"))
    .unwrap_or(false)
}

fn compute_size(w: i32, h: i32, max_h: i32) -> Size {
  if h <= max_h {
    return Size::new(w & !1, h);
  }
  let new_w = (w as f64 * max_h as f64 / h as f64).round() as i32;
  Size::new(new_w & !1, max_h)
}

fn spawn_ffmpeg(frame_size: Size, fps: f64, output: &str, use_hwenc: bool) -> std::process::Child {
  let mut args = vec![
    "-y".to_string(),
    "-f".to_string(), "rawvideo".to_string(),
    "-pixel_format".to_string(), "bgr24".to_string(),
    "-video_size".to_string(), format!("{}x{}", frame_size.width, frame_size.height),
    "-framerate".to_string(), fps.to_string(),
    "-i".to_string(), "pipe:0".to_string(),
  ];

  if use_hwenc {
    // VideoToolbox: hardware H.264 on Apple Silicon / Intel Mac
    // -q:v 85 = near-lossless (range 1–100, higher = better quality)
    args.extend([
      "-vcodec".to_string(), "h264_videotoolbox".to_string(),
      "-q:v".to_string(), "85".to_string(),
    ]);
  } else {
    // Fallback: software libx264
    args.extend([
      "-vcodec".to_string(), "libx264".to_string(),
      "-crf".to_string(), "18".to_string(),
      "-preset".to_string(), "medium".to_string(),
    ]);
  }

  args.extend([
    "-pix_fmt".to_string(), "yuv420p".to_string(),
    output.to_string(),
  ]);

  Command::new("ffmpeg")
    .args(&args)
    .stdin(Stdio::piped())
    .spawn()
    .expect("Failed to spawn ffmpeg. Install it with: brew install ffmpeg")
}
