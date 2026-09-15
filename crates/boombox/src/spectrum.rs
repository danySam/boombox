//! Turns the audio librespot is decoding into frequency bands for the TUI.
//!
//! The samples are tapped by wrapping librespot's `Sink` (see `streaming.rs`),
//! so nothing in librespot is patched: the decorator copies each packet into a
//! ring buffer on its way to the real audio device.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use rustfft::FftPlanner;
use rustfft::num_complex::Complex32;

/// Window size for the transform. At 44.1 kHz this is ~46 ms, which is short
/// enough to feel responsive and long enough to resolve bass usefully.
pub const FFT_SIZE: usize = 2048;

/// Frequencies below this are mostly rumble, above it mostly hiss; bands are
/// spaced logarithmically between them because pitch is perceived that way.
const MIN_HZ: f32 = 40.0;
const MAX_HZ: f32 = 16_000.0;

/// Anything quieter than this reads as silence.
const FLOOR_DB: f32 = -70.0;

/// Amplitude to a 0.0..=1.0 display value on a decibel curve.
///
/// Linear amplitude is the wrong scale for anything a person looks at, and
/// doubly so here: the tap sits after librespot's volume stage, whose soft
/// mixer is logarithmic. At 50% volume raw peaks land around 0.03, which on a
/// linear scale draws as nothing at all.
pub fn normalise_db(amplitude: f32) -> f32 {
    let db = 20.0 * amplitude.abs().max(1e-10).log10();
    ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0)
}

/// A fixed-size circular buffer of mono samples, written by the audio thread
/// and read by whoever is drawing.
pub struct SpectrumTap {
    ring: Mutex<Ring>,
    sample_rate: f32,
}

struct Ring {
    buf: Vec<f32>,
    pos: usize,
    /// Reset every time the envelope builder reads it.
    peak_since_read: f32,
    /// When samples last arrived, or when the tap was made if none have.
    last_audio: Instant,
}

impl SpectrumTap {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            ring: Mutex::new(Ring {
                buf: vec![0.0; FFT_SIZE],
                pos: 0,
                peak_since_read: 0.0,
                last_audio: Instant::now(),
            }),
            sample_rate: sample_rate as f32,
        }
    }

    /// Downmixes an interleaved packet to mono and appends it.
    ///
    /// Called from librespot's playback thread, so the critical section is
    /// kept to a memcpy -- no allocation, no transform.
    pub fn push_interleaved(&self, samples: &[f64], channels: usize) {
        if channels == 0 {
            return;
        }
        let Ok(mut ring) = self.ring.lock() else {
            return; // A poisoned lock must not take playback down.
        };
        if !samples.is_empty() {
            ring.last_audio = Instant::now();
        }

        for frame in samples.chunks(channels) {
            let sum: f64 = frame.iter().sum();
            let mono = (sum / frame.len() as f64) as f32;
            let pos = ring.pos;
            ring.buf[pos] = mono;
            ring.peak_since_read = ring.peak_since_read.max(mono.abs());
            ring.pos = (pos + 1) % FFT_SIZE;
        }
    }

    /// The most recent `FFT_SIZE` samples, oldest first.
    fn window(&self) -> Option<Vec<f32>> {
        let ring = self.ring.lock().ok()?;
        let mut out = Vec::with_capacity(FFT_SIZE);
        out.extend_from_slice(&ring.buf[ring.pos..]);
        out.extend_from_slice(&ring.buf[..ring.pos]);
        Some(out)
    }

    /// How long since samples last reached the sink, silence included: a
    /// quiet passage still arrives as samples, so only a sink receiving
    /// nothing at all goes quiet here.
    pub fn since_audio(&self) -> Duration {
        self.ring.lock().map(|ring| ring.last_audio.elapsed()).unwrap_or(Duration::MAX)
    }

    /// Loudest sample magnitude since the last call, then resets.
    ///
    /// The envelope builder samples this on a timer; peak-since-last-read is
    /// what it wants, because a track's shape is made of its transients.
    pub fn take_peak(&self) -> f32 {
        let Ok(mut ring) = self.ring.lock() else {
            return 0.0;
        };
        let peak = ring.peak_since_read;
        ring.peak_since_read = 0.0;
        peak
    }

    /// The most recent `points` samples, one per point, in -1.0..=1.0.
    ///
    /// Deliberately not a resampling of the whole ring. The ring holds 46ms,
    /// and squeezing that into a few hundred points aliases everything above a
    /// few hundred Hz into a dense hairball that reads as noise rather than as
    /// a waveform. Taking the most recent samples one-for-one shows a short
    /// window — around 6ms for a 256-point trace — which is a few cycles of
    /// bass and looks like a wave.
    pub fn waveform(&self, points: usize) -> Vec<f32> {
        if points == 0 {
            return Vec::new();
        }
        let Some(window) = self.window() else {
            return vec![0.0; points];
        };
        let start = window.len().saturating_sub(points);
        window[start..].to_vec()
    }

    /// Current band magnitudes, each 0.0..=1.0.
    pub fn bands(&self, count: usize) -> Vec<f32> {
        match self.window() {
            Some(w) => analyse(&w, self.sample_rate, count),
            None => vec![0.0; count],
        }
    }
}

