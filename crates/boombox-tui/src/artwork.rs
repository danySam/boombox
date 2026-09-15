//! Deriving a track's colours from its album art.
//!
//! Spotify puts image URLs in every payload and serves them without
//! authentication, so the smallest thumbnail — 64x64, under 3KB — is enough to
//! read a cover's colour identity for almost no cost.
//!
//! Extraction is deliberately conservative. Measured on real covers, most are
//! far less colourful than they look, and a confident-sounding hue taken from a
//! greyscale or busy cover is worse than no hue at all: the display would
//! change between tracks for no reason a listener could see. When the artwork
//! has nothing to say, the caller keeps the palette derived from the track URI,
//! which never fails.

use std::time::Duration;

use crate::palette::Palette;

/// Longest a cover fetch is allowed to take.
///
/// The thumbnail is under 3KB, so anything slower than this is a network
/// problem rather than a large image, and the URI-derived palette is already
/// on screen and perfectly good.
const FETCH_TIMEOUT: Duration = Duration::from_secs(4);

/// Downscale before reading colours.
///
/// The smallest artwork Spotify serves is already 64x64; this bounds the work
/// if a larger one is ever passed in, and averaging a few pixels together
/// suppresses JPEG ringing around hard edges.
const ANALYSIS_SIZE: u32 = 48;

/// Side of the cover kept for drawing, in pixels.
///
/// Far more than the cell grid can use -- quadrants manage about forty
/// samples across -- but a terminal drawing real pixels wants every one
/// of them, and this is what Spotify serves anyway. Keeping it also means
/// a resize redraws from what we already have rather than fetching again.
const COVER_SIZE: u32 = 300;

/// Pixels below this weight carry no useful colour.
const MIN_WEIGHT: f32 = 0.05;

/// Total colour weight needed before a cover is worth reading at all.
///
/// Guards the greyscale case: a cover that is all blacks, whites and greys has
/// no hue to extract, however confidently a mean over its pixels would report
/// one.
const MIN_COLOUR_SHARE: f32 = 0.06;

/// How tightly the colourful pixels have to agree before a single hue is
/// trusted, as the length of their mean vector on the hue circle.
///
/// Measured on real covers this runs from 0.94 for something strongly
/// single-hued down to 0.32 for a busy multi-coloured sleeve, where the mean
/// lands somewhere essentially arbitrary.
const MIN_CONCENTRATION: f32 = 0.45;

/// Hue gap above which two clusters are treated as a genuine pair rather than
/// as one colour split in two.
const DUOTONE_GAP: f32 = 45.0;

/// How tightly one cluster has to agree before it counts as a real colour
/// rather than a bag of unrelated pixels.
const MIN_CLUSTER_CONCENTRATION: f32 = 0.7;

/// A pixel reduced to what matters for this: hue, and how much it counts.
#[derive(Debug, Clone, Copy)]
struct Sample {
    hue: f32,
    value: f32,
    weight: f32,
}

/// A group of samples that agree on a colour.
#[derive(Debug, Clone, Copy)]
struct Cluster {
    hue: f32,
    /// Mean brightness, which decides which end of the ramp it belongs at.
    value: f32,
    /// Share of the total colour weight.
    share: f32,
    /// How tightly this group agrees, 0 to 1.
    concentration: f32,
}

/// Reads a palette from raw RGB pixels, or `None` if the artwork has no
/// colour worth using.
pub fn palette_from_rgb(pixels: &[[u8; 3]]) -> Option<Palette> {
    let samples = weigh(pixels);
    let total: f32 = samples.iter().map(|s| s.weight).sum();
    let colour_share = total / (pixels.len().max(1) as f32);
    if samples.len() < 16 || colour_share < MIN_COLOUR_SHARE {
        // Nothing but blacks, whites and greys. However confidently a mean
        // over these pixels would report a hue, there is not one.
        return None;
    }

    // Clusters first, deliberately. A cover with two strong colours has low
    // overall concentration by construction — the mean sits between them —
    // so testing that first would reject exactly the covers duotone exists
    // for. Measured on a real sleeve: 0.39 overall, and two tight groups.
    if let Some((dark, light)) = two_clusters(&samples) {
        let gap = arc(dark.hue, light.hue).abs();
        let both_real = dark.concentration >= MIN_CLUSTER_CONCENTRATION
            && light.concentration >= MIN_CLUSTER_CONCENTRATION
            && dark.share >= 0.2
            && light.share >= 0.2;
        if gap >= DUOTONE_GAP && both_real {
            return Some(Palette::duotone(dark.hue, light.hue));
        }
    }

    // Not two colours, so it has to be one — and one only counts if the
    // colourful pixels actually agree on it.
    let (hue, concentration) = circular_mean(&samples)?;
    (concentration >= MIN_CONCENTRATION).then(|| Palette::monotone(hue))
}

