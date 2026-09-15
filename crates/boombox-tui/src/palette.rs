//! A colour ramp per track, shared by every visualisation.
//!
//! Two things this is for. The visualisations should agree with each other —
//! a level that reads as "loud" in the bars should be the same colour in the
//! waterfall — and each track should look like itself, so the display changes
//! when the music does.
//!
//! The palette is derived from the track's URI rather than chosen, so it is
//! stable: the same song is the same colours every time, on every machine, with
//! no state to store.

use ratatui::style::Color;

/// Minimum hue travel across the ramp, in degrees.
///
/// Album art is usually far less colourful than it looks: measured on real
/// covers, the two dominant clusters sit 11-37 degrees apart and differ mostly
/// in brightness rather than hue. Following that faithfully gives a ramp that
/// is one colour brightening, which is the flat look a rotating ramp exists to
/// avoid. So a narrow pair is widened, keeping whichever direction the cover
/// suggested.
const MIN_TRAVEL: f32 = 40.0;

/// Travel used when there is nothing to derive one from.
const DEFAULT_TRAVEL: f32 = 78.0;

/// Stops for the ramp, as (position, fraction of the hue travel, saturation,
/// target luma).
///
/// The last field is perceived brightness, not HSL lightness, and the
/// difference matters. Lightness is not perceptual: yellow at L=0.3 is
/// visibly brighter than blue at L=0.5, so a ramp that rotates hue while
/// raising lightness dips and rises unpredictably. Since hue rotation is what
/// stops a colour map looking like one colour fading up, the fix is to name
/// the brightness we want and force each colour to it.
///
/// That the sequence rises and never turns back is the whole design
/// constraint: a ramp that brightens and dims as it goes — "jet", the
/// blue-cyan-green-yellow-red one this replaced — makes the eye read bands and
/// boundaries that are not in the data.
const STOPS: [(f32, f32, f32, f32); 5] = [
    (0.00, 0.000, 0.55, 0.05),
    (0.30, 0.154, 0.85, 0.26),
    (0.60, 0.436, 0.95, 0.48),
    (0.85, 0.744, 0.88, 0.67),
    (1.00, 1.000, 0.55, 0.88),
];

/// A track's colour identity: the hue at each end of the ramp.
///
/// A single hue would do for artwork that is essentially one colour, which is
/// most of it, but a genuinely two-toned cover has something to say and the
/// pair lets it. The interpolation between them takes the short way round, so
/// a red-to-green cover passes through orange rather than through blue.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    /// Hue at the dark end, in degrees.
    start: f32,
    /// Signed travel to the bright end. Kept as a delta rather than a second
    /// hue so the direction survives; 350 to 10 is +20, not -340.
    travel: f32,
}

impl Default for Palette {
    fn default() -> Self {
        // Roughly where the old fixed ramp started, so a build with nothing
        // playing looks like it did before.
        Self { start: 280.0, travel: DEFAULT_TRAVEL }
    }
}

impl Palette {
    /// The palette for a track URI. Stable across runs and machines.
    pub fn for_uri(uri: &str) -> Self {
        // FNV-1a rather than DefaultHasher: the standard hasher is explicitly
        // not stable between releases, and a song changing colour after a
        // toolchain upgrade would be a strange bug to chase.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in uri.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Self::monotone((hash % 360) as f32)
    }

    /// A ramp from one hue, rotating forward as it brightens.
    pub fn monotone(hue: f32) -> Self {
        Self { start: hue.rem_euclid(360.0), travel: DEFAULT_TRAVEL }
    }

    /// A ramp between two hues taken from artwork, dark end first.
    ///
    /// Widened to [`MIN_TRAVEL`] when the pair is close together, which is the
    /// common case: most covers are effectively one colour and following them
    /// exactly produces a flat ramp.
    pub fn duotone(dark_hue: f32, light_hue: f32) -> Self {
        let start = dark_hue.rem_euclid(360.0);
        // Short arc: the signed difference in -180..180.
        let direct = (light_hue - dark_hue + 540.0).rem_euclid(360.0) - 180.0;
        let travel = if direct.abs() >= MIN_TRAVEL {
            direct
        } else if direct < 0.0 {
            -MIN_TRAVEL
        } else {
            MIN_TRAVEL
        };
        Self { start, travel }
    }

    /// Hue at the dark end, for callers that want a single representative.
    pub fn base_hue(&self) -> f32 {
        self.start
    }

    /// Signed hue travel across the ramp, for tests and diagnostics.
    pub fn travel_degrees(&self) -> f32 {
        self.travel
    }