/// Hann window, real FFT, then log-spaced buckets scaled to decibels.
pub fn analyse(samples: &[f32], sample_rate: f32, bands: usize) -> Vec<f32> {
    if samples.is_empty() || bands == 0 {
        return vec![0.0; bands];
    }

    let n = samples.len();
    // Without a window, the discontinuity at the buffer edges smears energy
    // across every bin and the display never settles.
    let mut buffer: Vec<Complex32> = samples
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let t = i as f32 / (n - 1).max(1) as f32;
            let hann = 0.5 - 0.5 * (std::f32::consts::TAU * t).cos();
            Complex32::new(s * hann, 0.0)
        })
        .collect();

    FftPlanner::new().plan_fft_forward(n).process(&mut buffer);

    // Only the first half is meaningful; the rest mirrors it.
    let bin_hz = sample_rate / n as f32;
    let usable = n / 2;

    let mut out = Vec::with_capacity(bands);
    for b in 0..bands {
        let (lo, hi) = band_edges(b, bands);
        let lo_bin = ((lo / bin_hz) as usize).min(usable.saturating_sub(1));
        let hi_bin = ((hi / bin_hz) as usize).clamp(lo_bin + 1, usable);

        // Peak rather than mean: an average across a wide high band washes
        // out exactly the transients that make a visualiser look alive.
        let peak = buffer[lo_bin..hi_bin].iter().map(|c| c.norm()).fold(0.0f32, f32::max);

        // Normalise for window length, then convert to dB.
        out.push(normalise_db(peak / (n as f32 / 4.0)));
    }
    out
}