/// Keeps the colourful, mid-bright pixels and weights them.
///
/// Blacks, whites and greys dominate most covers by count while carrying no
/// hue at all, so an unweighted average is meaningless. Weighting by
/// saturation and by distance from the extremes of brightness is what makes
/// the result reflect what a person would call the cover's colour.
fn weigh(pixels: &[[u8; 3]]) -> Vec<Sample> {
    pixels
        .iter()
        .filter_map(|rgb| {
            let (hue, saturation, value) = rgb_to_hsv(*rgb);
            let midness = (1.0 - (value - 0.55).abs() / 0.55).clamp(0.0, 1.0);
            let weight = saturation * midness;
            (weight > MIN_WEIGHT).then_some(Sample { hue, value, weight })
        })
        .collect()
}

/// Weighted mean hue, and how concentrated the samples are around it.
///
/// Hue is an angle, so this has to be a vector mean: the arithmetic mean of
/// 350 and 10 is 180, the opposite colour.
fn circular_mean(samples: &[Sample]) -> Option<(f32, f32)> {
    let mut x = 0.0;
    let mut y = 0.0;
    let mut total = 0.0;
    for s in samples {
        let radians = s.hue.to_radians();
        x += s.weight * radians.cos();
        y += s.weight * radians.sin();
        total += s.weight;
    }
    if total <= f32::EPSILON {
        return None;
    }
    let hue = y.atan2(x).to_degrees().rem_euclid(360.0);
    Some((hue, x.hypot(y) / total))
}

/// Splits the samples into two groups by hue and returns them darker first.
///
/// One pass of a two-means on the hue circle, seeded from the extremes. Enough
/// for the question being asked, which is only whether there are two colours
/// or one.
fn two_clusters(samples: &[Sample]) -> Option<(Cluster, Cluster)> {
    let (mean, _) = circular_mean(samples)?;
    // Seed on either side of the mean so the split has somewhere to go.
    let mut centres = (mean - 60.0, mean + 60.0);

    let split = |centres: (f32, f32)| {
        let mut a = Vec::new();
        let mut b = Vec::new();
        for s in samples {
            if arc(s.hue, centres.0).abs() <= arc(s.hue, centres.1).abs() {
                a.push(*s);
            } else {
                b.push(*s);
            }
        }
        (a, b)
    };

    let mut groups = split(centres);
    for _ in 0..12 {
        if groups.0.is_empty() || groups.1.is_empty() {
            return None;
        }
        centres = (circular_mean(&groups.0)?.0, circular_mean(&groups.1)?.0);
        groups = split(centres);
    }
    if groups.0.is_empty() || groups.1.is_empty() {
        return None;
    }

    let total: f32 = samples.iter().map(|s| s.weight).sum();
    let summarise = |members: &[Sample]| -> Option<Cluster> {
        let (hue, concentration) = circular_mean(members)?;
        let weight: f32 = members.iter().map(|s| s.weight).sum();
        let value = members.iter().map(|s| s.value * s.weight).sum::<f32>() / weight.max(1e-6);
        Some(Cluster { hue, value, share: weight / total.max(1e-6), concentration })
    };

    let first = summarise(&groups.0)?;
    let second = summarise(&groups.1)?;
    // Darker cluster at the dark end, so the ramp follows the cover's own
    // shadow-to-highlight logic rather than inverting it.
    Some(if first.value <= second.value { (first, second) } else { (second, first) })
}

/// Signed shortest angle from `from` to `to`, in -180..180.
fn arc(from: f32, to: f32) -> f32 {
    (to - from + 540.0).rem_euclid(360.0) - 180.0
}

