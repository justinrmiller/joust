//! Optional video frame extraction through an external `ffmpeg` binary.
//!
//! Decoding H.264/HEVC/VP9/AV1 needs a real codec library, so rather than
//! linking one joust shells out to `ffmpeg` when it is installed (or pointed
//! to by `JOUST_FFMPEG`). Without it, video metadata still works (see
//! [`crate::av`]) and previews fall back to a placeholder.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use image::DynamicImage;

/// Longest a single ffmpeg invocation may run.
const TIMEOUT: Duration = Duration::from_secs(20);

/// Path of a working ffmpeg, if any.
fn binary() -> Option<&'static Path> {
    static FFMPEG: OnceLock<Option<PathBuf>> = OnceLock::new();
    FFMPEG
        .get_or_init(|| {
            candidates(std::env::var_os("JOUST_FFMPEG"), cfg!(target_os = "macos"))
                .into_iter()
                .find(|candidate| {
                    Command::new(candidate)
                        .arg("-version")
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .is_ok_and(|status| status.success())
                })
        })
        .as_deref()
}

/// Where to look for ffmpeg, in order: only `JOUST_FFMPEG` when it is set,
/// otherwise `ffmpeg` on `PATH` and, on macOS, the Homebrew and MacPorts
/// locations, which aren't on the `PATH` of an app launched from Finder.
fn candidates(env: Option<std::ffi::OsString>, macos: bool) -> Vec<PathBuf> {
    if let Some(path) = env {
        return vec![path.into()];
    }
    let mut candidates = vec![PathBuf::from("ffmpeg")];
    if macos {
        candidates.extend(
            ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/opt/local/bin/ffmpeg"]
                .map(PathBuf::from),
        );
    }
    candidates
}

/// Whether video frames can be extracted.
pub fn available() -> bool {
    binary().is_some()
}

/// Runs ffmpeg with `args`, returning stdout on success (or `None` on
/// failure / timeout).
fn run(args: &[&str]) -> Option<Vec<u8>> {
    let mut child = Command::new(binary()?)
        .args(["-nostdin", "-hide_banner", "-loglevel", "error"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    // Drain stdout on a thread so a full pipe can't stall ffmpeg.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        stdout.read_to_end(&mut out).map(|_| out)
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < TIMEOUT => {
                std::thread::sleep(Duration::from_millis(15))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let out = reader.join().ok()?.ok()?;
    (status.success() && !out.is_empty()).then_some(out)
}

/// Decodes the frame at `seconds` from the video file at `path`, scaled to
/// fit within `max_side` × `max_side`.
pub fn frame_at(path: &Path, seconds: f64, max_side: u32) -> Option<DynamicImage> {
    let seek = format!("{:.3}", seconds.max(0.0));
    let scale = format!("scale=w={max_side}:h={max_side}:force_original_aspect_ratio=decrease");
    let png = run(&[
        "-ss",
        &seek,
        "-i",
        path.to_str()?,
        "-frames:v",
        "1",
        "-vf",
        &scale,
        "-f",
        "image2pipe",
        "-c:v",
        "png",
        "pipe:1",
    ])?;
    image::load_from_memory(&png).ok()
}

/// Timestamp for a representative poster frame.
pub fn poster_time(duration: Option<f64>) -> f64 {
    duration.map_or(0.0, |d| (d * 0.1).min(1.0))
}

/// `count` evenly spaced timestamps across `duration` (frame centres).
pub fn filmstrip_times(duration: f64, count: usize) -> Vec<f64> {
    (0..count)
        .map(|i| duration * (i as f64 + 0.5) / count as f64)
        .collect()
}

/// Decodes several frames in parallel.
pub fn frames_at(path: &Path, times: &[f64], max_side: u32) -> Vec<Option<DynamicImage>> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = times
            .iter()
            .map(|&t| scope.spawn(move || frame_at(path, t, max_side)))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().ok().flatten())
            .collect()
    })
}

