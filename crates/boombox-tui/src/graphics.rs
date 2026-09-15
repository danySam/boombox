//! Drawing the album cover as real pixels, where the terminal can.
//!
//! Cell characters top out at about forty samples across a cover, which is
//! an impression of the artwork rather than the artwork. A terminal with a
//! graphics protocol draws the image itself.
//!
//! The protocol is written directly rather than through a library, for one
//! reason: every escape here carries `q=2`, which tells the terminal not to
//! reply. A reply would arrive on stdin and be read as keystrokes, and that
//! is precisely how an earlier attempt at images in this TUI broke every
//! key the user pressed. Nothing in this module ever asks the terminal a
//! question.

use std::io::Write;

use base64::Engine as _;
use ratatui::layout::Rect;

use crate::artwork::Cover;

/// Which protocol to draw covers with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// Block characters. Works everywhere, so it is also the fallback.
    Cells,
    Kitty,
    /// iTerm2's inline images. Takes an image file rather than pixels, so
    /// it gets the original JPEG untouched.
    Iterm,
    /// The oldest of the three and the only one on plain xterm or Windows
    /// Terminal, at the cost of a palette: 256 colours where the other two
    /// are truecolor.
    Sixel,
}

impl Protocol {
    /// Reads the setting, consulting the environment only for `auto`.
    ///
    /// `TERM` and `KITTY_WINDOW_ID` are set by the terminal itself before
    /// the program starts, so reading them costs nothing and cannot
    /// produce input the way a query can.
    pub fn detect(setting: &str) -> Self {
        match setting {
            "off" | "cells" => Self::Cells,
            "kitty" => Self::Kitty,
            "iterm" | "iterm2" => Self::Iterm,
            "sixel" => Self::Sixel,
            _ => {
                // tmux does not forward graphics escapes unless it has been
                // told to, and a silently invisible cover is worse than a
                // blocky one. Multiplexed sessions get the cells.
                if std::env::var_os("TMUX").is_some()
                    || std::env::var("TERM").is_ok_and(|t| t.starts_with("screen"))
                {
                    return Self::Cells;
                }
                let term = std::env::var("TERM").unwrap_or_default();
                let program = std::env::var("TERM_PROGRAM").unwrap_or_default();

                // Preferred in order of what they give us. Kitty and iTerm2
                // are both truecolor; sixel is palette-limited, so it comes
                // last even where a terminal speaks more than one.
                let kitty = term.contains("kitty")
                    || std::env::var_os("KITTY_WINDOW_ID").is_some()
                    // Ghostty and WezTerm speak kitty's protocol too, and
                    // it is the better of the ones they offer.
                    || program == "ghostty"
                    || program == "WezTerm";

                if kitty {
                    Self::Kitty
                } else if program == "iTerm.app" {
                    Self::Iterm
                } else if term.starts_with("foot")
                    || term.starts_with("contour")
                    || term.starts_with("mlterm")
                    || std::env::var_os("WT_SESSION").is_some()
                {
                    Self::Sixel
                } else {
                    // xterm is deliberately absent: it only draws sixel when
                    // built and started for it, and nothing in the
                    // environment says whether it was. Guessing wrong there
                    // means an invisible cover, so it takes the blocks
                    // unless the config says otherwise.
                    Self::Cells
                }
            }
        }
    }
}

impl Protocol {
    /// Downgrades to cells where the chosen protocol cannot actually work.
    ///
    /// Sixel is the only one that needs to know how big a cell is, and a
    /// terminal that does not report its pixel size gives us no way to
    /// size the image. Better to draw blocks than a cover of the wrong
    /// size, or none at all.
    pub fn usable(self) -> Self {
        if self == Self::Sixel && cell_size().is_none() {
            tracing::debug!("no pixel metrics from the terminal; sixel is not usable");
            return Self::Cells;
        }
        self
    }
}

/// Identifier for our one image. Any number will do; a fixed one means a
/// crashed run leaves at most one stale image rather than a new one each
/// time.
const IMAGE_ID: u32 = 0x5C0F;

/// Kitty accepts at most 4096 bytes of payload per escape.
const CHUNK: usize = 4096;

