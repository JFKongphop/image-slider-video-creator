// boomerang: plays images forward then backward, repeated n times.
// No crossfade — hard cuts between frames.
//
// One boomerang cycle:
//   forward:  [0, 1, 2, ..., N-1]
//   backward: [N-2, N-3, ..., 1]   ← skip endpoints to avoid duplicate frames
//
// Repeat the cycle `loops` times.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Instant;

use opencv::{
  Result,
  core::{Mat, Size},
  imgcodecs, imgproc,
  prelude::*,
};

fn main() -> Result<()> {
  // ── Configuration ────────────────────────────────────────────────────────
  let images_dir   = "images/d";
  let output_path  = "boomerang.mp4";
  let fps: f64     = 30.0;
  let seconds_per_image: f64 = 0.09; // how long each image holds per pass
  let target_height: i32     = 2160;
  let loops: usize           = 2;   // number of boomerang cycles (forward+back = 1)
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

  // Load all images into memory upfront — boomerang needs random access
  // (forward and backward), so a sequential channel won't work here.
  let first_raw = imgcodecs::imread(image_paths[0].to_str().unwrap(), imgcodecs::IMREAD_COLOR)?;
  if first_raw.empty() {
    eprintln!("Could not read first image: {}", image_paths[0].display());
    std::process::exit(1);
  }
  let frame_size = compute_size(first_raw.cols(), first_raw.rows(), target_height);
  let hold_frames = (fps * seconds_per_image).round() as usize;

  println!("Loading {} images...", image_paths.len());
  let load_start = Instant::now();

  let frames: Vec<Mat> = image_paths
    .iter()
    .enumerate()
    .map(|(i, path)| {
      let img = imgcodecs::imread(path.to_str().unwrap(), imgcodecs::IMREAD_COLOR)
        .unwrap_or_default();
      if img.empty() {
        eprintln!("  [SKIP] Cannot read: {}", path.display());
        return Mat::default();
      }
      if img.cols() == frame_size.width && img.rows() == frame_size.height {
        print!("  [{}/{}] {}\r", i + 1, image_paths.len(), path.file_name().unwrap().to_str().unwrap());
        return img;
      }
      let mut resized = Mat::default();
      imgproc::resize(&img, &mut resized, frame_size, 0.0, 0.0, imgproc::INTER_AREA).unwrap();
      print!("  [{}/{}] {}\r", i + 1, image_paths.len(), path.file_name().unwrap().to_str().unwrap());
      resized
    })
    .collect();

  println!("\nLoaded in {:.2}s", load_start.elapsed().as_secs_f64());

  // Build the boomerang index sequence for one cycle:
  //   forward:  [0, 1, 2, ..., N-1]
  //   backward: [N-2, N-3, ..., 1]
  let n = frames.len();
  let mut cycle: Vec<usize> = (0..n).collect();
  if n > 2 {
    cycle.extend((1..n - 1).rev()); // skip first and last to avoid duplicate frames at endpoints
  }

  let use_hwenc = videotoolbox_available();
  let encoder_name = if use_hwenc { "h264_videotoolbox (HW)" } else { "libx264 (SW)" };

  println!();
  println!(
    "Output  : {}x{} @ {} fps | {:.2}s per image | {} images",
    frame_size.width, frame_size.height, fps, seconds_per_image, n
  );
  println!("Encoder : {}", encoder_name);
  println!("Loops   : {} (forward + backward = 1 cycle)", loops);
  println!(
    "Duration: {:.1}s  ({} frames × {} hold frames × {} loops)",
    cycle.len() as f64 * seconds_per_image * loops as f64,
    cycle.len(),
    hold_frames,
    loops
  );
  println!();

  let total_start = Instant::now();
  let mut ffmpeg = spawn_ffmpeg(frame_size, fps, output_path, use_hwenc);
  let stdin = ffmpeg.stdin.as_mut().expect("failed to get ffmpeg stdin");

  for lp in 0..loops {
    print!("Loop {}/{}: ", lp + 1, loops);
    for &idx in &cycle {
      let frame = &frames[idx];
      if frame.empty() {
        continue;
      }
      let data = frame.data_bytes()?;
      for _ in 0..hold_frames {
        stdin.write_all(data).unwrap();
      }
    }
    println!("done");
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
    args.extend([
      "-vcodec".to_string(), "h264_videotoolbox".to_string(),
      "-q:v".to_string(), "85".to_string(),
    ]);
  } else {
    args.extend([
      "-vcodec".to_string(), "libx264".to_string(),
      "-crf".to_string(), "18".to_string(),
      "-preset".to_string(), "medium".to_string(),
    ]);
  }

  args.extend([
    "-pix_fmt".to_string(), "yuv420p".to_string(),
    "-colorspace".to_string(), "bt709".to_string(),
    "-color_primaries".to_string(), "bt709".to_string(),
    "-color_trc".to_string(), "bt709".to_string(),
    "-color_range".to_string(), "tv".to_string(),
    output.to_string(),
  ]);

  Command::new("ffmpeg")
    .args(&args)
    .stdin(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .expect("Failed to spawn ffmpeg. Install it with: brew install ffmpeg")
}
