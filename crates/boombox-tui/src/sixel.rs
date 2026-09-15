//! Encoding an image as sixel.
//!
//! The oldest of the three terminal image formats and the only one plain
//! xterm or Windows Terminal will draw, which is what makes it worth
//! having: it is the difference between "images on kitty and iTerm2" and
//! "images on Linux and Windows too".
//!
//! It costs colour. Sixel carries a palette of at most 256 registers where
//! the other two protocols take full-colour pixels, so this quantises
//! first. Album art usually survives that well -- covers tend to have a
//! limited palette to begin with.

/// Sixel allows 256 colour registers; a few are left spare because some
/// terminals reserve the top of the range.
const PALETTE: usize = 250;

/// Six rows of pixels share one character, which is where the name comes
/// from: each character carries six vertical bits.
const BAND: usize = 6;

/// One box of colours during the split. `from`/`to` index into the working
/// order, which is permuted rather than copied.
struct Box_ {
    from: usize,
    to: usize,
}

impl Box_ {
    fn len(&self) -> usize {
        self.to - self.from
    }
}

/// Reduces an image to at most [`PALETTE`] colours by median cut.
///
/// Splitting the box with the widest spread, along the channel it is
/// widest in, keeps detail where the picture actually varies -- a uniform
/// colour cube would spend registers on colours the cover never uses.
///
/// Returns the palette and one index per pixel. The indices come out of
/// the split itself rather than a nearest-colour search afterwards, which
/// would be a quarter of a million distance calculations per cover.
fn quantise(pixels: &[[u8; 3]], limit: usize) -> (Vec<[u8; 3]>, Vec<u8>) {
    let mut order: Vec<u32> = (0..pixels.len() as u32).collect();
    let mut boxes = vec![Box_ { from: 0, to: order.len() }];

    while boxes.len() < limit {
        // Split the box with the most pixels still spread over a range;
        // one that is already a single colour cannot be improved.
        let Some(target) = (0..boxes.len())
            .filter(|i| {
                boxes[*i].len() > 1 && spread(pixels, &order[boxes[*i].from..boxes[*i].to]).1 > 0
            })
            .max_by_key(|i| boxes[*i].len())
        else {
            break;
        };

        let (channel, _) = spread(pixels, &order[boxes[target].from..boxes[target].to]);
        let (from, to) = (boxes[target].from, boxes[target].to);
        order[from..to].sort_unstable_by_key(|i| pixels[*i as usize][channel]);
        let mid = from + (to - from) / 2;

        boxes[target].to = mid;
        boxes.push(Box_ { from: mid, to });
    }

    let mut palette = Vec::with_capacity(boxes.len());
    let mut indices = vec![0u8; pixels.len()];
    for (slot, b) in boxes.iter().enumerate() {
        let members = &order[b.from..b.to];
        palette.push(average(pixels, members));
        for &i in members {
            indices[i as usize] = slot as u8;
        }
    }
    (palette, indices)
}

/// The channel a set of pixels varies most in, and by how much.
fn spread(pixels: &[[u8; 3]], members: &[u32]) -> (usize, u8) {
    let mut lo = [255u8; 3];
    let mut hi = [0u8; 3];
    for &i in members {
        let p = pixels[i as usize];
        for c in 0..3 {
            lo[c] = lo[c].min(p[c]);
            hi[c] = hi[c].max(p[c]);
        }
    }
    let mut best = (0usize, 0u8);
    for c in 0..3 {
        let range = hi[c] - lo[c];
        if range >= best.1 {
            best = (c, range);
        }
    }
    best
}

fn average(pixels: &[[u8; 3]], members: &[u32]) -> [u8; 3] {
    if members.is_empty() {
        return [0, 0, 0];
    }
    let mut total = [0u64; 3];
    for &i in members {
        let p = pixels[i as usize];
        for c in 0..3 {
            total[c] += u64::from(p[c]);
        }
    }
    let n = members.len() as u64;
    [(total[0] / n) as u8, (total[1] / n) as u8, (total[2] / n) as u8]
}

/// Encodes `width` by `height` RGB pixels as a sixel data stream.
pub fn encode(pixels: &[[u8; 3]], width: usize, height: usize) -> String {
    if width == 0 || height == 0 || pixels.len() < width * height {
        return String::new();
    }
    let (palette, indices) = quantise(&pixels[..width * height], PALETTE);

    let mut out = String::with_capacity(width * height / 2);
    // P1 = 0: pixel aspect 1:1. P2 = 1: leave untouched pixels transparent
    // rather than painting them background, so the cover does not come
    // with a black card behind it.
    out.push_str("\u{1b}Pq");
    for (i, c) in palette.iter().enumerate() {
        // Sixel colours are percentages, not bytes.
        let pc = |v: u8| (u32::from(v) * 100 + 127) / 255;
        out.push_str(&format!("#{i};2;{};{};{}", pc(c[0]), pc(c[1]), pc(c[2])));
    }

    for band in 0..height.div_ceil(BAND) {
        let top = band * BAND;
        let rows = BAND.min(height - top);

        // Which colours appear in this band at all: emitting a pass for
        // every palette entry would multiply the output by fifty.
        let mut present = vec![false; palette.len()];
        for y in top..top + rows {
            for x in 0..width {
                present[indices[y * width + x] as usize] = true;
            }
        }

        let mut first = true;
        for (slot, _) in present.iter().enumerate().filter(|(_, p)| **p) {
            if !first {
                // Carriage return: overlay the next colour on the same band.
                out.push('$');
            }
            first = false;
            out.push_str(&format!("#{slot}"));

            let mut run = (0u8, 0usize);
            for x in 0..width {
                let mut bits = 0u8;
                for (r, row) in (top..top + rows).enumerate() {
                    if indices[row * width + x] as usize == slot {
                        bits |= 1 << r;
                    }
                }
                if bits == run.0 {
                    run.1 += 1;
                } else {
                    write_run(&mut out, run);
                    run = (bits, 1);
                }
            }
            write_run(&mut out, run);
        }
        // Next band down.
        out.push('-');
    }

    out.push_str("\u{1b}\\");
    out
}

