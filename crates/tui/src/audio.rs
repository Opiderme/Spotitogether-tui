//! Sound analysis for the visualizer.
//!
//! Spotify plays the music in its own app and no longer gives audio
//! analysis to new API apps, so, like `cava`, we listen to the system output
//! (PipeWire/PulseAudio "monitor" of the default sink) and compute the
//! spectrum locally. Nothing is recorded or sent anywhere; capture only runs
//! while the visualizer is open.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

pub const BANDS: usize = 32;
const RATE: u32 = 44_100;
const WINDOW: usize = 2048;
const HOP: usize = 512;
const WAVE_POINTS: usize = 256;
const MIN_HZ: f32 = 40.0;
const MAX_HZ: f32 = 16_000.0;
/// Minimum time between two detected beats.
const BEAT_GAP_S: f32 = 0.18;

/// What the visualizer draws from, all levels in 0..=1.
#[derive(Debug, Clone, PartialEq)]
pub struct Analysis {
    /// Spectrum, low to high frequencies (log scale).
    pub bands: [f32; BANDS],
    /// Overall loudness.
    pub level: f32,
    pub bass: f32,
    pub mid: f32,
    pub treble: f32,
    /// Increases on every detected beat (compare with the previous value).
    pub beats: u64,
    /// Recent signal, -1..=1, for oscilloscope-like styles.
    pub wave: Vec<f32>,
}

impl Default for Analysis {
    fn default() -> Self {
        Self { bands: [0.0; BANDS], level: 0.0, bass: 0.0, mid: 0.0, treble: 0.0, beats: 0, wave: vec![0.0; WAVE_POINTS] }
    }
}

impl Analysis {
    /// Stand-in when the system sound cannot be captured: a calm
    /// procedural motion (120 BPM while playing) so the styles still move.
    pub fn simulated(t: f32, playing: bool) -> Self {
        let energy = if playing { 1.0 } else { 0.15 };
        let beat_phase = (t * 2.0).fract(); // 120 BPM
        let kick = if playing { (1.0 - beat_phase).powi(4) } else { 0.0 };
        let mut bands = [0.0; BANDS];
        for (i, b) in bands.iter_mut().enumerate() {
            let x = i as f32 / BANDS as f32;
            let wobble = 0.5 + 0.5 * (t * (1.3 + x * 3.0) + x * 9.0).sin();
            *b = (energy * (0.75 - 0.5 * x) * wobble + kick * (1.0 - x).powi(3) * 0.6).clamp(0.0, 1.0);
        }
        let wave = (0..WAVE_POINTS)
            .map(|i| {
                let x = i as f32 / WAVE_POINTS as f32;
                energy * 0.6 * ((x * 12.0 + t * 6.0).sin() * 0.7 + (x * 31.0 - t * 9.0).sin() * 0.3)
            })
            .collect();
        let avg = |r: std::ops::Range<usize>| bands[r.clone()].iter().sum::<f32>() / r.len() as f32;
        Self {
            level: avg(0..BANDS),
            bass: (avg(0..4) + kick * 0.4).min(1.0),
            mid: avg(8..20),
            treble: avg(20..BANDS),
            beats: if playing { (t * 2.0) as u64 } else { 0 },
            wave,
            bands,
        }
    }
}

/// Turns blocks of samples into an [`Analysis`] (pure, testable).
pub struct Analyzer {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    ring: Vec<f32>,
    /// FFT bin range of each band.
    band_bins: Vec<(usize, usize)>,
    /// Automatic gain: slowly decaying peak of the band values.
    peak: f32,
    bass_avg: f32,
    since_beat: f32,
    beats: u64,
}

