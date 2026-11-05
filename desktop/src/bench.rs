//! Frame-time benchmark (`femme-desktop --bench SECONDS`, built with `--features bench`).
//!
//! Frame timings come from gpui's own profiler (`gpui` feature `profiler`, switched on by the `bench`
//! cargo feature so ordinary builds do not carry it), which stamps every
//! drawn frame inside the platform loop: when the compositor's frame signal fired, when the window
//! became dirty, how long `Window::draw` took per phase, and when the frame was handed to the GPU.
//!
//! "Frame time" below is gpui's definition of frame work: the main-thread time from the later of
//! (frame signal, first invalidation) until the frame is submitted. It excludes the vsync wait inside
//! the swapchain present, which is not work. Draw phases, present time and the interval between
//! presented frames are reported next to it so a missed vsync is visible.

use std::fmt::{self, Write as _};
use std::time::Duration;
#[cfg(feature = "bench")]
use std::time::Instant;

#[cfg(feature = "bench")]
use gpui::profiler::{FrameEvent, FrameTiming, FrameTimingCollector};
use hdrhistogram::Histogram;

/// What the run was configured with, echoed in the report.
#[derive(Clone, Debug, Default)]
pub struct BenchConfig {
    /// Measured seconds (after warm-up).
    pub seconds: u64,
    /// Synthetic terminal panes.
    pub panes: usize,
    /// Synthetic source rate.
    pub hz: u32,
    /// What the panes show, one source per pane (`fire,fire,cat`).
    pub mode: String,
    /// The panes fill the window (`--full-height`) instead of sitting in the app's frame.
    pub full_height: bool,
    /// `--target-ms`: the p99 target to judge against, instead of the one the display's refresh implies.
    pub target_ms: Option<f64>,
    /// The shell's transitions ran throughout (`--motion`).
    pub motion: bool,
    /// The window's size in logical pixels and its scale factor, read when measuring starts (a
    /// compositor may override the size that was asked for).
    pub window: Option<(u32, u32, f32)>,
}

/// Main-thread costs the gpui profiler does not break out, summed per frame by the code that incurs
/// them (terminal grid painting, applying frames to the row caches). A frame's total is closed by
/// [`probe::frame`], which `FemmeApp::render` calls once per frame, so each sample is the previous
/// frame's total; over a long run that is the same distribution.
pub mod probe {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
    use std::sync::Mutex;
    use std::time::Instant;

    use hdrhistogram::Histogram;

    static ON: AtomicBool = AtomicBool::new(false);
    static PAINT: AtomicU64 = AtomicU64::new(0);
    static APPLY: AtomicU64 = AtomicU64::new(0);
    static SAMPLES: Mutex<Option<(Histogram<u64>, Histogram<u64>)>> = Mutex::new(None);

    /// Whether probes record (one relaxed load; callers skip their `Instant::now()` when false).
    pub fn enabled() -> bool {
        ON.load(Relaxed)
    }

    /// Starts recording into fresh histograms.
    pub fn start() {
        PAINT.store(0, Relaxed);
        APPLY.store(0, Relaxed);
        *SAMPLES.lock().expect("probe lock") = Some((super::histogram(), super::histogram()));
        ON.store(true, Relaxed);
    }

    /// Stops recording and returns the (paint, apply) per-frame histograms.
    pub fn stop() -> Option<(Histogram<u64>, Histogram<u64>)> {
        ON.store(false, Relaxed);
        SAMPLES.lock().expect("probe lock").take()
    }

    /// Adds the time since `t0` to this frame's terminal-paint total.
    pub fn paint_since(t0: Instant) {
        PAINT.fetch_add(super::nanos(t0.elapsed()), Relaxed);
    }

    /// Adds the time since `t0` to this frame's cache-apply total (frames are applied when a pane
    /// is drawn, so this lands inside the layout phase).
    pub fn apply_since(t0: Instant) {
        APPLY.fetch_add(super::nanos(t0.elapsed()), Relaxed);
    }

    /// Closes the totals gathered since the last call into the histograms.
    pub fn frame() {
        if !enabled() {
            return;
        }
        let (p, a) = (PAINT.swap(0, Relaxed), APPLY.swap(0, Relaxed));
        if let Some((hp, ha)) = SAMPLES.lock().expect("probe lock").as_mut() {
            if p > 0 {
                hp.saturating_record(p);
            }
            if a > 0 {
                ha.saturating_record(a);
            }
        }
    }
}

/// What one terminal pane reports to the report.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PaneStats {
    /// The pane's label (`1 fire`).
    pub label: String,
    /// Grid width in cells, as the pane last applied a frame.
    pub cols: u16,
    /// Grid height in cells.
    pub rows: u16,
    /// Frames applied (while measuring, once it is in a report).
    pub applied: u64,
    /// Times the pane re-opened its stream because a frame did not apply (a gap or a restart).
    pub resyncs: u64,
}