/// A video held in memory, spilled to a temporary file so ffmpeg can seek.
pub struct SpilledVideo {
    file: tempfile::NamedTempFile,
}

impl SpilledVideo {
    pub fn new(bytes: &[u8], extension: &str) -> Option<Self> {
        use std::io::Write;
        let mut file = tempfile::Builder::new()
            .prefix("joust-")
            .suffix(&format!(".{extension}"))
            .tempfile()
            .ok()?;
        file.write_all(bytes).ok()?;
        file.flush().ok()?;
        Some(Self { file })
    }

    pub fn path(&self) -> &Path {
        self.file.path()
    }
}

/// Encodes a short synthetic clip with ffmpeg's `gradients` source, used for
/// the sample database and tests. `colors` are `0xRRGGBB` values.
pub fn synthesize_clip(
    seconds: f64,
    size: (u32, u32),
    colors: [u32; 3],
    seed: u32,
) -> Option<Vec<u8>> {
    let source = format!(
        "gradients=s={}x{}:d={seconds}:r=24:n=3:c0=0x{:06x}:c1=0x{:06x}:c2=0x{:06x}:speed=0.03:seed={seed}",
        size.0, size.1, colors[0], colors[1], colors[2]
    );
    let output = tempfile::Builder::new()
        .prefix("joust-clip-")
        .suffix(".mp4")
        .tempfile()
        .ok()?;
    let output_path = output.path().to_str()?;
    // H.264 when the build has libx264, otherwise ffmpeg's built-in MPEG-4.
    let encoders: [&[&str]; 2] = [
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
        ],
        &["-c:v", "mpeg4", "-q:v", "5"],
    ];
    for encoder in encoders {
        let mut args = vec!["-y", "-f", "lavfi", "-i", &source];
        args.extend_from_slice(encoder);
        args.extend_from_slice(&["-movflags", "+faststart", "-f", "mp4", output_path]);
        if run_to_file(&args) {
            return std::fs::read(output.path()).ok().filter(|b| !b.is_empty());
        }
    }
    None
}

fn run_to_file(args: &[&str]) -> bool {
    let Some(binary) = binary() else { return false };
    let Ok(mut child) = Command::new(binary)
        .args(["-nostdin", "-hide_banner", "-loglevel", "error"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if started.elapsed() < TIMEOUT => {
                std::thread::sleep(Duration::from_millis(15))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_paths() {
        assert_eq!(candidates(Some("/x/ffmpeg".into()), true), [PathBuf::from("/x/ffmpeg")]);
        assert_eq!(candidates(None, false), [PathBuf::from("ffmpeg")]);
        let macos = candidates(None, true);
        assert_eq!(macos[0], PathBuf::from("ffmpeg"));
        assert!(macos.contains(&PathBuf::from("/opt/homebrew/bin/ffmpeg")));
    }

    #[test]
    fn timestamps() {
        assert_eq!(poster_time(None), 0.0);
        assert_eq!(poster_time(Some(4.0)), 0.4);
        assert_eq!(poster_time(Some(600.0)), 1.0);
        assert_eq!(filmstrip_times(8.0, 4), vec![1.0, 3.0, 5.0, 7.0]);
    }

    #[test]
    fn synthesizes_and_decodes_clips_when_ffmpeg_exists() {
        if !available() {
            eprintln!("skipping: ffmpeg not found");
            return;
        }
        let clip = synthesize_clip(2.0, (160, 90), [0x102030, 0xff8800, 0x0088ff], 7).unwrap();
        let info = crate::av::info(&clip).unwrap();
        assert!((info.duration.unwrap() - 2.0).abs() < 0.2);
        assert_eq!((info.width, info.height), (Some(160), Some(90)));

        let spilled = SpilledVideo::new(&clip, "mp4").unwrap();
        let frames = frames_at(spilled.path(), &filmstrip_times(2.0, 3), 64);
        assert_eq!(frames.len(), 3);
        let first = frames[0].as_ref().unwrap();
        assert_eq!((first.width(), first.height()), (64, 36));
    }
}