impl Default for Analyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer {
    pub fn new() -> Self {
        let fft = FftPlanner::new().plan_fft_forward(WINDOW);
        let window = (0..WINDOW)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (WINDOW - 1) as f32).cos())
            .collect();
        let bin_hz = RATE as f32 / WINDOW as f32;
        let band_bins = (0..BANDS)
            .map(|b| {
                let edge = |k: f32| MIN_HZ * (MAX_HZ / MIN_HZ).powf(k / BANDS as f32);
                let lo = (edge(b as f32) / bin_hz).floor() as usize;
                let hi = ((edge(b as f32 + 1.0) / bin_hz).ceil() as usize).max(lo + 1);
                (lo.max(1), hi.min(WINDOW / 2))
            })
            .collect();
        Self { fft, window, ring: vec![0.0; WINDOW], band_bins, peak: 1e-3, bass_avg: 0.0, since_beat: 1.0, beats: 0 }
    }

    /// Adds mono samples (any length) and analyses the latest window.
    pub fn push(&mut self, samples: &[f32]) -> Analysis {
        let n = samples.len().min(WINDOW);
        self.ring.drain(..n);
        self.ring.extend_from_slice(&samples[samples.len() - n..]);
        self.since_beat += samples.len() as f32 / RATE as f32;

        let mut buf: Vec<Complex<f32>> =
            self.ring.iter().zip(&self.window).map(|(s, w)| Complex { re: s * w, im: 0.0 }).collect();
        self.fft.process(&mut buf);

        let mut bands = [0.0f32; BANDS];
        for (b, &(lo, hi)) in self.band_bins.iter().enumerate() {
            let sum: f32 = buf[lo..hi].iter().map(|c| c.norm()).sum();
            // Compress the dynamics, and tilt up the treble (less energy).
            let tilt = 1.0 + b as f32 / BANDS as f32 * 2.0;
            bands[b] = (1.0 + sum / (hi - lo) as f32 * tilt).ln();
        }
        // Automatic gain: quiet and loud tracks both fill the screen.
        let max = bands.iter().cloned().fold(0.0, f32::max);
        self.peak = (self.peak * 0.995).max(max).max(1e-3);
        for b in &mut bands {
            *b = (*b / self.peak).clamp(0.0, 1.0);
        }

        let avg = |r: std::ops::Range<usize>| bands[r.clone()].iter().sum::<f32>() / r.len() as f32;
        let bass = avg(0..4);
        // Beat: bass clearly above its recent average.
        if bass > 0.3 && bass > self.bass_avg * 1.35 && self.since_beat > BEAT_GAP_S {
            self.beats += 1;
            self.since_beat = 0.0;
        }
        self.bass_avg = self.bass_avg * 0.9 + bass * 0.1;

        let recent = &self.ring[WINDOW - HOP..];
        let rms = (recent.iter().map(|s| s * s).sum::<f32>() / recent.len() as f32).sqrt();
        let step = WINDOW as f32 / WAVE_POINTS as f32;
        let wave_peak = self.ring.iter().fold(1e-3f32, |m, s| m.max(s.abs()));
        let wave = (0..WAVE_POINTS).map(|i| self.ring[(i as f32 * step) as usize] / wave_peak).collect();

        Analysis {
            bands,
            level: (rms * 4.0).min(1.0).max(avg(0..BANDS) * 0.8),
            bass,
            mid: avg(8..20),
            treble: avg(20..BANDS),
            beats: self.beats,
            wave,
        }
    }
}

