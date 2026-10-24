//! Timing instrumentation and the scripted UI self-test.
//!
//! With `--perf LOG`, startup, first paint and per-frame UI times are
//! measured and appended to LOG. `--self-test` drives the first tab through
//! vertical scrolling, scrollbar jumps and horizontal scrolling, then exits.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::grid::GridCommand;

/// Phases of the self-test, run in order.
const PHASES: [(&str, usize); 4] = [
    ("vertical scroll (3 rows/frame)", 360),
    ("scrollbar jumps (1 per frame)", 120),
    ("horizontal scroll (60 px/frame)", 360),
    ("go to random rows", 60),
];

pub struct Perf {
    log: Option<PathBuf>,
    started: Instant,
    pub first_frame: Option<Duration>,
    frame_start: Option<Instant>,
    last_frame_end: Option<Instant>,
    samples: Vec<(f32, f32)>,
    self_test: bool,
    phase: usize,
    phase_frames: usize,
    warmup: usize,
    report: Vec<String>,
    rng: u64,
    pub done: bool,
}

impl Perf {
    pub fn new(log: Option<PathBuf>, self_test: bool, started: Instant) -> Self {
        Perf {
            log,
            started,
            first_frame: None,
            frame_start: None,
            last_frame_end: None,
            samples: Vec::new(),
            self_test,
            phase: 0,
            phase_frames: 0,
            warmup: 0,
            report: Vec::new(),
            rng: 0x9E37_79B9_7F4A_7C15,
            done: false,
        }
    }

    pub fn since_start(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn begin_frame(&mut self) {
        self.frame_start = Some(Instant::now());
        if self.first_frame.is_none() {
            self.first_frame = Some(self.started.elapsed());
        }
    }

    /// Records the UI time of this frame (and the interval since the last one).
    pub fn end_frame(&mut self) {
        let now = Instant::now();
        let Some(s) = self.frame_start.take() else {
            return;
        };
        let ui_ms = (now - s).as_secs_f32() * 1000.0;
        let interval = self
            .last_frame_end
            .map_or(0.0, |l| (now - l).as_secs_f32() * 1000.0);
        self.last_frame_end = Some(now);
        if self.self_test && self.warmup >= 10 && self.phase < PHASES.len() {
            self.samples.push((ui_ms, interval));
        }
    }

    pub fn note(&mut self, line: String) {
        if self.log.is_some() {
            self.report.push(line);
        }
    }

    /// Next scripted command for the grid, if the self-test is running.
    pub fn self_test_step(&mut self, wide: bool, indexing: bool) -> Option<Vec<GridCommand>> {
        if !self.self_test || self.phase >= PHASES.len() {
            return None;
        }
        if self.warmup < 10 {
            self.warmup += 1;
            return Some(Vec::new());
        }
        let (name, frames) = PHASES[self.phase];
        if self.phase_frames == frames {
            self.finish_phase(name, indexing);
            self.phase += 1;
            self.phase_frames = 0;
            if self.phase == PHASES.len() {
                self.done = true;
                self.flush();
                return None;
            }
            return Some(vec![GridCommand::Top]);
        }
        self.phase_frames += 1;
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        let r = (self.rng % 10_000) as f32 / 10_000.0;
        Some(match self.phase {
            0 => vec![GridCommand::ScrollRows(3)],
            1 => vec![GridCommand::JumpFraction(r)],
            2 => vec![GridCommand::ScrollX(if wide { 60.0 } else { 15.0 })],
            _ => vec![GridCommand::JumpFraction(r * 0.98)],
        })
    }

    fn finish_phase(&mut self, name: &str, indexing: bool) {
        let mut ui: Vec<f32> = self.samples.iter().map(|s| s.0).collect();
        let mut iv: Vec<f32> = self
            .samples
            .iter()
            .map(|s| s.1)
            .filter(|&x| x > 0.0)
            .collect();
        self.samples.clear();
        if ui.is_empty() {
            return;
        }
        let stat = |v: &mut Vec<f32>| -> (f32, f32, f32) {
            v.sort_by(|a, b| a.total_cmp(b));
            let p = |q: f32| v[((v.len() - 1) as f32 * q) as usize];
            (p(0.5), p(0.95), *v.last().unwrap_or(&0.0))
        };
        let (u50, u95, umax) = stat(&mut ui);
        let (i50, i95, imax) = if iv.is_empty() {
            (0.0, 0.0, 0.0)
        } else {
            stat(&mut iv)
        };
        let fps = if i50 > 0.0 { 1000.0 / i50 } else { 0.0 };
        self.report.push(format!(
            "  {name}{}: ui time p50 {u50:.2} ms, p95 {u95:.2} ms, max {umax:.2} ms | frame interval p50 {i50:.1} ms ({fps:.0} fps), p95 {i95:.1} ms, max {imax:.1} ms",
            if indexing { " [indexing running]" } else { "" }
        ));
    }

    pub fn flush(&mut self) {
        let Some(path) = &self.log else { return };
        if self.report.is_empty() {
            return;
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            for line in self.report.drain(..) {
                let _ = writeln!(f, "{line}");
            }
        }
    }
}

/// Writes an RGBA image as an uncompressed PNG (used by `--screenshot`).
pub fn write_png(
    path: &std::path::Path,
    width: usize,
    height: usize,
    rgba: &[u8],
) -> std::io::Result<()> {
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let crc = crc32(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }
    let mut raw = Vec::with_capacity((width * 4 + 1) * height);
    for row in rgba.chunks_exact(width * 4).take(height) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    // zlib stream made of stored (uncompressed) deflate blocks.
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65_535).collect();
    for (i, b) in blocks.iter().enumerate() {
        z.push((i + 1 == blocks.len()) as u8);
        z.extend_from_slice(&(b.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(b.len() as u16)).to_le_bytes());
        z.extend_from_slice(b);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    std::fs::write(path, out)
}