    /// Colour for a normalised value, 0.0 quiet to 1.0 loud.
    pub fn at(&self, value: f32) -> Color {
        let v = value.clamp(0.0, 1.0);
        let mut segment = 0;
        while segment + 2 < STOPS.len() && v > STOPS[segment + 1].0 {
            segment += 1;
        }
        let (t0, h0, s0, l0) = STOPS[segment];
        let (t1, h1, s1, l1) = STOPS[segment + 1];
        let blend = if (t1 - t0).abs() < f32::EPSILON { 0.0 } else { (v - t0) / (t1 - t0) };

        let fraction = h0 + (h1 - h0) * blend;
        let hue = self.start + self.travel * fraction;
        let (r, g, b) = shade(hue, s0 + (s1 - s0) * blend, l0 + (l1 - l0) * blend);
        Color::Rgb(r, g, b)
    }

    /// The bright end, for a trace that should sit above everything else.
    pub fn highlight(&self) -> Color {
        self.at(1.0)
    }

    /// A dimmed colour for the scope's afterglow, `keep` of full brightness.
    pub fn ghost(&self, keep: f32) -> Color {
        let (r, g, b) = shade(self.start + self.travel * 0.4, 0.85, 0.42 * keep.clamp(0.0, 1.0));
        Color::Rgb(r, g, b)
    }
}

/// A hue at a given saturation, forced to a target perceived brightness.
///
/// Takes the fully-chromatic colour for the hue, then darkens toward black or
/// blends toward white until its luma matches. Doing it this way means the
/// ramp's brightness is exactly what the stops ask for, whatever the hue,
/// rather than whatever HSL happens to produce.
fn shade(hue: f32, saturation: f32, target: f32) -> (u8, u8, u8) {
    let (r, g, b) = hsl_f(hue, saturation, 0.5);
    let base = luma_f(r, g, b);
    let t = target.clamp(0.0, 1.0);

    let (r, g, b) = if t <= base {
        let k = if base > f32::EPSILON { t / base } else { 0.0 };
        (r * k, g * k, b * k)
    } else {
        let k = (t - base) / (1.0 - base).max(f32::EPSILON);
        (r + (1.0 - r) * k, g + (1.0 - g) * k, b + (1.0 - b) * k)
    };
    ((r * 255.0).round() as u8, (g * 255.0).round() as u8, (b * 255.0).round() as u8)
}

/// Rec. 601 luma: how bright the eye finds a colour, not the plain average.
fn luma_f(r: f32, g: f32, b: f32) -> f32 {
    0.299 * r + 0.587 * g + 0.114 * b
}

/// Hue in degrees (wrapping), saturation and lightness in 0..=1.
///
/// Only the tests need the 8-bit form now; the ramp works in floats so it can
/// correct brightness before quantising.
#[cfg(test)]
fn hsl_to_rgb(hue: f32, saturation: f32, lightness: f32) -> (u8, u8, u8) {
    let (r, g, b) = hsl_f(hue, saturation, lightness);
    ((r * 255.0).round() as u8, (g * 255.0).round() as u8, (b * 255.0).round() as u8)
}