/// How big one cell is, in pixels.
///
/// Sixel draws pixels, not cells, so it has to be told how many to make.
/// This comes from the window-size ioctl rather than a terminal query --
/// the kernel already knows, and asking the terminal would put a reply on
/// stdin.
pub fn cell_size() -> Option<(u16, u16)> {
    let size = ratatui::crossterm::terminal::window_size().ok()?;
    if size.width == 0 || size.height == 0 || size.columns == 0 || size.rows == 0 {
        return None;
    }
    Some((size.width / size.columns, size.height / size.rows))
}

/// Keeps track of what the terminal has been told, so a redraw does not
/// re-send a quarter of a megabyte of pixels.
pub struct Graphics {
    protocol: Protocol,
    /// The cover currently held by the terminal, by URL.
    transmitted: Option<String>,
    /// Where it was last drawn, so an unchanged frame emits nothing.
    placed: Option<Rect>,
}

impl Graphics {
    pub fn new(protocol: Protocol) -> Self {
        Self { protocol, transmitted: None, placed: None }
    }

    /// Brings the terminal's idea of the cover into line with ours.
    ///
    /// `area` is where the cover belongs, or `None` when nothing should be
    /// shown -- a visualisation took the stage, the window got too small,
    /// or there is no cover.
    pub fn sync(
        &mut self,
        out: &mut impl Write,
        area: Option<Rect>,
        url: Option<&str>,
        cover: Option<&Cover>,
    ) -> std::io::Result<()> {
        let (Some(area), Some(url), Some(cover)) = (area, url, cover) else {
            return self.clear(out);
        };

        match self.protocol {
            // The cell grid drew it already.
            Protocol::Cells => Ok(()),
            Protocol::Kitty => {
                if self.transmitted.as_deref() != Some(url) {
                    self.delete(out)?;
                    transmit_kitty(out, cover)?;
                    self.transmitted = Some(url.to_string());
                    // A new image is not placed yet, whatever the old did.
                    self.placed = None;
                }
                if self.placed == Some(area) {
                    return Ok(());
                }
                place_kitty(out, area)?;
                self.placed = Some(area);
                out.flush()
            }
            // Neither of these has a placement separate from the data, so
            // moving the image means sending it again. Both draw at the
            // cursor and become part of the screen, unlike kitty's images
            // which live in a layer of their own with an id.
            Protocol::Iterm | Protocol::Sixel => {
                if self.transmitted.as_deref() == Some(url) && self.placed == Some(area) {
                    return Ok(());
                }
                if let Some(old) = self.placed {
                    blank(out, old)?;
                }
                cursor_to(out, area)?;
                if self.protocol == Protocol::Iterm {
                    draw_iterm(out, cover, area)?;
                } else {
                    draw_sixel(out, cover, area)?;
                }
                self.transmitted = Some(url.to_string());
                self.placed = Some(area);
                out.flush()
            }
        }
    }

    /// Removes the image, if the terminal is holding one.
    pub fn clear(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        if self.transmitted.is_none() {
            return Ok(());
        }
        match self.protocol {
            Protocol::Cells => {}
            Protocol::Kitty => self.delete(out)?,
            // No delete to send: the image is screen content, so it goes
            // by being written over.
            Protocol::Iterm | Protocol::Sixel => {
                if let Some(area) = self.placed {
                    blank(out, area)?;
                }
            }
        }
        self.transmitted = None;
        self.placed = None;
        out.flush()
    }

    fn delete(&self, out: &mut impl Write) -> std::io::Result<()> {
        write!(out, "\x1b_Ga=d,d=i,i={IMAGE_ID},q=2\x1b\\")
    }

    /// Called when the screen has been repainted underneath us, so the
    /// next sync places the image again even if nothing else changed.
    pub fn invalidate(&mut self) {
        self.placed = None;
    }
}