/// Logarithmic edges for band `b` of `bands`.
fn band_edges(b: usize, bands: usize) -> (f32, f32) {
    let ratio = (MAX_HZ / MIN_HZ).ln();
    let lo = MIN_HZ * (ratio * b as f32 / bands as f32).exp();
    let hi = MIN_HZ * (ratio * (b + 1) as f32 / bands as f32).exp();
    (lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 44_100.0;

    fn sine(hz: f32, len: usize, amplitude: f32) -> Vec<f32> {
        (0..len)
            .map(|i| {
                let t = i as f32 / RATE;
                amplitude * (std::f32::consts::TAU * hz * t).sin()
            })
            .collect()
    }

    /// Which band should contain `hz`.
    fn band_of(hz: f32, bands: usize) -> usize {
        (0..bands).find(|b| band_edges(*b, bands).1 > hz).unwrap_or(bands - 1)
    }

    #[test]
    fn silence_reads_as_zero_everywhere() {
        let bands = analyse(&vec![0.0; FFT_SIZE], RATE, 32);
        assert_eq!(bands.len(), 32);
        assert!(bands.iter().all(|b| *b == 0.0), "{bands:?}");
    }

    #[test]
    fn a_tone_peaks_in_the_band_containing_it() {
        let bands = analyse(&sine(1000.0, FFT_SIZE, 0.8), RATE, 32);
        let expected = band_of(1000.0, 32);
        let loudest = bands
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(i, _)| i)
            .unwrap();
        // Spectral leakage can put the peak in an adjacent band.
        assert!(
            loudest.abs_diff(expected) <= 1,
            "1kHz landed in band {loudest}, expected around {expected}: {bands:?}"
        );
    }

    #[test]
    fn bass_and_treble_land_in_different_places() {
        let low = analyse(&sine(80.0, FFT_SIZE, 0.8), RATE, 32);
        let high = analyse(&sine(8000.0, FFT_SIZE, 0.8), RATE, 32);
        let peak = |v: &[f32]| {
            v.iter().enumerate().max_by(|a, b| a.1.partial_cmp(b.1).unwrap()).unwrap().0
        };
        assert!(peak(&low) < peak(&high), "low={} high={}", peak(&low), peak(&high));
    }

    #[test]
    fn louder_input_produces_a_higher_reading() {
        let quiet = analyse(&sine(1000.0, FFT_SIZE, 0.05), RATE, 32);
        let loud = analyse(&sine(1000.0, FFT_SIZE, 0.9), RATE, 32);
        let b = band_of(1000.0, 32);
        let peak = |v: &[f32], i: usize| {
            v[i.saturating_sub(1)..=(i + 1).min(31)].iter().cloned().fold(0.0f32, f32::max)
        };
        assert!(peak(&loud, b) > peak(&quiet, b), "loud should exceed quiet");
    }

    #[test]
    fn every_value_is_within_the_unit_range() {
        // Deliberately clipping input: the display must not overflow.
        let bands = analyse(&sine(500.0, FFT_SIZE, 12.0), RATE, 64);
        assert!(bands.iter().all(|b| (0.0..=1.0).contains(b)), "{bands:?}");
    }

    #[test]
    fn band_edges_ascend_and_span_the_audible_range() {
        let bands = 64;
        let mut previous = 0.0;
        for b in 0..bands {
            let (lo, hi) = band_edges(b, bands);
            assert!(lo >= previous, "band {b} overlaps the previous one");
            assert!(hi > lo, "band {b} is empty");
            previous = lo;
        }
        assert!((band_edges(0, bands).0 - MIN_HZ).abs() < 0.01);
        assert!((band_edges(bands - 1, bands).1 - MAX_HZ).abs() < 1.0);
    }

    #[test]
    fn degenerate_requests_do_not_panic() {
        assert!(analyse(&[], RATE, 8).iter().all(|b| *b == 0.0));
        assert!(analyse(&sine(1000.0, FFT_SIZE, 0.5), RATE, 0).is_empty());
    }

    #[test]
    fn the_tap_downmixes_stereo_and_wraps_around() {
        let tap = SpectrumTap::new(44_100);
        // Hard-panned left: the mono mix should be half amplitude.
        tap.push_interleaved(&[1.0, 0.0, 1.0, 0.0], 2);
        let w = tap.window().unwrap();
        assert_eq!(w[FFT_SIZE - 1], 0.5);

        // Overrun the ring; it must keep the most recent samples, not panic.
        let long: Vec<f64> = (0..FFT_SIZE * 3).map(|_| 0.25).collect();
        tap.push_interleaved(&long, 1);
        let w = tap.window().unwrap();
        assert_eq!(w.len(), FFT_SIZE);
        assert!(w.iter().all(|s| (*s - 0.25).abs() < 1e-6));
    }

    #[test]
    fn the_waveform_is_the_most_recent_samples_unaltered() {
        let tap = SpectrumTap::new(44_100);
        let samples: Vec<f64> = sine(440.0, FFT_SIZE, 0.9).iter().map(|s| *s as f64).collect();
        tap.push_interleaved(&samples, 1);

        let w = tap.waveform(64);
        assert_eq!(w.len(), 64);
        // One sample per point: the tail of the input, not a summary of it.
        let expected: Vec<f32> = samples[FFT_SIZE - 64..].iter().map(|s| *s as f32).collect();
        for (got, want) in w.iter().zip(&expected) {
            assert!((got - want).abs() < 1e-6, "got {got}, want {want}");
        }
    }

    #[test]
    fn a_short_window_covers_only_a_few_milliseconds() {
        // The point of the change: 256 points is ~6ms at 44.1kHz, a few cycles
        // of bass, rather than the ring's whole 46ms squeezed into 256 points.
        let ms = 256.0 / 44_100.0 * 1000.0;
        assert!(ms < 8.0, "expected a short window, got {ms}ms");
    }

    #[test]
    fn asking_for_more_points_than_the_ring_holds_returns_the_ring() {
        let tap = SpectrumTap::new(44_100);
        tap.push_interleaved(&vec![0.5f64; 64], 1);
        assert_eq!(tap.waveform(FFT_SIZE * 2).len(), FFT_SIZE);
    }

    #[test]
    fn the_running_peak_resets_when_read() {
        let tap = SpectrumTap::new(44_100);
        tap.push_interleaved(&[0.3, -0.7, 0.2], 1);
        assert!((tap.take_peak() - 0.7).abs() < 1e-6, "magnitude, not signed value");
        assert_eq!(tap.take_peak(), 0.0, "a second read sees nothing new");
        tap.push_interleaved(&[0.4], 1);
        assert!((tap.take_peak() - 0.4).abs() < 1e-6);
    }

    #[test]
    fn a_zero_point_waveform_is_empty_rather_than_a_panic() {
        assert!(SpectrumTap::new(44_100).waveform(0).is_empty());
    }

    #[test]
    fn a_mono_stream_is_passed_through_unchanged() {
        let tap = SpectrumTap::new(44_100);
        tap.push_interleaved(&[0.7], 1);
        assert_eq!(tap.window().unwrap()[FFT_SIZE - 1], 0.7);
    }

    #[test]
    fn zero_channels_is_ignored_rather_than_dividing_by_zero() {
        let tap = SpectrumTap::new(44_100);
        tap.push_interleaved(&[1.0, 2.0], 0);
        assert!(tap.window().unwrap().iter().all(|s| *s == 0.0), "nothing should be recorded");
    }
}