/// Seconds the app runs before measuring starts (first frames build glyph atlases and caches).
pub const WARMUP: Duration = Duration::from_millis(2500);

/// Percentile summary of one histogram of nanosecond samples.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stats {
    /// Sample count.
    pub n: u64,
    /// Median, milliseconds.
    pub p50: f64,
    /// 90th percentile, milliseconds.
    pub p90: f64,
    /// 99th percentile, milliseconds.
    pub p99: f64,
    /// 99.9th percentile, milliseconds.
    pub p999: f64,
    /// Maximum, milliseconds.
    pub max: f64,
}

impl Stats {
    /// Summarises a histogram of nanoseconds.
    pub fn of(h: &Histogram<u64>) -> Self {
        let ms = |v: u64| v as f64 / 1e6;
        Self {
            n: h.len(),
            p50: ms(h.value_at_quantile(0.50)),
            p90: ms(h.value_at_quantile(0.90)),
            p99: ms(h.value_at_quantile(0.99)),
            p999: ms(h.value_at_quantile(0.999)),
            max: ms(h.max()),
        }
    }
}

fn histogram() -> Histogram<u64> {
    // 1 ns .. 60 s, three significant digits.
    Histogram::new_with_bounds(1, 60_000_000_000, 3).expect("static histogram bounds are valid")
}

/// Collects frame events and accumulates the histograms.
#[cfg(feature = "bench")]
pub struct Bench {
    cfg: BenchConfig,
    collector: FrameTimingCollector,
    started: Instant,
    last_draw: Option<FrameTiming>,
    last_present: Option<Instant>,
    refresh: Option<Duration>,
    work: Histogram<u64>,
    draw: Histogram<u64>,
    layout: Histogram<u64>,
    prepaint: Histogram<u64>,
    paint: Histogram<u64>,
    present: Histogram<u64>,
    interval: Histogram<u64>,
    late: u64,
}

#[cfg(feature = "bench")]
impl Bench {
    /// Turns the profiler on and starts collecting from now.
    pub fn start(cfg: BenchConfig) -> Self {
        gpui::profiler::set_trace_enabled(true);
        probe::start();
        Self {
            cfg,
            collector: FrameTimingCollector::new(),
            started: Instant::now(),
            last_draw: None,
            last_present: None,
            refresh: None,
            work: histogram(),
            draw: histogram(),
            layout: histogram(),
            prepaint: histogram(),
            paint: histogram(),
            present: histogram(),
            interval: histogram(),
            late: 0,
        }
    }

    /// Seconds measured so far.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Drains frame events recorded since the last call into the histograms.
    pub fn poll(&mut self) {
        for ev in self.collector.collect_unseen() {
            match ev {
                FrameEvent::Draw(t) => {
                    if t.refresh_interval.is_some() {
                        self.refresh = t.refresh_interval;
                    }
                    self.draw.saturating_record(nanos(t.draw_duration()));
                    self.layout
                        .saturating_record(nanos(t.phases.request_layout));
                    self.prepaint.saturating_record(nanos(t.phases.prepaint));
                    self.paint.saturating_record(nanos(t.phases.paint));
                    self.last_draw = Some(t);
                }
                FrameEvent::Present(p) => {
                    self.present.saturating_record(nanos(p.present_duration()));
                    // gpui only fills `animation_interval` for its own animations, so time the gap myself.
                    if let Some(prev) = self.last_present.replace(p.present_start) {
                        self.interval.saturating_record(nanos(
                            p.present_start.saturating_duration_since(prev),
                        ));
                    }
                    if let Some(d) = self.last_draw.take() {
                        // Frame work as gpui defines it; without a frame signal fall back to the draw start.
                        let work = d.work(p.present_start).unwrap_or_else(|| {
                            p.present_start.saturating_duration_since(d.draw_start)
                        });
                        self.work.saturating_record(nanos(work));
                        if work > d.budget() {
                            self.late += 1;
                        }
                    }
                }
            }
        }
    }

    /// Stops collecting and builds the report; `panes` is what the terminal panes counted since
    /// measuring started.
    pub fn finish(mut self, panes: Vec<PaneStats>) -> Report {
        self.poll();
        gpui::profiler::set_trace_enabled(false);
        let (term_paint, term_apply) =
            probe::stop().map_or_else(Default::default, |(p, a)| (Stats::of(&p), Stats::of(&a)));
        let secs = self.started.elapsed().as_secs_f64();
        Report {
            cfg: self.cfg,
            secs,
            refresh_hz: self.refresh.map(|r| 1.0 / r.as_secs_f64()),
            frames: self.work.len(),
            late: self.late,
            work: Stats::of(&self.work),
            draw: Stats::of(&self.draw),
            layout: Stats::of(&self.layout),
            prepaint: Stats::of(&self.prepaint),
            paint: Stats::of(&self.paint),
            present: Stats::of(&self.present),
            interval: Stats::of(&self.interval),
            term_paint,
            term_apply,
            panes,
            rss_mib: rss_kib().map(|k| k as f64 / 1024.0),
        }
    }
}