/// Sends the pixels, without displaying them.
fn transmit_kitty(out: &mut impl Write, cover: &Cover) -> std::io::Result<()> {
    let mut rgb = Vec::with_capacity(cover.pixels.len() * 3);
    for pixel in &cover.pixels {
        rgb.extend_from_slice(pixel);
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(&rgb);

    // f=24 is plain RGB, which saves encoding a PNG for an image we
    // already hold decoded.
    let mut chunks = encoded.as_bytes().chunks(CHUNK).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let more = u8::from(chunks.peek().is_some());
        if first {
            write!(
                out,
                "\x1b_Ga=t,f=24,s={},v={},i={IMAGE_ID},q=2,m={more};",
                cover.size, cover.size
            )?;
            first = false;
        } else {
            write!(out, "\x1b_Gm={more},q=2;")?;
        }
        out.write_all(chunk)?;
        write!(out, "\x1b\\")?;
    }
    Ok(())
}

/// Displays the transmitted image over a block of cells.
fn place_kitty(out: &mut impl Write, area: Rect) -> std::io::Result<()> {
    cursor_to(out, area)?;
    write!(out, "\x1b_Ga=p,i={IMAGE_ID},c={},r={},q=2\x1b\\", area.width, area.height)
}

/// Puts the cursor at the top-left of an area. One-based, as terminals count.
fn cursor_to(out: &mut impl Write, area: Rect) -> std::io::Result<()> {
    write!(out, "\x1b[{};{}H", area.y + 1, area.x + 1)
}

/// Writes spaces over an area. The inline protocols have no way to remove
/// an image, so the only way to take one back is to overwrite it.
fn blank(out: &mut impl Write, area: Rect) -> std::io::Result<()> {
    let row = " ".repeat(area.width as usize);
    for y in area.y..area.y + area.height {
        write!(out, "\x1b[{};{}H{row}", y + 1, area.x + 1)?;
    }
    Ok(())
}