/// Sixel run-length encoding: `!n` before a repeated character. Only worth
/// it past three, below which the escape is longer than what it replaces.
fn write_run(out: &mut String, (bits, count): (u8, usize)) {
    if count == 0 {
        return;
    }
    let ch = char::from(0x3F + bits);
    if count > 3 {
        out.push_str(&format!("!{count}{ch}"));
    } else {
        for _ in 0..count {
            out.push(ch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_image_encodes_to_nothing() {
        assert!(encode(&[], 0, 0).is_empty());
        assert!(encode(&[[1, 2, 3]], 4, 4).is_empty(), "short buffer must not panic");
    }

    #[test]
    fn the_stream_is_wrapped_in_the_sixel_introducer_and_terminator() {
        let out = encode(&[[255, 0, 0]; 4], 2, 2);
        assert!(out.starts_with("\u{1b}Pq"), "{out:?}");
        assert!(out.ends_with("\u{1b}\\"), "{out:?}");
    }

    /// Sixel colours are percentages of full scale, not bytes. Sending 255
    /// where 100 was meant is a common way to get a white rectangle.
    #[test]
    fn colours_are_written_as_percentages() {
        let out = encode(&[[255, 0, 0]; 4], 2, 2);
        assert!(out.contains("2;100;0;0"), "full red should be 100: {out:?}");
    }

    #[test]
    fn a_single_colour_image_uses_one_register() {
        let out = encode(&[[10, 20, 30]; 36], 6, 6);
        assert_eq!(out.matches(";2;").count(), 1, "one palette entry: {out:?}");
    }

    /// The whole point of quantising: more distinct colours than registers
    /// must still produce a valid stream.
    #[test]
    fn more_colours_than_registers_are_reduced_to_fit() {
        let pixels: Vec<[u8; 3]> =
            (0..4096).map(|i| [(i % 256) as u8, (i / 16 % 256) as u8, (i / 256) as u8]).collect();
        let (palette, indices) = quantise(&pixels, PALETTE);
        assert!(palette.len() <= PALETTE, "{} registers", palette.len());
        assert!(palette.len() > 1, "should have used several");
        assert!(indices.iter().all(|i| (*i as usize) < palette.len()), "index out of range");
        let out = encode(&pixels, 64, 64);
        assert!(out.starts_with("\u{1b}Pq") && out.ends_with("\u{1b}\\"));
    }

    /// Quantising must keep colours roughly where they were -- a palette
    /// that maps everything to grey is valid sixel and a ruined picture.
    #[test]
    fn quantising_keeps_each_pixel_near_its_own_colour() {
        let pixels: Vec<[u8; 3]> = (0..900)
            .map(|i| match i % 3 {
                0 => [200, 30, 40],
                1 => [30, 200, 40],
                _ => [40, 30, 200],
            })
            .collect();
        let (palette, indices) = quantise(&pixels, PALETTE);
        for (i, p) in pixels.iter().enumerate() {
            let got = palette[indices[i] as usize];
            let error: u32 = (0..3).map(|c| u32::from(got[c].abs_diff(p[c]))).sum();
            assert!(error < 30, "pixel {p:?} became {got:?}");
        }
    }

    #[test]
    fn a_run_is_encoded_once_rather_than_repeated() {
        // A wide band of one colour should compress.
        let out = encode(&[[7, 7, 7]; 600], 100, 6);
        assert!(out.contains('!'), "expected run-length encoding: {out:?}");
        assert!(out.len() < 200, "a flat band should be short, got {}", out.len());
    }

    #[test]
    fn short_runs_are_written_out_rather_than_escaped() {
        let mut out = String::new();
        write_run(&mut out, (1, 2));
        assert_eq!(out, "@@", "two of a kind is shorter written twice");
    }

    #[test]
    fn every_band_is_terminated() {
        // 13 rows is three bands: 6, 6, 1.
        let out = encode(&[[1, 2, 3]; 13 * 4], 4, 13);
        assert_eq!(out.matches('-').count(), 3, "one terminator per band: {out:?}");
    }
}
