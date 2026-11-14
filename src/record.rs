//! MP4 recording: pipes the received JPEGs into an ffmpeg process (H.264).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use crate::command;
use std::process::{Child, Stdio};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::thread::{self, JoinHandle};

/// `ffmpeg[.exe]` next to our executable (portable bundle), else `ffmpeg` from PATH.
pub fn find_ffmpeg() -> Option<PathBuf> {
    let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    let bundled = std::env::current_exe().ok()?.with_file_name(name);
    if bundled.is_file() {
        return Some(bundled);
    }
    let ok = command(Path::new("ffmpeg")).arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status();
    ok.is_ok_and(|s| s.success()).then(|| PathBuf::from("ffmpeg"))
}

pub struct Recorder {
    frames: SyncSender<Vec<u8>>,
    writer: JoinHandle<()>,
    child: Child,
    pub path: PathBuf,
}

impl Recorder {
    pub fn start(ffmpeg: &Path, path: PathBuf) -> std::io::Result<Recorder> {
        let mut child = command(ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            // Timestamps from arrival time: the device frame rate is not constant.
            .args(["-use_wallclock_as_timestamps", "1", "-f", "mjpeg", "-i", "pipe:0"])
            .args(["-c:v", "libx264", "-preset", "superfast", "-tune", "zerolatency", "-crf", "18"])
            // ponytail: no -fps_mode (FFmpeg 5.1+ only) and no -vsync (gone in newer builds): MP4 output is
            // constant-rate, frames are repeated or dropped to keep the wall-clock timing.
            .args(["-pix_fmt", "yuv420p", "-vf", "pad=ceil(iw/2)*2:ceil(ih/2)*2"])
            .args(["-movflags", "+faststart"])
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        // Bounded: if ffmpeg falls behind we drop frames instead of growing memory.
        let (frames, rx) = sync_channel::<Vec<u8>>(30);
        let writer = thread::spawn(move || {
            for jpeg in rx {
                if stdin.write_all(&jpeg).is_err() {
                    return; // ffmpeg died; stop() reports its stderr
                }
            }
        });
        Ok(Recorder { frames, writer, child, path })
    }

    /// Queues a frame; returns false once ffmpeg is gone.
    pub fn push(&self, jpeg: &[u8]) -> bool {
        !matches!(self.frames.try_send(jpeg.to_vec()), Err(TrySendError::Disconnected(_)))
    }

    /// Closes the stream and lets ffmpeg finalise the MP4 in the background.
    pub fn stop(self) -> JoinHandle<Result<PathBuf, String>> {
        let Recorder { frames, writer, mut child, path } = self;
        drop(frames);
        thread::spawn(move || {
            let _ = writer.join(); // drops stdin -> EOF for ffmpeg
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                let _ = e.read_to_string(&mut err);
            }
            match child.wait() {
                Ok(s) if s.success() => Ok(path),
                Ok(s) => Err(format!("ffmpeg {s}: {}", err.trim())),
                Err(e) => Err(e.to_string()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_mp4() {
        let Some(ffmpeg) = find_ffmpeg() else {
            eprintln!("ffmpeg not found: skipping");
            return;
        };
        let path = std::env::temp_dir().join(format!("otoview-test-{}.mp4", std::process::id()));
        let rec = Recorder::start(&ffmpeg, path.clone()).unwrap();
        for i in 0..20 {
            assert!(rec.push(&crate::sim::fake_jpeg(i)));
            thread::sleep(std::time::Duration::from_millis(40));
        }
        let out = rec.stop().join().unwrap().unwrap();
        let bytes = std::fs::read(&out).unwrap();
        let _ = std::fs::remove_file(&out);
        assert_eq!(&bytes[4..8], b"ftyp", "not an MP4");
        assert!(bytes.len() > 2000);
    }
}
