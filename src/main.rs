use std::path::PathBuf;
use std::process::Command;

use opencv::{
  Result,
  core::{self, Mat, Size},
  imgcodecs, imgproc,
  prelude::*,
  videoio::VideoWriter,
};

fn main() -> Result<()> {
  // ── Configuration ────────────────────────────────────────────────────────
  let images_dir = "images";        // folder containing input images
  let output_path = "output.mp4";   // output video file
  let fps: f64 = 30.0;              // frames per second
  let seconds_per_image: f64 = 0.4; // how long each image is shown (hold time, excluding fade)
  let seconds_fade: f64 = 0.1;      // crossfade duration in seconds
  // ─────────────────────────────────────────────────────────────────────────

  // Collect image files and sort them by filename
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

  // Read the first image to determine video frame size (preserves original resolution)
  let first = imgcodecs::imread(image_paths[0].to_str().unwrap(), imgcodecs::IMREAD_COLOR)?;
  if first.empty() {
    eprintln!("Could not read first image: {}", image_paths[0].display());
    std::process::exit(1);
  }

  let frame_size = Size::new(first.cols(), first.rows());
  let hold_frames = (fps * seconds_per_image).round() as i32;
  let fade_frames = (fps * seconds_fade).round() as i32;

  // mp4v (MPEG-4 Part 2) — high-quality, widely supported .mp4 codec
  let fourcc = VideoWriter::fourcc('m', 'p', '4', 'v')?;
  let mut writer = VideoWriter::new(output_path, fourcc, fps, frame_size, true)?;

  if !writer.is_opened()? {
    eprintln!("Failed to open VideoWriter. Check the output path and codec support.");
    std::process::exit(1);
  }

  println!(
    "Video: {}x{} @ {} fps | {:.1}s hold + {:.1}s fade | {} images",
    frame_size.width,
    frame_size.height,
    fps,
    seconds_per_image,
    seconds_fade,
    image_paths.len()
  );
  println!();

  // Load and resize a frame to the canonical size
  let load_frame = |path: &PathBuf| -> Result<Mat> {
    let img = imgcodecs::imread(path.to_str().unwrap(), imgcodecs::IMREAD_COLOR)?;
    if img.cols() != frame_size.width || img.rows() != frame_size.height {
      let mut resized = Mat::default();
      imgproc::resize(&img, &mut resized, frame_size, 0.0, 0.0, imgproc::INTER_LANCZOS4)?;
      Ok(resized)
    } else {
      Ok(img)
    }
  };

  let mut current = load_frame(&image_paths[0])?;

  for i in 0..image_paths.len() {
    let path = &image_paths[i];

    if current.empty() {
      eprintln!("  [SKIP] Cannot read: {}", path.display());
      // Try to load next
      if i + 1 < image_paths.len() {
        current = load_frame(&image_paths[i + 1])?;
      }
      continue;
    }

    // Write hold frames for the current image
    for _ in 0..hold_frames {
      writer.write(&current)?;
    }

    // Crossfade into the next image (skip fade after the last image)
    if i + 1 < image_paths.len() {
      let next = load_frame(&image_paths[i + 1])?;
      if !next.empty() {
        for f in 0..fade_frames {
          let alpha = (f as f64 + 1.0) / (fade_frames as f64 + 1.0); // 0 < alpha < 1
          let mut blended = Mat::default();
          core::add_weighted(&current, 1.0 - alpha, &next, alpha, 0.0, &mut blended, -1)?;
          writer.write(&blended)?;
        }
        current = next;
      }
    }

    println!(
      "[{:>width$}/{}] {}",
      i + 1,
      image_paths.len(),
      path.file_name().unwrap().to_str().unwrap(),
      width = image_paths.len().to_string().len()
    );
  }

  println!();
  println!("Saved: {}", output_path);

  // Finalize the file — VideoWriter writes the mp4 moov atom on drop.
  // ffmpeg cannot read the file until this is done.
  drop(writer);

  // Compress the full-resolution video down to 2160p
  compress_to_2160p(output_path, "output_2160p.mp4");

  Ok(())
}

/// Re-encodes `input` as H.264 scaled to 2160p height (width auto, keeps aspect ratio).
/// The output is written to `output`.
fn compress_to_2160p(input: &str, output: &str) {
  println!("Compressing to 2160p → {}", output);

  let status = Command::new("ffmpeg")
    .args([
      "-y",
      "-i", input,
      // Scale so that height = 2160; width rounded to nearest even number.
      // If the video is already ≤2160p tall this is a no-op scale.
      "-vf", "scale=-2:min(ih\\,2160)",
      "-vcodec", "libx264",
      "-crf", "18",          // visually lossless
      "-preset", "slow",     // better compression ratio
      "-pix_fmt", "yuv420p", // broad player compatibility
      "-acodec", "copy",     // keep audio if present
      output,
    ])
    .status()
    .expect("Failed to spawn ffmpeg. Make sure it is installed (brew install ffmpeg).");

  if status.success() {
    println!("2160p video saved: {}", output);
  } else {
    eprintln!("ffmpeg exited with status: {}", status);
  }
}