fn rgb_to_hsv([r, g, b]: [u8; 3]) -> (f32, f32, f32) {
    let (r, g, b) = (f32::from(r) / 255.0, f32::from(g) / 255.0, f32::from(b) / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;

    let hue = if delta < f32::EPSILON {
        0.0
    } else if max == r {
        60.0 * (((g - b) / delta) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    let saturation = if max > f32::EPSILON { delta / max } else { 0.0 };
    (hue.rem_euclid(360.0), saturation, max)
}

/// Fetches a cover and reads a palette from it.
///
/// Returns `None` for anything the caller should not act on: a failed fetch, an
/// undecodable image, or artwork with no colour worth using. In every case the
/// caller keeps the palette it already had.
/// A square cover, decoded and ready to draw.
#[derive(Debug, Clone)]
pub struct Cover {
    pub size: u32,
    /// `size * size` pixels, row-major.
    pub pixels: Vec<[u8; 3]>,
    /// The file exactly as Spotify served it.
    ///
    /// Kept because iTerm2 takes an image file rather than pixels, so
    /// handing it the original JPEG means no decode, no resample and no
    /// re-encode -- the terminal draws what the CDN sent. A cover is a few
    /// tens of kilobytes, far less than the pixels beside it.
    pub source: Vec<u8>,
}

impl Cover {
    /// A cover with no source file, for tests and for anything that only
    /// needs the pixels.
    #[cfg(test)]
    pub fn from_pixels(size: u32, pixels: Vec<[u8; 3]>) -> Self {
        Self { size, pixels, source: Vec::new() }
    }
}

impl Cover {
    /// Mean of every source pixel inside one destination cell.
    ///
    /// Coordinates are normalised, and the region is half-open so
    /// neighbouring cells do not both claim the pixel on the boundary.
    ///
    /// Averaging rather than picking the nearest pixel, which is what this
    /// did first and was wrong: drawing a 128 pixel cover into about thirty
    /// cells keeps one source pixel in sixteen and throws the rest away, so
    /// whichever pixel happened to sit under the sample point decided the
    /// colour of the whole cell. That is aliasing, and it reads as harsh
    /// rather than merely coarse. The earlier note here claimed blending
    /// would turn covers to mush; that is true of enlarging an image and
    /// not of shrinking one.
    pub fn sample_area(&self, u0: f32, u1: f32, v0: f32, v1: f32) -> [u8; 3] {
        let size = self.size.max(1) as usize;
        let lo = |t: f32| ((t.clamp(0.0, 1.0) * size as f32) as usize).min(size - 1);
        // At least one pixel, however small the region.
        let hi = |t: f32, from: usize| {
            (((t.clamp(0.0, 1.0) * size as f32).ceil() as usize).max(from + 1)).min(size)
        };
        let (x0, y0) = (lo(u0), lo(v0));
        let (x1, y1) = (hi(u1, x0), hi(v1, y0));

        let mut total = [0u32; 3];
        let mut count = 0u32;
        for y in y0..y1 {
            for x in x0..x1 {
                if let Some(p) = self.pixels.get(y * size + x) {
                    total[0] += u32::from(p[0]);
                    total[1] += u32::from(p[1]);
                    total[2] += u32::from(p[2]);
                    count += 1;
                }
            }
        }
        if count == 0 {
            return [0, 0, 0];
        }
        [(total[0] / count) as u8, (total[1] / count) as u8, (total[2] / count) as u8]
    }
}

/// What one fetch of a cover yields. Both come from the same download, so
/// asking for the colours and asking for the picture cost one request.
#[derive(Debug, Clone, Default)]
pub struct Artwork {
    pub palette: Option<Palette>,
    pub cover: Option<Cover>,
}

pub async fn fetch(url: &str) -> Artwork {
    let Some(client) = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .user_agent(concat!("boombox/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()
    else {
        return Artwork::default();
    };

    // No authentication: Spotify serves artwork from a plain CDN, so this
    // needs neither the token nor the daemon.
    let bytes = match client.get(url).send().await {
        Ok(response) if response.status().is_success() => match response.bytes().await {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::debug!("could not read artwork: {e}");
                return Artwork::default();
            }
        },
        Ok(response) => {
            tracing::debug!("artwork {url} returned {}", response.status());
            return Artwork::default();
        }
        Err(e) => {
            tracing::debug!("could not fetch artwork: {e}");
            return Artwork::default();
        }
    };

    let decoded = match image::load_from_memory(&bytes) {
        Ok(image) => image,
        Err(e) => {
            tracing::debug!("could not decode artwork: {e}");
            return Artwork::default();
        }
    };

    let small =
        decoded.resize_exact(ANALYSIS_SIZE, ANALYSIS_SIZE, image::imageops::FilterType::Triangle);
    let pixels: Vec<[u8; 3]> = small.to_rgb8().pixels().map(|p| p.0).collect();

    let palette = palette_from_rgb(&pixels);
    match &palette {
        Some(p) => tracing::info!(
            "artwork palette: hue {:.0}, travel {:.0}",
            p.base_hue(),
            p.travel_degrees()
        ),
        None => tracing::debug!("artwork has no usable colour; keeping the derived palette"),
    }

    tracing::debug!(
        "cover pipeline: fetched {}x{} -> stored {COVER_SIZE}",
        decoded.width(),
        decoded.height()
    );
    let big = decoded.resize_exact(COVER_SIZE, COVER_SIZE, image::imageops::FilterType::Triangle);
    let cover = Cover {
        size: COVER_SIZE,
        pixels: big.to_rgb8().pixels().map(|p| p.0).collect(),
        source: bytes.to_vec(),
    };

    Artwork { palette, cover: Some(cover) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of averaging: a cell covering several source
    /// pixels takes their mean rather than whichever one it landed on.
    #[test]
    fn a_cell_spanning_several_pixels_averages_them() {
        let cover = Cover::from_pixels(2, vec![[100, 0, 0], [200, 0, 0], [0, 0, 0], [100, 0, 0]]);
        // The whole image: (100 + 200 + 0 + 100) / 4 = 100.
        assert_eq!(cover.sample_area(0.0, 1.0, 0.0, 1.0), [100, 0, 0]);
        // Just the top row: (100 + 200) / 2 = 150.
        assert_eq!(cover.sample_area(0.0, 1.0, 0.0, 0.5), [150, 0, 0]);
    }

    /// A region narrower than one pixel still has to yield that pixel
    /// rather than dividing by zero.
    #[test]
    fn a_region_smaller_than_a_pixel_yields_that_pixel() {
        let cover =
            Cover::from_pixels(2, vec![[10, 20, 30], [40, 50, 60], [70, 80, 90], [1, 2, 3]]);
        assert_eq!(cover.sample_area(0.1, 0.1, 0.1, 0.1), [10, 20, 30]);
        assert_eq!(cover.sample_area(0.9, 0.9, 0.9, 0.9), [1, 2, 3]);
    }

    #[test]
    fn sampling_reaches_every_corner_and_clamps_outside() {
        // 2x2: top-left red, top-right green, bottom-left blue, bottom-right white.
        let cover =
            Cover::from_pixels(2, vec![[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255]]);
        let at = |u: f32, v: f32| cover.sample_area(u, u, v, v);
        assert_eq!(at(0.0, 0.0), [255, 0, 0]);
        assert_eq!(at(0.9, 0.0), [0, 255, 0]);
        assert_eq!(at(0.0, 0.9), [0, 0, 255]);
        assert_eq!(at(0.9, 0.9), [255, 255, 255]);
        // Out of range must not panic or wrap to the far side.
        assert_eq!(at(2.0, 2.0), [255, 255, 255]);
        assert_eq!(at(-1.0, -1.0), [255, 0, 0]);
    }

    #[test]
    fn sampling_an_empty_cover_is_not_a_panic() {
        let cover = Cover::from_pixels(0, Vec::new());
        assert_eq!(cover.sample_area(0.0, 1.0, 0.0, 1.0), [0, 0, 0]);
    }

    /// A field of one hue, at a given saturation and brightness.
    fn field(count: usize, hue: f32, saturation: f32, value: f32) -> Vec<[u8; 3]> {
        let c = value * saturation;
        let x = c * (1.0 - ((hue / 60.0) % 2.0 - 1.0).abs());
        let m = value - c;
        let (r, g, b) = match (hue / 60.0) as u32 {
            0 => (c, x, 0.0),
            1 => (x, c, 0.0),
            2 => (0.0, c, x),
            3 => (0.0, x, c),
            4 => (x, 0.0, c),
            _ => (c, 0.0, x),
        };
        vec![[((r + m) * 255.0) as u8, ((g + m) * 255.0) as u8, ((b + m) * 255.0) as u8,]; count]
    }

    #[test]
    fn a_strongly_single_hued_cover_gives_a_monotone_ramp() {
        let p = palette_from_rgb(&field(400, 200.0, 0.7, 0.55)).expect("should read a hue");
        // Within a few degrees of the source: the round trip through HSV and
        // back is not exact at 8 bits.
        assert!((p.base_hue() - 200.0).abs() < 6.0, "got {}", p.base_hue());
    }

    #[test]
    fn a_greyscale_cover_is_declined_rather_than_guessed_at() {
        let greys: Vec<[u8; 3]> = (0..400).map(|i| [(i % 256) as u8; 3]).collect();
        assert!(palette_from_rgb(&greys).is_none());
    }

    #[test]
    fn a_cover_with_a_few_coloured_pixels_is_declined() {
        // Mostly black with a handful of colour: not enough to call it.
        let mut pixels = vec![[8u8, 8, 8]; 400];
        pixels.extend(field(6, 30.0, 0.9, 0.6));
        assert!(palette_from_rgb(&pixels).is_none());
    }

    #[test]
    fn a_two_toned_cover_is_read_even_though_its_overall_hue_is_incoherent() {
        // Regression. Rejecting on low overall concentration before clustering
        // threw away exactly the covers duotone exists for: two strong colours
        // put the circular mean between them, so concentration is low by
        // construction. A real sleeve measured 0.39 overall with two tight
        // groups, and was being declined.
        let mut pixels = field(300, 20.0, 0.75, 0.30);
        pixels.extend(field(300, 160.0, 0.75, 0.65));
        let p = palette_from_rgb(&pixels).expect("two tight clusters should be read");
        assert!(p.travel_degrees().abs() > 100.0, "travel was {}", p.travel_degrees());
    }

    #[test]
    fn a_lopsided_pair_is_not_treated_as_a_duotone() {
        // A wash of one colour with a few pixels of another is one colour.
        let mut pixels = field(600, 200.0, 0.7, 0.5);
        pixels.extend(field(40, 40.0, 0.7, 0.5));
        let p = palette_from_rgb(&pixels).expect("should still read the dominant hue");
        assert!(
            (p.travel_degrees().abs() - 78.0).abs() < 0.01,
            "expected the default travel, got {}",
            p.travel_degrees()
        );
    }

    #[test]
    fn a_busy_cover_is_declined_rather_than_averaged() {
        // Colours spread right round the circle: the mean is arithmetic only.
        let mut pixels = Vec::new();
        for hue in (0..360).step_by(20) {
            pixels.extend(field(30, hue as f32, 0.8, 0.55));
        }
        assert!(palette_from_rgb(&pixels).is_none(), "a rainbow has no dominant hue");
    }

    #[test]
    fn a_genuinely_two_toned_cover_gives_a_duotone_ramp() {
        // Two-toned: red against a darker green, 106 degrees apart.
        let mut pixels = field(300, 2.0, 0.8, 0.62);
        pixels.extend(field(240, 108.0, 0.6, 0.30));
        let p = palette_from_rgb(&pixels).expect("should read a pair");
        // Travel should follow the covers' gap rather than the default.
        assert!(p.travel_degrees().abs() > 60.0, "travel was {}", p.travel_degrees());
    }

    #[test]
    fn two_shades_of_one_colour_stay_monotone() {
        // One colour at two brightnesses: 165 and 192.
        let mut pixels = field(300, 165.0, 0.5, 0.18);
        pixels.extend(field(300, 192.0, 0.52, 0.43));
        let p = palette_from_rgb(&pixels).expect("should read a hue");
        // Widening a 27-degree gap into a duotone would invent a difference
        // the cover does not have.
        assert!(
            (p.travel_degrees().abs() - 78.0).abs() < 0.01,
            "expected the default travel, got {}",
            p.travel_degrees()
        );
    }

    #[test]
    fn the_darker_cluster_lands_at_the_dark_end() {
        let mut pixels = field(300, 20.0, 0.8, 0.25);
        pixels.extend(field(300, 140.0, 0.8, 0.75));
        let p = palette_from_rgb(&pixels).expect("should read a pair");
        assert!((p.base_hue() - 20.0).abs() < 8.0, "dark end was {}", p.base_hue());
    }

    #[test]
    fn no_pixels_is_declined_without_panicking() {
        assert!(palette_from_rgb(&[]).is_none());
    }

    #[test]
    fn hsv_matches_known_colours() {
        let (h, s, v) = rgb_to_hsv([255, 0, 0]);
        assert!(h < 1.0 && (s - 1.0).abs() < 0.01 && (v - 1.0).abs() < 0.01, "{h} {s} {v}");
        let (h, _, _) = rgb_to_hsv([0, 255, 0]);
        assert!((h - 120.0).abs() < 1.0, "{h}");
        let (_, s, _) = rgb_to_hsv([70, 70, 70]);
        assert!(s < 0.01, "grey has no saturation");
    }

    #[test]
    fn arcs_take_the_short_way() {
        assert!((arc(350.0, 10.0) - 20.0).abs() < 0.01);
        assert!((arc(10.0, 350.0) + 20.0).abs() < 0.01);
        assert!(arc(0.0, 180.0).abs() <= 180.0);
    }
}