fn nanos(d: Duration) -> u64 {
    d.as_nanos().clamp(1, u64::MAX as u128) as u64
}

/// Resident set size of this process in KiB (Linux `/proc`, else `ps`).
pub fn rss_kib() -> Option<u64> {
    if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
        let line = s.lines().find(|l| l.starts_with("VmRSS:"))?;
        return line.split_whitespace().nth(1)?.parse().ok();
    }
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// The frame-time target: `over` (`--target-ms`) if given, else 10 ms p99 on a display at or above
/// 100 Hz, 16 ms below that.
pub fn target_ms(refresh_hz: Option<f64>, over: Option<f64>) -> f64 {
    match (over, refresh_hz) {
        (Some(ms), _) => ms,
        (None, Some(hz)) if hz < 100.0 => 16.0,
        _ => 10.0,
    }
}

/// The finished benchmark numbers.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// The run configuration.
    pub cfg: BenchConfig,
    /// Seconds measured.
    pub secs: f64,
    /// Display refresh rate as reported by the platform, if it reports one.
    pub refresh_hz: Option<f64>,
    /// Frames presented while measuring.
    pub frames: u64,
    /// Frames whose work exceeded one refresh interval.
    pub late: u64,
    /// Frame work (see module docs).
    pub work: Stats,
    /// `Window::draw` as a whole.
    pub draw: Stats,
    /// Render + layout request phase.
    pub layout: Stats,
    /// Prepaint phase.
    pub prepaint: Stats,
    /// Paint phase.
    pub paint: Stats,
    /// Platform submission (includes any vsync wait).
    pub present: Stats,
    /// Gap between consecutive presented frames (frames are only drawn when something changed).
    pub interval: Stats,
    /// Time painting terminal grids, summed over all panes, per frame (inside `paint`).
    pub term_paint: Stats,
    /// Time applying terminal frames to row caches, summed per frame (in the layout phase: a pane
    /// applies what arrived when it is drawn).
    pub term_apply: Stats,
    /// Per terminal pane: grid size, frames applied while measuring, resyncs.
    pub panes: Vec<PaneStats>,
    /// Resident memory at the end, MiB.
    pub rss_mib: Option<f64>,
}

impl Report {
    /// Whether p99 frame work meets the target for this display.
    pub fn pass(&self) -> bool {
        self.work.p99 <= self.target_ms()
    }

    /// The p99 frame-work target this run is judged against.
    pub fn target_ms(&self) -> f64 {
        target_ms(self.refresh_hz, self.cfg.target_ms)
    }