/// Background capture of the default output's monitor.
pub struct AudioCapture {
    latest: Arc<Mutex<(Analysis, Instant)>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioCapture {
    pub fn start() -> Result<Self, String> {
        use libpulse_binding::def::BufferAttr;
        use libpulse_binding::sample::{Format, Spec};
        use libpulse_binding::stream::Direction;
        use libpulse_simple_binding::Simple;

        let spec = Spec { format: Format::F32le, channels: 1, rate: RATE };
        // Small fragments: low latency between the sound and the picture.
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: (HOP * 4) as u32,
        };
        let simple = Simple::new(
            None,
            "Spotitogether",
            Direction::Record,
            Some("@DEFAULT_MONITOR@"),
            "visualiseur",
            &spec,
            None,
            Some(&attr),
        )
        .map_err(|e| e.to_string().unwrap_or_else(|| "PulseAudio indisponible".into()))?;

        let latest = Arc::new(Mutex::new((Analysis::default(), Instant::now())));
        let stop = Arc::new(AtomicBool::new(false));
        let (shared, stopping) = (latest.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("audio".into())
            .spawn(move || {
                let mut analyzer = Analyzer::new();
                let mut bytes = vec![0u8; HOP * 4];
                while !stopping.load(Ordering::Relaxed) {
                    if simple.read(&mut bytes).is_err() {
                        break;
                    }
                    let samples: Vec<f32> =
                        bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
                    let analysis = analyzer.push(&samples);
                    if let Ok(mut l) = shared.lock() {
                        *l = (analysis, Instant::now());
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { latest, stop, thread: Some(thread) })
    }

    /// Latest analysis, or `None` when the capture stalled (no sound server
    /// data for a while).
    pub fn latest(&self) -> Option<Analysis> {
        let l = self.latest.lock().ok()?;
        (l.1.elapsed().as_millis() < 500).then(|| l.0.clone())
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // The thread wakes up at the next fragment (~12 ms) and exits.
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(hz: f32, n: usize, offset: usize) -> Vec<f32> {
        (0..n).map(|i| (2.0 * std::f32::consts::PI * hz * (i + offset) as f32 / RATE as f32).sin() * 0.5).collect()
    }

    fn loudest_band(a: &Analysis) -> usize {
        a.bands.iter().enumerate().max_by(|x, y| x.1.total_cmp(y.1)).unwrap().0
    }

    #[test]
    fn a_low_tone_lights_the_low_bands_and_a_high_tone_the_high_ones() {
        let mut low = Analyzer::new();
        let a = low.push(&sine(80.0, WINDOW, 0));
        assert!(loudest_band(&a) < 6, "80 Hz in band {}", loudest_band(&a));
        assert!(a.bass > a.treble);

        let mut high = Analyzer::new();
        let a = high.push(&sine(8_000.0, WINDOW, 0));
        assert!(loudest_band(&a) > 24, "8 kHz in band {}", loudest_band(&a));
        assert!(a.treble > a.bass);
        assert!(a.wave.iter().any(|w| w.abs() > 0.5));
    }

    #[test]
    fn silence_is_quiet() {
        let mut an = Analyzer::new();
        let a = an.push(&vec![0.0; WINDOW]);
        assert!(a.level < 0.01 && a.beats == 0);
    }

    #[test]
    fn kick_drums_are_counted_as_beats() {
        let mut an = Analyzer::new();
        let mut beats = 0;
        // 2 s of 120 BPM kicks: a 60 Hz burst every 0.5 s, then silence.
        for hop in 0..(2 * RATE as usize / HOP) {
            let t = hop * HOP;
            let in_kick = (t % (RATE as usize / 2)) < 4_000;
            let block = if in_kick { sine(60.0, HOP, t) } else { vec![0.0; HOP] };
            beats = an.push(&block).beats;
        }
        assert!((3..=5).contains(&beats), "{beats} beats for 4 kicks");
    }

    #[test]
    fn simulated_motion_follows_play_state() {
        let playing = Analysis::simulated(1.3, true);
        let paused = Analysis::simulated(1.3, false);
        assert!(playing.level > paused.level);
        assert_eq!(paused.beats, 0);
        assert!(Analysis::simulated(3.1, true).beats > playing.beats);
    }

    /// Real capture of the default output monitor (needs a sound server).
    #[test]
    #[ignore]
    fn captures_the_system_output() {
        let capture = AudioCapture::start().expect("PulseAudio/PipeWire monitor");
        std::thread::sleep(std::time::Duration::from_millis(500));
        let a = capture.latest().expect("fresh analysis");
        println!("level {:.2} bass {:.2} beats {}", a.level, a.bass, a.beats);
        drop(capture); // must stop promptly
    }
}