/// iTerm2 takes an image file, so it gets the original JPEG: no decode, no
/// resample, no re-encode. It is also a tenth of what the pixels would
/// cost on the wire.
fn draw_iterm(out: &mut impl Write, cover: &Cover, area: Rect) -> std::io::Result<()> {
    if cover.source.is_empty() {
        return Ok(());
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(&cover.source);
    write!(
        out,
        "\x1b]1337;File=inline=1;size={};width={};height={};preserveAspectRatio=1:{encoded}\x07",
        cover.source.len(),
        area.width,
        area.height
    )
}

/// Sixel needs the picture at the exact pixel size it will occupy, since
/// it draws pixels and knows nothing about cells.
fn draw_sixel(out: &mut impl Write, cover: &Cover, area: Rect) -> std::io::Result<()> {
    let Some((cell_w, cell_h)) = cell_size() else {
        return Ok(());
    };
    let width = area.width as usize * cell_w as usize;
    let height = area.height as usize * cell_h as usize;
    if width == 0 || height == 0 {
        return Ok(());
    }

    let mut pixels = Vec::with_capacity(width * height);
    for y in 0..height {
        for x in 0..width {
            let (u0, u1) = (x as f32 / width as f32, (x + 1) as f32 / width as f32);
            let (v0, v1) = (y as f32 / height as f32, (y + 1) as f32 / height as f32);
            pixels.push(cover.sample_area(u0, u1, v0, v1));
        }
    }
    out.write_all(crate::sixel::encode(&pixels, width, height).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_setting_is_obeyed_without_consulting_the_environment() {
        assert_eq!(Protocol::detect("off"), Protocol::Cells);
        assert_eq!(Protocol::detect("cells"), Protocol::Cells);
        assert_eq!(Protocol::detect("kitty"), Protocol::Kitty);
    }

    /// An explicit `kitty` still means kitty: someone running tmux with
    /// passthrough configured has said what they want, and second-guessing
    /// them would leave no way to switch it on.
    #[test]
    fn multiplexers_fall_back_but_can_be_overridden() {
        assert_eq!(Protocol::detect("kitty"), Protocol::Kitty);
    }

    /// Anything unrecognised falls back rather than failing: a typo in the
    /// config should cost the pretty version, not the album art.
    #[test]
    fn an_unknown_setting_falls_back_to_cells_or_detection() {
        // Whatever the terminal running the tests supports, a setting it does
        // not recognise must behave exactly like "auto".
        assert_eq!(Protocol::detect("nonsense"), Protocol::detect("auto"));
    }

    fn drawn(cover: &Cover, area: Rect) -> String {
        let mut out = Vec::new();
        let mut graphics = Graphics::new(Protocol::Kitty);
        graphics.sync(&mut out, Some(area), Some("u"), Some(cover)).unwrap();
        String::from_utf8_lossy(&out).into_owned()
    }

    fn tiny() -> Cover {
        Cover::from_pixels(2, vec![[1, 2, 3], [4, 5, 6], [7, 8, 9], [10, 11, 12]])
    }

    /// The whole reason this is hand-written. A terminal reply would be
    /// read as keystrokes, which is how images broke input here before.
    #[test]
    fn every_escape_suppresses_the_terminals_reply() {
        let text = drawn(&tiny(), Rect { x: 0, y: 0, width: 4, height: 2 });
        let escapes: Vec<&str> = text.split("\u{1b}_G").skip(1).collect();
        assert!(!escapes.is_empty(), "something should have been written");
        for escape in escapes {
            let header = escape.split(';').next().unwrap_or_default();
            assert!(header.contains("q=2"), "missing q=2 in: {header}");
        }
    }

    #[test]
    fn the_image_is_placed_at_the_area_it_was_given() {
        let text = drawn(&tiny(), Rect { x: 10, y: 4, width: 6, height: 3 });
        // Cursor first, one-based, row then column.
        assert!(text.contains("\u{1b}[5;11H"), "{text:?}");
        assert!(text.contains("c=6,r=3"), "{text:?}");
    }

    #[test]
    fn the_pixels_are_sent_once_and_the_placement_is_cheap() {
        let cover = tiny();
        let area = Rect { x: 0, y: 0, width: 4, height: 2 };
        let mut out = Vec::new();
        let mut graphics = Graphics::new(Protocol::Kitty);

        graphics.sync(&mut out, Some(area), Some("u"), Some(&cover)).unwrap();
        let first = out.len();
        assert!(String::from_utf8_lossy(&out).contains("a=t"), "pixels sent");

        // Same cover, same place: nothing more to say.
        out.clear();
        graphics.sync(&mut out, Some(area), Some("u"), Some(&cover)).unwrap();
        assert!(out.is_empty(), "an unchanged frame must be silent");

        // Moved: place again, but do not resend the pixels.
        out.clear();
        let moved = Rect { x: 1, ..area };
        graphics.sync(&mut out, Some(moved), Some("u"), Some(&cover)).unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("a=p"), "placed again");
        assert!(!text.contains("a=t"), "but not retransmitted");
        assert!(out.len() < first / 2, "and cheaply");
    }

    #[test]
    fn a_new_cover_replaces_the_old_one() {
        let area = Rect { x: 0, y: 0, width: 4, height: 2 };
        let mut out = Vec::new();
        let mut graphics = Graphics::new(Protocol::Kitty);
        graphics.sync(&mut out, Some(area), Some("first"), Some(&tiny())).unwrap();

        out.clear();
        graphics.sync(&mut out, Some(area), Some("second"), Some(&tiny())).unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("a=d"), "the old image is deleted: {text:?}");
        assert!(text.contains("a=t"), "and the new one sent");
    }

    /// Switching to a visualisation, or shrinking the window, has to take
    /// the image away -- it is drawn over the top of the cells, so leaving
    /// it would cover whatever replaced it.
    #[test]
    fn losing_the_area_removes_the_image() {
        let area = Rect { x: 0, y: 0, width: 4, height: 2 };
        let mut out = Vec::new();
        let mut graphics = Graphics::new(Protocol::Kitty);
        graphics.sync(&mut out, Some(area), Some("u"), Some(&tiny())).unwrap();

        out.clear();
        graphics.sync(&mut out, None, None, None).unwrap();
        assert!(String::from_utf8_lossy(&out).contains("a=d"), "deleted");

        // And again is silent: there is nothing left to delete.
        out.clear();
        graphics.sync(&mut out, None, None, None).unwrap();
        assert!(out.is_empty());
    }

    fn with_source() -> Cover {
        let mut cover = tiny();
        cover.source = b"jpeg-bytes-here".to_vec();
        cover
    }

    fn drawn_with(protocol: Protocol, cover: &Cover, area: Rect) -> String {
        let mut out = Vec::new();
        let mut graphics = Graphics::new(protocol);
        graphics.sync(&mut out, Some(area), Some("u"), Some(cover)).unwrap();
        String::from_utf8_lossy(&out).into_owned()
    }

    /// iTerm2 takes a file, so it gets the original bytes rather than a
    /// re-encoding of pixels we decoded from them.
    #[test]
    fn iterm_sends_the_original_file_untouched() {
        let cover = with_source();
        let text = drawn_with(Protocol::Iterm, &cover, Rect { x: 2, y: 1, width: 8, height: 4 });
        let encoded = base64::engine::general_purpose::STANDARD.encode(&cover.source);
        assert!(text.contains(&encoded), "the source file should be sent verbatim");
        assert!(text.contains("size=15"), "and its true length declared: {text:?}");
        assert!(text.contains("width=8;height=4"), "sized in cells: {text:?}");
        assert!(text.contains("preserveAspectRatio=1"), "{text:?}");
    }

    /// Nothing to send is not an error -- a cover fetched before this
    /// field existed, or one that failed to download, still has pixels.
    #[test]
    fn iterm_draws_nothing_without_a_source_file() {
        let text = drawn_with(Protocol::Iterm, &tiny(), Rect { x: 0, y: 0, width: 4, height: 2 });
        assert!(!text.contains("1337"), "{text:?}");
    }

    /// The inline protocols have no delete, so the only way to take an
    /// image back is to write over the cells it occupied.
    #[test]
    fn moving_an_inline_image_blanks_where_it_was() {
        let cover = with_source();
        let area = Rect { x: 3, y: 2, width: 5, height: 3 };
        let mut out = Vec::new();
        let mut graphics = Graphics::new(Protocol::Iterm);
        graphics.sync(&mut out, Some(area), Some("u"), Some(&cover)).unwrap();

        out.clear();
        let moved = Rect { x: 10, ..area };
        graphics.sync(&mut out, Some(moved), Some("u"), Some(&cover)).unwrap();
        let text = String::from_utf8_lossy(&out);
        // Three rows of five spaces at the old position.
        assert!(text.contains("\u{1b}[3;4H     "), "old rows blanked: {text:?}");
        assert!(text.contains("\u{1b}[11;H") || text.contains("width=5"), "{text:?}");
    }

    #[test]
    fn an_unchanged_inline_frame_is_silent() {
        let cover = with_source();
        let area = Rect { x: 0, y: 0, width: 4, height: 2 };
        let mut out = Vec::new();
        let mut graphics = Graphics::new(Protocol::Iterm);
        graphics.sync(&mut out, Some(area), Some("u"), Some(&cover)).unwrap();
        out.clear();
        graphics.sync(&mut out, Some(area), Some("u"), Some(&cover)).unwrap();
        assert!(out.is_empty(), "nothing changed, nothing to say");
    }

    /// The cell grid draws its own cover, so this must stay out of the way
    /// entirely rather than emitting anything.
    #[test]
    fn the_cell_protocol_writes_nothing_at_all() {
        let text =
            drawn_with(Protocol::Cells, &with_source(), Rect { x: 0, y: 0, width: 4, height: 2 });
        assert!(text.is_empty(), "{text:?}");
    }

    #[test]
    fn a_large_cover_is_split_into_chunks_the_protocol_allows() {
        let size = 64u32;
        let cover = Cover::from_pixels(size, vec![[9, 9, 9]; (size * size) as usize]);
        let text = drawn(&cover, Rect { x: 0, y: 0, width: 10, height: 5 });
        let payloads: Vec<&str> = text.split("\u{1b}_G").skip(1).collect();
        assert!(payloads.len() > 1, "should have been chunked");
        for payload in payloads {
            let body = payload.split_once(';').map(|(_, b)| b).unwrap_or("");
            // Cut at the terminator: what follows it belongs to whatever
            // was written next, not to this payload.
            let body = body.split("\u{1b}\\").next().unwrap_or("");
            assert!(body.len() <= CHUNK, "chunk of {} exceeds the limit", body.len());
        }
    }
}