/// As [`hsl_to_rgb`], in floats.
fn hsl_f(hue: f32, saturation: f32, lightness: f32) -> (f32, f32, f32) {
    let h = hue.rem_euclid(360.0) / 60.0;
    let s = saturation.clamp(0.0, 1.0);
    let l = lightness.clamp(0.0, 1.0);

    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let m = l - c / 2.0;

    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (r + m, g + m, b + m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luma(colour: Color) -> f32 {
        match colour {
            // Rec. 601 weights: how bright the eye actually finds it, rather
            // than the plain average.
            Color::Rgb(r, g, b) => {
                0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b)
            }
            other => panic!("expected an rgb colour, got {other:?}"),
        }
    }

    #[test]
    fn the_ramp_brightens_all_the_way_up() {
        // The property the old "jet" ramp broke, and the reason for this one.
        let p = Palette::default();
        let mut previous = -1.0;
        for step in 0..=50 {
            let current = luma(p.at(step as f32 / 50.0));
            assert!(current > previous, "dipped at {step}: {current} <= {previous}");
            previous = current;
        }
    }

    #[test]
    fn every_hue_produces_a_ramp_that_brightens() {
        // Not just the default: a track can land on any hue.
        for hue in (0..360).step_by(15) {
            let p = Palette::monotone(hue as f32);
            let mut previous = -1.0;
            for step in 0..=20 {
                let current = luma(p.at(step as f32 / 20.0));
                assert!(current > previous, "hue {hue} dipped at step {step}");
                previous = current;
            }
        }
    }

    #[test]
    fn the_ramp_hits_the_brightness_the_stops_ask_for() {
        // The point of forcing luma rather than trusting HSL lightness.
        for hue in [0.0, 60.0, 200.0, 300.0] {
            let p = Palette::monotone(hue);
            for (position, _, _, target) in STOPS {
                let got = luma(p.at(position)) / 255.0;
                assert!(
                    (got - target).abs() < 0.02,
                    "hue {hue} at {position}: wanted {target}, got {got}"
                );
            }
        }
    }

    #[test]
    fn the_ends_are_dark_and_light() {
        let p = Palette::default();
        assert!(luma(p.at(0.0)) < 40.0, "quiet should be nearly black");
        assert!(luma(p.at(1.0)) > 190.0, "loud should be nearly white");
    }

    #[test]
    fn a_close_hue_pair_is_widened_to_keep_the_ramp_from_going_flat() {
        // A cover whose two colour clusters sit 11 degrees apart.
        let p = Palette::duotone(339.0, 350.0);
        assert!(p.travel.abs() >= MIN_TRAVEL, "travel was {}", p.travel);
        assert!(p.travel > 0.0, "and keeps the direction the cover suggested");
    }

    #[test]
    fn a_wide_hue_pair_is_followed_as_given() {
        // Genuinely two-toned: 106 degrees apart.
        let p = Palette::duotone(2.0, 108.0);
        assert!((p.travel - 106.0).abs() < 0.01, "travel was {}", p.travel);
    }

    #[test]
    fn duotone_takes_the_short_way_round() {
        // 350 -> 10 is +20 the short way, not -340 the long way.
        let p = Palette::duotone(350.0, 10.0);
        assert!(p.travel > 0.0 && p.travel <= 180.0, "travel was {}", p.travel);

        // And the other direction: 10 -> 350 is -20.
        let q = Palette::duotone(10.0, 350.0);
        assert!(q.travel < 0.0 && q.travel >= -180.0, "travel was {}", q.travel);
    }

    #[test]
    fn a_backwards_pair_widens_backwards() {
        let p = Palette::duotone(100.0, 95.0);
        assert!((p.travel + MIN_TRAVEL).abs() < 0.01, "travel was {}", p.travel);
    }

    #[test]
    fn a_duotone_ramp_still_brightens_all_the_way() {
        // The constraint that makes any hue pair safe.
        for (dark, light) in [(2.0, 108.0), (200.0, 40.0), (350.0, 10.0), (60.0, 240.0)] {
            let p = Palette::duotone(dark, light);
            let mut previous = -1.0;
            for step in 0..=30 {
                let current = luma(p.at(step as f32 / 30.0));
                assert!(current > previous, "{dark}->{light} dipped at {step}");
                previous = current;
            }
        }
    }

    #[test]
    fn a_uri_always_gives_the_same_palette() {
        let a = Palette::for_uri("spotify:track:ExampleTrack0000000003");
        let b = Palette::for_uri("spotify:track:ExampleTrack0000000003");
        assert_eq!(a, b, "must be stable, or a song changes colour on replay");
    }

    #[test]
    fn different_tracks_generally_get_different_palettes() {
        let uris: Vec<String> = (0..64).map(|i| format!("spotify:track:{i:022}")).collect();
        let hues: std::collections::HashSet<u32> =
            uris.iter().map(|u| Palette::for_uri(u).base_hue() as u32).collect();
        // Collisions are fine in principle; wholesale clumping is not.
        assert!(hues.len() > 50, "only {} distinct hues from 64 tracks", hues.len());
    }

    #[test]
    fn out_of_range_values_clamp_rather_than_wrap() {
        let p = Palette::default();
        assert_eq!(p.at(-5.0), p.at(0.0));
        assert_eq!(p.at(5.0), p.at(1.0));
    }

    #[test]
    fn hsl_hits_the_primaries() {
        assert_eq!(hsl_to_rgb(0.0, 1.0, 0.5), (255, 0, 0));
        assert_eq!(hsl_to_rgb(120.0, 1.0, 0.5), (0, 255, 0));
        assert_eq!(hsl_to_rgb(240.0, 1.0, 0.5), (0, 0, 255));
    }

    #[test]
    fn hsl_handles_the_achromatic_ends() {
        assert_eq!(hsl_to_rgb(0.0, 0.0, 0.0), (0, 0, 0));
        assert_eq!(hsl_to_rgb(0.0, 0.0, 1.0), (255, 255, 255));
        // Hue is irrelevant with no saturation.
        assert_eq!(hsl_to_rgb(90.0, 0.0, 0.5), hsl_to_rgb(270.0, 0.0, 0.5));
    }

    #[test]
    fn hue_wraps_rather_than_clipping() {
        assert_eq!(hsl_to_rgb(360.0, 1.0, 0.5), hsl_to_rgb(0.0, 1.0, 0.5));
        assert_eq!(hsl_to_rgb(-120.0, 1.0, 0.5), hsl_to_rgb(240.0, 1.0, 0.5));
    }

    #[test]
    fn the_ghost_is_dimmer_than_the_trace() {
        let p = Palette::default();
        assert!(luma(p.ghost(1.0)) < luma(p.highlight()));
        assert!(luma(p.ghost(0.2)) < luma(p.ghost(1.0)), "and fades with age");
    }
}