/// Peak amplitude across a track, filled in as it plays.
///
/// Spotify will not tell us the shape of a track in advance, so this is the
/// shape of what has actually been heard: the daemon samples the tap on a
/// timer and writes each peak into the bucket for the current position.
pub struct Envelope {
    /// One bucket per column of resolution; f32::NAN means "not played yet".
    buckets: Vec<f32>,
    /// Which track this describes, so a change resets it.
    uri: Option<String>,
}

impl Envelope {
    pub fn new(resolution: usize) -> Self {
        Self { buckets: vec![f32::NAN; resolution.max(1)], uri: None }
    }

    /// Points the envelope at `uri`, clearing it if that is a different
    /// track -- or no track at all, which is what an emptied player reports.
    pub fn follow(&mut self, uri: Option<&str>) {
        if self.uri.as_deref() != uri {
            self.buckets.fill(f32::NAN);
            self.uri = uri.map(str::to_string);
        }
    }

    /// Records `peak` at `progress` through a track of `duration`.
    pub fn record(&mut self, uri: &str, progress_ms: u64, duration_ms: u64, peak: f32) {
        self.follow(Some(uri));
        if duration_ms == 0 {
            return; // A live stream has no shape to fill in.
        }
        let fraction = (progress_ms as f64 / duration_ms as f64).clamp(0.0, 1.0);
        let index = ((fraction * self.buckets.len() as f64) as usize).min(self.buckets.len() - 1);
        let slot = &mut self.buckets[index];
        *slot = if slot.is_nan() { peak } else { slot.max(peak) };
    }

    /// Buckets resampled to `points` on a decibel curve, with unplayed
    /// positions as 0.0.
    pub fn sample(&self, points: usize) -> Vec<f32> {
        if points == 0 {
            return Vec::new();
        }
        (0..points)
            .map(|i| {
                let lo = i * self.buckets.len() / points;
                let hi =
                    ((i + 1) * self.buckets.len() / points).max(lo + 1).min(self.buckets.len());
                let peak = self.buckets[lo..hi]
                    .iter()
                    .filter(|v| !v.is_nan())
                    .cloned()
                    .fold(0.0f32, f32::max);
                // Unplayed stays exactly zero; anything heard gets the curve.
                if peak > 0.0 { normalise_db(peak) } else { 0.0 }
            })
            .collect()
    }
}

