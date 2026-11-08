//! A video player with no library of its own: `ffmpeg` on this machine decodes, the window draws.
//!
//! * Picture: one `ffmpeg` writes raw BGRA frames (what gpui's [`RenderImage`] holds) at a fixed
//!   rate to a pipe; a thread cuts them into frames and hands them over a channel two deep, so
//!   decoding runs only just ahead of playback and memory stays flat at any length. Each frame is
//!   due at its own time (`from + n / fps`); the view takes the newest due one and drops the late.
//! * Sound: a second `ffmpeg` decodes 48 kHz stereo into `pw-play` (`paplay` when there is no
//!   PipeWire). Pause and seek stop both and start them again at the new place.
//! * The clock starts at the first frame, not at the launch, so a slow start skips nothing.
//! * A frame in gpui's sprite atlas stays there until it is dropped: a replaced frame is dropped
//!   once [`RETIRE_AFTER`] newer ones have been drawn (the GPU may still be reading it before
//!   that), as alhazen-gpui does.

use std::collections::VecDeque;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::Instant;

use gpui::RenderImage;

/// The widest a frame is decoded: wider ones are scaled down (a 4K frame is 33 MB).
const MAX_WIDTH: u32 = 1920;

/// The most frames a second shown; faster video is decoded at this rate.
const MAX_FPS: f64 = 60.;

/// How many newer frames are drawn before a replaced one leaves the sprite atlas.
const RETIRE_AFTER: usize = 3;

/// A video file on this machine. A clip made from bytes ([`Clip::from_bytes`]) is a temporary
/// file, removed when the last holder lets go.
pub struct Clip {
    path: PathBuf,
    temporary: bool,
}

impl Clip {
    /// `bytes` of the video called `name`, written to a temporary file.
    pub fn from_bytes(name: &str, bytes: &[u8]) -> std::io::Result<Arc<Self>> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let ext = name.rsplit_once('.').map_or("mp4", |(_, e)| e);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("femme-clip-{}-{n}.{ext}", std::process::id()));
        std::fs::write(&path, bytes)?;
        Ok(Arc::new(Self {
            path,
            temporary: true,
        }))
    }

    /// A still of the video (a PNG, half a second in or the first frame), for a thumbnail.
    pub fn poster(&self) -> Result<Vec<u8>, String> {
        for at in ["0.5", "0"] {
            let out = ffmpeg()
                .args(["-ss", at, "-i"])
                .arg(&self.path)
                .args([
                    "-frames:v",
                    "1",
                    "-f",
                    "image2pipe",
                    "-vcodec",
                    "png",
                    "pipe:1",
                ])
                .output()
                .map_err(needs_ffmpeg)?;
            if out.status.success() && !out.stdout.is_empty() {
                return Ok(out.stdout);
            }
        }
        Err("no picture in it".into())
    }
}

impl Drop for Clip {
    fn drop(&mut self) {
        if self.temporary {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Whether a file is a video the player opens, by its name.
pub fn is_video(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, ext)| {
        matches!(
            ext.to_ascii_lowercase().as_str(),
            "mp4" | "mov" | "webm" | "mkv" | "avi" | "m4v"
        )
    })
}

fn ffmpeg() -> Command {
    let mut c = Command::new("ffmpeg");
    c.args(["-v", "error", "-nostdin"]).stdin(Stdio::null());
    c
}

fn needs_ffmpeg(e: std::io::Error) -> String {
    format!("playing a video needs ffmpeg: {e}")
}

/// What `ffprobe` says of a video.
#[derive(Clone, Debug, PartialEq)]
pub struct Probe {
    /// The size frames are decoded at: the video's, at most [`MAX_WIDTH`] wide, even.
    pub width: u32,
    /// See `width`.
    pub height: u32,
    /// Frames a second, at most [`MAX_FPS`].
    pub fps: f64,
    /// Seconds.
    pub duration: f64,
    /// It has sound.
    pub audio: bool,
}

/// `ffprobe -of json` output, read. `None` when it has no picture.
fn parse_probe(json: &str) -> Option<Probe> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let streams = v["streams"].as_array()?;
    let video = streams.iter().find(|s| s["codec_type"] == "video")?;
    let (w, h) = (
        video["width"].as_u64()? as f64,
        video["height"].as_u64()? as f64,
    );
    let even = |x: f64| ((x / 2.).round() as u32 * 2).max(2);
    let width = even(w.min(MAX_WIDTH as f64));
    let height = even(h * width as f64 / w);
    // "30000/1001"; "0/0" when unknown
    let fps = video["avg_frame_rate"]
        .as_str()
        .and_then(|r| r.split_once('/'))
        .and_then(|(n, d)| Some(n.parse::<f64>().ok()? / d.parse::<f64>().ok()?))
        .filter(|f| f.is_finite() && *f > 0.)
        .unwrap_or(30.)
        .min(MAX_FPS);
    let duration = v["format"]["duration"]
        .as_str()
        .and_then(|d| d.parse().ok())
        .unwrap_or(0.);
    Some(Probe {
        width,
        height,
        fps,
        duration,
        audio: streams.iter().any(|s| s["codec_type"] == "audio"),
    })
}