    /// Presented frames per second.
    pub fn fps(&self) -> f64 {
        self.frames as f64 / self.secs.max(f64::EPSILON)
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = &self.cfg;
        let hz = self.refresh_hz.map_or_else(
            || "unknown refresh".to_string(),
            |h| format!("{h:.1} Hz display"),
        );
        let win = c
            .window
            .map_or_else(String::new, |(w, h, s)| format!(", window {w}x{h} at {s}x"));
        let layout = match (c.full_height, c.motion) {
            (true, _) => "full-height panes",
            (false, true) => "panes in the frame, motion on",
            (false, false) => "panes in the frame",
        };
        writeln!(
            f,
            "femme-desktop bench: {} pane(s) {}@{}Hz, {layout}, {:.1}s measured, {hz}{win}",
            c.panes, c.mode, c.hz, self.secs
        )?;
        writeln!(
            f,
            "frames {} ({:.1} fps), late {} ({:.2}%)",
            self.frames,
            self.fps(),
            self.late,
            100.0 * self.late as f64 / self.frames.max(1) as f64
        )?;
        let mut t = String::new();
        writeln!(
            t,
            "{:<18}{:>8}{:>8}{:>8}{:>8}{:>8}   (ms)",
            "", "p50", "p90", "p99", "p99.9", "max"
        )?;
        let row = |t: &mut String, name: &str, s: &Stats| {
            let _ = writeln!(
                t,
                "{name:<18}{:>8.2}{:>8.2}{:>8.2}{:>8.2}{:>8.2}",
                s.p50, s.p90, s.p99, s.p999, s.max
            );
        };
        row(&mut t, "frame work", &self.work);
        row(&mut t, "  draw", &self.draw);
        row(&mut t, "    layout", &self.layout);
        row(&mut t, "    prepaint", &self.prepaint);
        row(&mut t, "    paint", &self.paint);
        row(&mut t, "  present", &self.present);
        row(&mut t, "frame interval", &self.interval);
        row(&mut t, "terminals paint", &self.term_paint);
        row(&mut t, "terminals apply", &self.term_apply);
        f.write_str(&t)?;
        for p in &self.panes {
            writeln!(
                f,
                "pane {:<9} {:>3}x{:<3} cells, {} frames applied ({:.1}/s), {} resync(s)",
                p.label,
                p.cols,
                p.rows,
                p.applied,
                p.applied as f64 / self.secs.max(f64::EPSILON),
                p.resyncs
            )?;
        }
        writeln!(
            f,
            "resyncs {} in total",
            self.panes.iter().map(|p| p.resyncs).sum::<u64>()
        )?;
        if let Some(m) = self.rss_mib {
            writeln!(f, "rss {m:.0} MiB")?;
        }
        write!(
            f,
            "p99 frame work {:.2} ms vs {} ms target{}: {}",
            self.work.p99,
            self.target_ms(),
            if c.target_ms.is_some() {
                " (--target-ms)"
            } else {
                ""
            },
            if self.pass() { "PASS" } else { "FAIL" }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn stats_percentiles() {
        let mut h = histogram();
        for ms in 1..=100u64 {
            h.record(ms * 1_000_000).unwrap();
        }
        let s = Stats::of(&h);
        assert_eq!(s.n, 100);
        assert!((s.p50 - 50.0).abs() < 0.1, "{s:?}");
        assert!((s.p90 - 90.0).abs() < 0.1, "{s:?}");
        assert!((s.p99 - 99.0).abs() < 0.1, "{s:?}");
        assert!((s.max - 100.0).abs() < 0.1, "{s:?}");
    }

    #[test]
    fn target_follows_refresh() {
        assert_eq!(target_ms(Some(165.0), None), 10.0);
        assert_eq!(target_ms(Some(120.0), None), 10.0);
        assert_eq!(target_ms(Some(60.0), None), 16.0);
        assert_eq!(target_ms(None, None), 10.0);
    }

    #[test]
    fn target_override_beats_the_display_refresh() {
        // A 60 Hz panel would judge against 16 ms; `--target-ms 10` holds it to 10 ms.
        assert_eq!(target_ms(Some(60.0), Some(10.0)), 10.0);
        assert_eq!(target_ms(Some(165.0), Some(6.0)), 6.0);
        let report = |over| Report {
            refresh_hz: Some(60.0),
            work: Stats {
                p99: 12.0,
                ..Stats::default()
            },
            cfg: BenchConfig {
                target_ms: over,
                ..BenchConfig::default()
            },
            ..Report::default()
        };
        assert!(
            report(None).pass(),
            "12 ms passes the 16 ms a 60 Hz display implies"
        );
        assert!(!report(Some(10.0)).pass(), "and fails the 10 ms override");
        assert!(report(Some(10.0))
            .to_string()
            .contains("vs 10 ms target (--target-ms): FAIL"));
    }

    #[test]
    fn report_prints_grid_size_applied_frames_and_resyncs_per_pane() {
        let pane = |label: &str, cols, rows, applied, resyncs| PaneStats {
            label: label.into(),
            cols,
            rows,
            applied,
            resyncs,
        };
        let report = Report {
            secs: 20.0,
            panes: vec![
                pane("1 fire", 92, 24, 1200, 0),
                pane("2 cat", 92, 24, 1190, 2),
            ],
            ..Report::default()
        };
        let text = report.to_string();
        let lines: Vec<&str> = text.lines().filter(|l| l.starts_with("pane ")).collect();
        assert_eq!(lines.len(), 2, "{text}");
        for (line, want) in lines.iter().zip(["1 fire", "2 cat"]) {
            assert!(line.contains(want) && line.contains("92x24"), "{line}");
        }
        assert!(
            lines[0].contains("1200 frames applied (60.0/s), 0 resync(s)"),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].contains("1190 frames applied (59.5/s), 2 resync(s)"),
            "{}",
            lines[1]
        );
        assert!(text.contains("resyncs 2 in total"), "{text}");
    }

    #[test]
    fn probe_closes_one_sample_per_frame() {
        probe::start();
        probe::paint_since(Instant::now() - Duration::from_millis(2));
        probe::paint_since(Instant::now() - Duration::from_millis(3));
        probe::frame();
        probe::frame(); // nothing accumulated: no sample
        let (paint, apply) = probe::stop().expect("probe was started");
        assert_eq!(paint.len(), 1);
        assert!(
            paint.max() >= 5_000_000,
            "both paints summed into one frame"
        );
        assert_eq!(apply.len(), 0);
        assert!(!probe::enabled());
    }
}