#[cfg(test)]
mod envelope_tests {
    use super::*;

    #[test]
    fn peaks_land_in_the_bucket_for_their_position() {
        let mut env = Envelope::new(10);
        env.record("t", 0, 1000, 0.5);
        env.record("t", 900, 1000, 0.9);
        let s = env.sample(10);
        assert!((s[0] - normalise_db(0.5)).abs() < 1e-6, "start: {s:?}");
        assert!((s[9] - normalise_db(0.9)).abs() < 1e-6, "end: {s:?}");
        assert_eq!(s[5], 0.0, "the middle was never played");
    }

    #[test]
    fn quiet_but_audible_playback_is_still_visible() {
        // Regression: the tap sits after a logarithmic volume stage, so at
        // half volume real peaks are around 0.03. On a linear scale the seek
        // bar drew as entirely blank.
        let mut env = Envelope::new(4);
        env.record("t", 0, 100, 0.03);
        let v = env.sample(4)[0];
        assert!(v > 0.4, "0.03 amplitude should be clearly visible, got {v}");
    }

    #[test]
    fn a_bucket_keeps_the_loudest_peak_it_saw() {
        let mut env = Envelope::new(4);
        env.record("t", 0, 100, 0.3);
        env.record("t", 10, 100, 0.8);
        env.record("t", 20, 100, 0.1);
        assert!((env.sample(4)[0] - normalise_db(0.8)).abs() < 1e-6);
    }

    #[test]
    fn changing_track_clears_the_shape() {
        let mut env = Envelope::new(4);
        env.record("a", 0, 100, 0.9);
        env.record("b", 90, 100, 0.2);
        let s = env.sample(4);
        assert_eq!(s[0], 0.0, "the old track's peak must not linger: {s:?}");
        assert!((s[3] - normalise_db(0.2)).abs() < 1e-6);
    }

    /// The reported bug. A queue runs out, the player empties, and the last
    /// track's shape stayed under the progress bar against `0:00 / 0:00`.
    #[test]
    fn an_emptied_player_clears_the_shape() {
        let mut env = Envelope::new(4);
        env.record("t", 0, 100, 0.9);
        env.follow(None);
        assert!(env.sample(4).iter().all(|v| *v == 0.0), "{:?}", env.sample(4));
    }

    /// Recording needs sound, so clearing only on record left the previous
    /// track's shape over the first silent moments of every new one.
    #[test]
    fn a_new_track_clears_the_shape_before_anything_is_heard() {
        let mut env = Envelope::new(4);
        env.record("a", 0, 100, 0.9);
        env.follow(Some("b"));
        assert!(env.sample(4).iter().all(|v| *v == 0.0), "{:?}", env.sample(4));
    }

    /// Every poll passes through here, so following the track already
    /// playing must never wipe what has been heard of it.
    #[test]
    fn following_the_same_track_keeps_its_shape() {
        let mut env = Envelope::new(4);
        env.record("t", 0, 100, 0.9);
        env.follow(Some("t"));
        assert!((env.sample(4)[0] - normalise_db(0.9)).abs() < 1e-6);
    }

    #[test]
    fn the_end_of_a_track_does_not_run_off_the_end_of_the_buckets() {
        let mut env = Envelope::new(4);
        env.record("t", 100, 100, 0.7);
        env.record("t", 500, 100, 0.8); // past the end, e.g. a stale progress read
        assert!((env.sample(4)[3] - normalise_db(0.8)).abs() < 1e-6);
    }

    #[test]
    fn a_stream_with_no_duration_is_ignored_rather_than_dividing_by_zero() {
        let mut env = Envelope::new(4);
        env.record("t", 5000, 0, 0.9);
        assert!(env.sample(4).iter().all(|v| *v == 0.0));
    }

    #[test]
    fn resampling_never_panics_at_awkward_sizes() {
        let mut env = Envelope::new(3);
        env.record("t", 0, 100, 1.0);
        assert_eq!(env.sample(100).len(), 100, "upsampling");
        assert_eq!(env.sample(1).len(), 1, "downsampling");
        assert!(env.sample(0).is_empty());
    }
}