fn probe(path: &Path) -> Result<Probe, String> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-of", "json", "-show_entries"])
        .args(["stream=codec_type,width,height,avg_frame_rate:format=duration"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(needs_ffmpeg)?;
    parse_probe(&String::from_utf8_lossy(&out.stdout)).ok_or_else(|| "not a video".into())
}

/// A decoded frame and the second it is due.
struct Frame {
    at: f64,
    image: Arc<RenderImage>,
}

/// A running process (or two piped together), stopped when dropped.
struct Running(Vec<Child>);

impl Drop for Running {
    fn drop(&mut self) {
        for c in &mut self.0 {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// The picture from `from` seconds on: frames on `rx` until the end.
fn decode(path: &Path, from: f64, p: &Probe) -> Result<(Running, Receiver<Frame>), String> {
    let mut child = ffmpeg()
        .args(["-ss", &format!("{from:.3}"), "-i"])
        .arg(path)
        .args([
            "-an",
            "-vf",
            &format!("fps={:.3},scale={}:{}", p.fps, p.width, p.height),
            "-pix_fmt",
            "bgra",
            "-f",
            "rawvideo",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(needs_ffmpeg)?;
    let mut out = child.stdout.take().expect("piped");
    let (tx, rx) = mpsc::sync_channel(2);
    let (w, h, fps) = (p.width, p.height, p.fps);
    std::thread::spawn(move || {
        for n in 0u64.. {
            let mut bgra = vec![0; (w * h * 4) as usize];
            if out.read_exact(&mut bgra).is_err() {
                return;
            }
            let Some(buffer) = image::RgbaImage::from_raw(w, h, bgra) else {
                return;
            };
            let image = Arc::new(RenderImage::new([image::Frame::new(buffer)]));
            if tx
                .send(Frame {
                    at: from + n as f64 / fps,
                    image,
                })
                .is_err()
            {
                return;
            }
        }
    });
    Ok((Running(vec![child]), rx))
}

/// The sound from `from` seconds on, playing; `None` when there is nothing to play it with.
fn sound(path: &Path, from: f64) -> Option<Running> {
    let on_path = |bin: &str| {
        std::env::var_os("PATH")
            .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
    };
    let (bin, args): (&str, &[&str]) = if on_path("pw-play") {
        (
            "pw-play",
            &[
                "--raw",
                "--format",
                "s16",
                "--rate",
                "48000",
                "--channels",
                "2",
                "-",
            ],
        )
    } else if on_path("paplay") {
        (
            "paplay",
            &["--raw", "--format=s16le", "--rate=48000", "--channels=2"],
        )
    } else {
        return None;
    };
    let mut dec = ffmpeg()
        .args(["-ss", &format!("{from:.3}"), "-i"])
        .arg(path)
        .args(["-vn", "-f", "s16le", "-ac", "2", "-ar", "48000", "pipe:1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let pcm = dec.stdout.take()?;
    match Command::new(bin)
        .args(args)
        .stdin(pcm)
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(sink) => Some(Running(vec![dec, sink])),
        Err(_) => {
            drop(Running(vec![dec]));
            None
        }
    }
}

/// Playing from `from` seconds since `since`; `since` is `None` until the first frame is drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    Playing { from: f64, since: Option<Instant> },
    Paused { at: f64 },
}

/// One open video.
pub struct Player {
    clip: Arc<Clip>,
    probe: Probe,
    state: State,
    muted: bool,
    /// The decoder and its frames; `None` while paused.
    picture: Option<(Running, Receiver<Frame>)>,
    sound: Option<Running>,
    /// The next frame, taken from the channel and not due yet.
    next: Option<Frame>,
    /// The frame on screen, and when it was due.
    shown: Option<(f64, Arc<RenderImage>)>,
    retired: VecDeque<Arc<RenderImage>>,
}

impl Player {
    /// Probes `clip` and starts it playing. Blocking (it runs `ffprobe`): off the main thread.
    pub fn open(clip: Arc<Clip>) -> Result<Self, String> {
        let probe = probe(&clip.path)?;
        let mut p = Self {
            clip,
            probe,
            state: State::Paused { at: 0. },
            muted: false,
            picture: None,
            sound: None,
            next: None,
            shown: None,
            retired: VecDeque::new(),
        };
        p.play_from(0.)?;
        Ok(p)
    }

    /// Length in seconds.
    pub fn duration(&self) -> f64 {
        self.probe.duration
    }

    /// Playing, not paused or at its end.
    pub fn playing(&self) -> bool {
        matches!(self.state, State::Playing { .. })
    }

    /// The sound is off.
    pub fn muted(&self) -> bool {
        self.muted
    }

    /// A decoder is running: frames are on their way (playing, or the still of a paused seek).
    pub fn decoding(&self) -> bool {
        self.picture.is_some()
    }

    /// The video has sound.
    pub fn has_sound(&self) -> bool {
        self.probe.audio
    }

    /// Where it is, in seconds.
    pub fn position(&self) -> f64 {
        match self.state {
            State::Playing { from, since } => {
                from + since.map_or(0., |s| s.elapsed().as_secs_f64())
            }
            State::Paused { at } => at,
        }
        .min(self.probe.duration.max(0.))
    }

    fn play_from(&mut self, at: f64) -> Result<(), String> {
        let at = at.clamp(0., self.probe.duration.max(0.));
        self.picture = Some(decode(&self.clip.path, at, &self.probe)?);
        self.sound = (self.probe.audio && !self.muted)
            .then(|| sound(&self.clip.path, at))
            .flatten();
        self.next = None;
        self.state = State::Playing {
            from: at,
            since: None,
        };
        Ok(())
    }

    /// Play, pause; at the end, from the start.
    pub fn toggle(&mut self) -> Result<(), String> {
        match self.state {
            State::Playing { .. } => {
                self.state = State::Paused {
                    at: self.position(),
                };
                self.picture = None;
                self.sound = None;
                Ok(())
            }
            State::Paused { at } if at >= self.probe.duration - 0.05 => self.play_from(0.),
            State::Paused { at } => self.play_from(at),
        }
    }

    /// To `at` seconds; it keeps playing, or stays paused (showing the frame there).
    pub fn seek(&mut self, at: f64) -> Result<(), String> {
        let playing = self.playing();
        self.play_from(at)?;
        if !playing {
            // one frame for the still, then paused
            self.state = State::Paused {
                at: at.clamp(0., self.probe.duration.max(0.)),
            };
        }
        Ok(())
    }

    /// Sound off, or on again.
    pub fn toggle_mute(&mut self) {
        self.muted = !self.muted;
        self.sound = None;
        if !self.muted && self.playing() {
            self.sound = sound(&self.clip.path, self.position());
        }
    }

    /// The frame to draw now, and frames to drop from the sprite atlas. Ends the video when its
    /// frames ran out.
    pub fn frame(&mut self) -> (Option<Arc<RenderImage>>, Vec<Arc<RenderImage>>) {
        let mut gone = Vec::new();
        let paused_still = matches!(self.state, State::Paused { .. }) && self.picture.is_some();
        loop {
            if self.next.is_none() {
                let Some((_, rx)) = &self.picture else { break };
                match rx.try_recv() {
                    Ok(f) => self.next = Some(f),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.picture = None;
                        self.sound = None;
                        if self.playing() {
                            self.state = State::Paused {
                                at: self.probe.duration,
                            };
                        }
                        break;
                    }
                }
            }
            // the first frame after a start or a seek starts the clock
            if let State::Playing {
                since: since @ None,
                ..
            } = &mut self.state
            {
                *since = Some(Instant::now());
            }
            let due = paused_still || self.next.as_ref().is_some_and(|f| f.at <= self.position());
            if !due {
                break;
            }
            let f = self.next.take().expect("checked");
            if let Some((_, old)) = self.shown.replace((f.at, f.image)) {
                self.retired.push_back(old);
            }
            if paused_still {
                // the still a paused seek asked for: the decoder can go
                self.picture = None;
                break;
            }
        }
        while self.retired.len() > RETIRE_AFTER {
            gone.extend(self.retired.pop_front());
        }
        (self.shown.as_ref().map(|(_, i)| i.clone()), gone)
    }

    /// Every frame it holds, for the sprite atlas to drop when the player closes.
    pub fn images(&mut self) -> Vec<Arc<RenderImage>> {
        self.picture = None;
        self.sound = None;
        let mut all: Vec<_> = self.retired.drain(..).collect();
        all.extend(self.shown.take().map(|(_, i)| i));
        all.extend(self.next.take().map(|f| f.image));
        all
    }
}

/// `m:ss`, or `h:mm:ss` from an hour.
pub fn clock(seconds: f64) -> String {
    let s = seconds.max(0.) as u64;
    match s / 3600 {
        0 => format!("{}:{:02}", s / 60, s % 60),
        h => format!("{h}:{:02}:{:02}", s / 60 % 60, s % 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_probe_reads_size_rate_length_and_sound() {
        let json = r#"{"streams":[{"codec_type":"video","width":3840,"height":2160,"avg_frame_rate":"30000/1001"},{"codec_type":"audio"}],"format":{"duration":"63.5"}}"#;
        let p = parse_probe(json).unwrap();
        assert_eq!(
            (p.width, p.height),
            (1920, 1080),
            "4K is decoded at 1920 wide"
        );
        assert!((p.fps - 29.97).abs() < 0.01);
        assert_eq!(p.duration, 63.5);
        assert!(p.audio);
        let odd = r#"{"streams":[{"codec_type":"video","width":641,"height":361,"avg_frame_rate":"0/0"}],"format":{}}"#;
        let p = parse_probe(odd).unwrap();
        assert_eq!(
            (p.width % 2, p.height % 2),
            (0, 0),
            "rawvideo sizes are even"
        );
        assert_eq!(p.fps, 30., "an unknown rate is 30");
        assert!(!p.audio);
        assert_eq!(parse_probe(r#"{"streams":[{"codec_type":"audio"}]}"#), None);
    }

    #[test]
    fn clock_reads_minutes_and_hours() {
        assert_eq!(clock(0.), "0:00");
        assert_eq!(clock(63.9), "1:03");
        assert_eq!(clock(3725.), "1:02:05");
    }

    #[test]
    fn a_clip_plays_to_its_end_and_frees_its_frames() {
        assert!(is_video("a.MP4") && !is_video("a.png"));
        let dir = std::env::temp_dir().join(format!("femme-video-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("clip.mp4");
        let made = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=160x90:rate=10",
            ])
            .args(["-t", "1", "-pix_fmt", "yuv420p"])
            .arg(&file)
            .status();
        // ponytail: no ffmpeg here, nothing to check
        if !made.is_ok_and(|s| s.success()) {
            return;
        }
        let clip = Clip::from_bytes("clip.mp4", &std::fs::read(&file).unwrap()).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(clip.poster().unwrap().starts_with(b"\x89PNG"));
        let mut p = Player::open(clip.clone()).unwrap();
        assert_eq!((p.probe.width, p.probe.height), (160, 90));
        let (mut drawn, mut dropped) = (0, 0);
        let started = Instant::now();
        while p.playing() && started.elapsed().as_secs() < 10 {
            let (frame, gone) = p.frame();
            drawn += usize::from(frame.is_some());
            dropped += gone.len();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!p.playing(), "it ends by itself");
        assert!(drawn > 0);
        assert!(
            dropped >= 10 - RETIRE_AFTER - 2,
            "replaced frames leave the atlas"
        );
        assert!(p.images().len() <= RETIRE_AFTER + 1);
        let path = clip.path.clone();
        drop((p, clip));
        assert!(!path.exists(), "a temporary clip goes with its last holder");
    }
}
