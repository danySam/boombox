use boombox_core::api::{Entry, PlaybackState, PlayingItem};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::app::{App, Focus, ToastKind, View, VisualMode};
use crate::palette::Palette;

const ACCENT: Color = Color::Green;
const DIM: Color = Color::DarkGray;

/// Separator plus title, byline and progress.
const BAR_HEIGHT: u16 = 4;

/// Draws a frame, returning where the album cover belongs.
///
/// The rectangle is reported rather than drawn into when the terminal
/// draws real pixels: the graphics layer fills it after the frame is
/// flushed, and the cells underneath are deliberately left blank so the
/// image is not painted over.
pub fn draw(frame: &mut Frame, app: &App) -> Option<Rect> {
    let area = frame.area();
    let state = app.playback();

    // Left alone with something playing, the stage takes the whole screen.
    // Everything except the track line and the progress bar goes away.
    if app.is_idle() {
        idle(frame, area, app, state.as_ref());
        return None;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(1), // legend, or a toast when there is one
            Constraint::Length(BAR_HEIGHT),
        ])
        .split(area);

    let cover = stage(frame, chunks[0], app, state.as_ref());
    player_bar(frame, chunks[2], app, state.as_ref());

    // Over the stage, not beside it: browsing is a layer, and playback
    // carries on underneath it.
    if app.browse_open {
        palette(frame, chunks[0], app);
    }
    // The toast takes the legend's line rather than covering content: the
    // legend is the most disposable thing on screen.
    match app.visible_toast() {
        Some(toast) => {
            let style = match toast.kind {
                ToastKind::Error => Style::new().fg(Color::Red),
                ToastKind::Info => Style::new().fg(Color::Yellow),
            };
            frame.render_widget(
                Paragraph::new(Span::styled(format!(" {}", toast.text), style)),
                chunks[1],
            );
        }
        None => legend(frame, chunks[1], app),
    }
    // Above the browse panel, because it is the thing being answered.
    if let Some(buffer) = app.link_prompt() {
        link_prompt(frame, area, buffer);
    }
    if app.show_help {
        help(frame, area, app);
    }
    // Anything drawn over the stage hides the picture, and an image sits
    // above the cells rather than under them -- so it has to go.
    if app.browse_open || app.show_help { None } else { cover }
}

/// `("repeat", 'r')` renders as `[r]epeat`: the bracket sits exactly where
/// the key is, so the word and the key teach each other. A number would
/// have to be memorised separately from the thing it opens, which is what
/// made the old digits hard to keep hold of.
fn mnemonic(word: &str, key: char) -> Vec<Span<'static>> {
    let bracket = Style::new().fg(DIM);
    let letter = Style::new().fg(ACCENT).add_modifier(Modifier::BOLD);

    let mut chars = word.chars();
    match chars.next() {
        // The usual case: the key is the word's own first letter.
        Some(first) if first.eq_ignore_ascii_case(&key) => vec![
            Span::styled("[", bracket),
            Span::styled(first.to_string(), letter),
            Span::styled("]", bracket),
            Span::styled(chars.as_str().to_string(), Style::new().fg(DIM)),
        ],
        // Anything else -- Enter, Tab, a symbol -- gets the key spelled out
        // in front, which is the same shape without pretending to a
        // mnemonic that is not there.
        _ => vec![
            Span::styled("[", bracket),
            Span::styled(key.to_string(), letter),
            Span::styled("] ", bracket),
            Span::styled(word.to_string(), Style::new().fg(DIM)),
        ],
    }
}

/// The keys that apply right now, spelled so the key is visible inside the
/// word. Context-sensitive because the full set is too long to read, and
/// most of it is irrelevant at any given moment.
fn legend(frame: &mut Frame, area: Rect, app: &App) {
    let items: Vec<(&str, char)> = if app.link_prompt().is_some() {
        vec![("play", '\u{21b5}'), ("cancel", '\u{238b}')]
    } else if app.is_typing() {
        vec![("search", '\u{21b5}'), ("cancel", '\u{238b}')]
    } else if app.browse_open {
        let mut items = vec![
            ("play", '\u{21b5}'),
            ("track", 't'),
            ("enqueue", 'e'),
            ("Enqueue all", 'E'),
            ("like", '.'),
        ];
        // Offered where the pasted playlist will actually appear. It is
        // the only way to reach a Daily Mix, so it needs to be visible
        // rather than something you have to already know.
        if app.view == View::Playlists {
            items.push(("Add playlist", 'A'));
        }
        // Inside a playlist, the way out was only mentioned in the help screen.
        if app.can_go_back() {
            items.push(("back", '\u{232b}'));
        }
        items.extend([("close", '\u{21e5}'), ("help", '?')]);
        items
    } else {
        vec![
            ("liked", 'l'),
            ("albums", 'a'),
            ("playlists", 'p'),
            ("Queue", 'Q'),
            ("devices", 'd'),
            ("search", '/'),
            ("visual", 'v'),
            ("help", '?'),
        ]
    };

    let mut spans = vec![Span::raw(" ")];
    for (word, key) in items {
        spans.extend(mnemonic(word, key));
        spans.push(Span::raw("  "));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// What sits behind everything: the visualisation, or the track itself when
/// no visualisation is running.
fn stage(frame: &mut Frame, area: Rect, app: &App, state: Option<&PlaybackState>) -> Option<Rect> {
    match app.visual {
        Some(mode) => {
            visual(frame, area, app, mode);
            None
        }
        None => now_playing_card(frame, area, app, state),
    }
}

/// Draws a cover with half blocks: `\u{2580}` carries two pixels, the upper
/// as its foreground and the lower as its background.
///
/// Terminal cells are about twice as tall as they are wide, so two pixels
/// stacked in one cell come out roughly square -- which is why the area
/// this is given is twice as wide as it is tall.
struct CoverArt<'a> {
    cover: &'a crate::artwork::Cover,
}

/// Every arrangement of four quadrants, indexed by a bitmask: 1 top-left,
/// 2 top-right, 4 bottom-left, 8 bottom-right. Set bits take the
/// foreground colour, clear bits the background.
///
/// All sixteen live in the Block Elements range alongside the half blocks
/// this replaces, so nothing here needs a font the previous version did
/// not already need.
const QUADRANTS: [char; 16] = [
    ' ', '\u{2598}', '\u{259d}', '\u{2580}', '\u{2596}', '\u{258c}', '\u{259e}', '\u{259b}',
    '\u{2597}', '\u{259a}', '\u{2590}', '\u{259c}', '\u{2584}', '\u{2599}', '\u{259f}', '\u{2588}',
];

/// Reduces four sub-pixels to the one glyph and two colours a cell can
/// actually show, by trying every split and keeping the closest.
///
/// Splitting on brightness was the obvious approach and measurably the
/// wrong one: where four sub-pixels differ in hue rather than in
/// brightness it groups the wrong pair, and the result was *less* faithful
/// than plain half blocks. There are only sixteen ways to divide four
/// things into two groups, so there is no need to guess -- and because
/// top-against-bottom is one of the sixteen, this can never do worse than
/// the half blocks it replaces.
fn quadrant(quads: [[u8; 3]; 4]) -> (char, Color, Color) {
    // Seeded with the solid block so a cell with nothing to distinguish
    // stays solid: every split scores the same on a flat cell, and a
    // space-with-a-background is a fussier way to draw the same thing.
    let mut best = (15usize, f32::MAX, [0u8; 3], [0u8; 3]);

    for mask in (0..16usize).rev() {
        let (mut fg, mut bg) = (Vec::with_capacity(4), Vec::with_capacity(4));
        for (i, quad) in quads.iter().enumerate() {
            if mask >> i & 1 == 1 { fg.push(*quad) } else { bg.push(*quad) }
        }
        // An empty group takes the other's colour: the glyph covers none
        // of the cell with it, so it makes no difference to what is shown.
        let fg_mean = mean(&fg).unwrap_or_else(|| mean(&bg).unwrap_or([0, 0, 0]));
        let bg_mean = mean(&bg).unwrap_or(fg_mean);

        let error: f32 = quads
            .iter()
            .enumerate()
            .map(|(i, quad)| {
                let shown = if mask >> i & 1 == 1 { fg_mean } else { bg_mean };
                (0..3)
                    .map(|c| {
                        let d = f32::from(quad[c]) - f32::from(shown[c]);
                        d * d
                    })
                    .sum::<f32>()
            })
            .sum();

        if error < best.1 {
            best = (mask, error, fg_mean, bg_mean);
        }
    }

    let (mask, _, fg, bg) = best;
    (QUADRANTS[mask], Color::Rgb(fg[0], fg[1], fg[2]), Color::Rgb(bg[0], bg[1], bg[2]))
}

fn mean(colours: &[[u8; 3]]) -> Option<[u8; 3]> {
    if colours.is_empty() {
        return None;
    }
    let n = colours.len() as u32;
    let sum = colours.iter().fold([0u32; 3], |mut acc, c| {
        for i in 0..3 {
            acc[i] += u32::from(c[i]);
        }
        acc
    });
    Some([(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8])
}

impl ratatui::widgets::Widget for CoverArt<'_> {
    fn render(self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let rows = area.height as f32;
        let cols = area.width as f32;
        for y in 0..area.height {
            for x in 0..area.width {
                // Four sub-pixels per cell rather than two: the glyph set
                // can place a corner, so the cover gets twice the
                // horizontal detail for the same number of cells.
                let (u0, u2) = (x as f32 / cols, (x + 1) as f32 / cols);
                let (v0, v2) = (y as f32 / rows, (y + 1) as f32 / rows);
                let (u1, v1) = ((u0 + u2) / 2.0, (v0 + v2) / 2.0);
                let quads = [
                    self.cover.sample_area(u0, u1, v0, v1),
                    self.cover.sample_area(u1, u2, v0, v1),
                    self.cover.sample_area(u0, u1, v1, v2),
                    self.cover.sample_area(u1, u2, v1, v2),
                ];
                let (ch, fg, bg) = quadrant(quads);
                if let Some(cell) = buf.cell_mut((area.x + x, area.y + y)) {
                    cell.set_char(ch).set_style(Style::new().fg(fg).bg(bg));
                }
            }
        }
    }
}

/// The largest square cover that fits, or `None` when there is not enough
/// room to be worth it. Below about ten cells a cover is a smudge, and the
/// text it would crowd out is more use.
fn cover_area(area: Rect) -> Option<Rect> {
    /// Lower than it was, because the golden section already shrinks the
    /// cover: keeping the old floor meant a twenty-row terminal lost the
    /// art entirely, which is worse than showing a small one.
    const MIN: u16 = 8;
    /// The cover takes the larger part of a golden section of the stage
    /// height, leaving the rest as margin. Filling the height made it the
    /// loudest thing on screen and, at one pixel per cell, showed off the
    /// coarseness rather than the picture.
    const GOLDEN: f32 = 0.618;
    /// Past this a cover is merely large rather than better: the source is
    /// 128 pixels, and a wall of colour crowds out everything else.
    const MAX: u16 = 20;

    let side = ((area.height as f32 * GOLDEN) as u16)
        // Half the width, because two pixels share a cell vertically.
        .min(area.width / 3)
        .min(MAX);
    if side < MIN {
        return None;
    }
    Some(Rect { x: area.x, y: area.y, width: side * 2, height: side })
}

/// The track, centred, with nothing else competing for the space.
///
/// With a cover to show, the two sit side by side: the stage is wide and
/// short, so stacking them wastes the width and squeezes the picture.
fn now_playing_card(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    state: Option<&PlaybackState>,
) -> Option<Rect> {
    let Some(item) = state.and_then(|s| s.item.as_ref()) else {
        frame.render_widget(
            Paragraph::new(
                "Nothing is playing.\n\nStart Spotify somewhere, then press d to pick a device.",
            )
            .style(Style::new().fg(DIM))
            .alignment(Alignment::Center),
            centered_v(area, 3),
        );
        return None;
    };

    let mut lines = vec![
        Line::from(Span::styled(
            item.name().to_string(),
            Style::new().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(item.byline(), Style::new().fg(ACCENT))),
    ];
    if let Some(collection) = item.collection() {
        lines.push(Line::from(Span::styled(collection.to_string(), Style::new().fg(DIM))));
    }
    let height = lines.len() as u16;

    let art = app.cover.as_ref().and_then(|cover| cover_area(area).map(|r| (cover, r)));
    let Some((cover, mut art_area)) = art else {
        frame.render_widget(
            Paragraph::new(lines).alignment(Alignment::Center),
            centered_v(area, height),
        );
        return None;
    };

    // Centre the pair horizontally, then hand the rest to the text.
    let gap = 4u16;
    let text_width = area.width.saturating_sub(art_area.width + gap);
    // Wide enough that a long album title is not clipped -- it used to
    // have the whole stage and now shares it with the cover.
    let block_width = art_area.width + gap + text_width.min(56);
    let left = area.x + (area.width.saturating_sub(block_width)) / 2;
    art_area.x = left;
    art_area.y = area.y + (area.height.saturating_sub(art_area.height)) / 2;

    tracing::debug!(
        "cover drawn: {}x{} cells = {}x{} samples",
        art_area.width,
        art_area.height,
        art_area.width * 2,
        art_area.height * 2
    );
    // Left blank when the terminal draws pixels: the image goes on top of
    // these cells, so anything written here would show through the gaps.
    if app.draws_cover_in_cells() {
        frame.render_widget(CoverArt { cover }, art_area);
    }
    frame.render_widget(
        Paragraph::new(lines),
        centered_v(
            Rect {
                x: art_area.x + art_area.width + gap,
                y: area.y,
                width: block_width.saturating_sub(art_area.width + gap),
                height: area.height,
            },
            height,
        ),
    );
    Some(art_area)
}

/// Full bleed: no border, no title. The stage is the whole area it is given.
fn visual(frame: &mut Frame, area: Rect, app: &App, mode: VisualMode) {
    if app.spectrum.is_empty() && app.waveform.is_empty() {
        frame.render_widget(
            Paragraph::new(
                "No audio is being decoded here.\n\nThe visualisations read samples from boombox's own \
                 Connect device, so they need a streaming build with playback transferred to it.",
            )
            .style(Style::new().fg(DIM))
            .alignment(Alignment::Center),
            centered_v(area, 4),
        );
        return;
    }
    match mode {
        VisualMode::Bars => frame.render_widget(
            Bars { bands: &app.smoothed, peaks: &app.peaks, palette: app.palette },
            area,
        ),
        VisualMode::Spectrogram => {
            frame.render_widget(Waterfall { history: &app.history, palette: app.palette }, area)
        }
        VisualMode::Scope => scope(frame, area, app),
    }
}

/// The pinned player. Always the same three lines in the same place, so the
/// eye learns where to look regardless of what the stage is doing.
fn player_bar(frame: &mut Frame, area: Rect, app: &App, state: Option<&PlaybackState>) {
    let block = Block::default().borders(Borders::TOP).border_style(Style::new().fg(DIM));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1)])
        .split(inner);

    let Some(state) = state else {
        frame.render_widget(
            Paragraph::new(Span::styled(" \u{23f8}  nothing playing", Style::new().fg(DIM))),
            rows[0],
        );
        return;
    };

    let item = state.item.as_ref();
    let glyph = if state.is_playing { "\u{25b6}" } else { "\u{23f8}" };

    // Words need roughly twice what the icons do; below this the title has
    // nothing left, so the icons come back.
    let wide = area.width >= 96;
    // Sized to what it holds. A fixed column clipped its right end, which is
    // where the device name and the daemon dot sit; the title gives way
    // instead, down to its minimum.
    let meta = Line::from(meta_spans(app, state, wide));
    let meta_width = (meta.width() as u16).min(rows[0].width.saturating_sub(10));
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(10), Constraint::Length(meta_width)])
        .split(rows[0]);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!(" {glyph}  "), Style::new().fg(ACCENT)),
            Span::styled(
                item.map(PlayingItem::name).unwrap_or("\u{2014}").to_string(),
                Style::new().add_modifier(Modifier::BOLD),
            ),
        ])),
        split[0],
    );
    frame.render_widget(Paragraph::new(meta).alignment(Alignment::Right), split[1]);
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!("    {}", item.map(PlayingItem::byline).unwrap_or_default()),
            Style::new().fg(DIM),
        )),
        rows[1],
    );
    progress(frame, rows[2], state, &app.envelope);
}

/// Shuffle, repeat, volume, device.
///
/// Written as `[s]huffle off` rather than an icon, so the row that shows
/// the state also teaches the key that changes it -- there is no second
/// place to look up how to turn shuffle on. Falls back to icons when the
/// terminal is too narrow to carry the words.
fn meta_spans(app: &App, state: &PlaybackState, wide: bool) -> Vec<Span<'static>> {
    let on = Style::new().fg(ACCENT);
    let off = Style::new().fg(DIM);
    let mut spans = Vec::new();

    if wide {
        spans.extend(mnemonic("shuffle", 's'));
        spans.push(Span::styled(
            format!(" {}  ", if state.shuffle_state { "on" } else { "off" }),
            if state.shuffle_state { on } else { off },
        ));
        spans.extend(mnemonic("repeat", 'r'));
        spans.push(Span::styled(
            format!(" {}  ", state.repeat_state),
            if state.repeat_state == boombox_core::api::RepeatState::Off { off } else { on },
        ));
    } else {
        spans.push(Span::styled(
            format!(" \u{21c4} {} ", if state.shuffle_state { "on" } else { "off" }),
            if state.shuffle_state { on } else { off },
        ));
        spans.push(Span::styled(
            format!(" \u{21bb} {} ", state.repeat_state),
            if state.repeat_state == boombox_core::api::RepeatState::Off { off } else { on },
        ));
    }

    if let Some(v) = app.volume() {
        spans.extend(volume_field(v, app.volume_pending(), on, off));
    }
    if let Some(d) = &state.device {
        spans.push(Span::styled(format!("\u{25b8} {} ", d.name), off));
    }
    spans.push(Span::styled(if app.connected_to_daemon { "\u{25cf} " } else { "\u{25cb} " }, off));
    spans
}

/// Cells in the volume bar. With the number beside it this is the whole
/// field; see [`VOLUME_FIELD`].
const VOLUME_BAR: usize = 8;

/// Width the volume takes in the status row, bar or not.
///
/// Fixed, so switching between the two does not shove shuffle and repeat
/// sideways. A row that rearranges itself every time you touch the volume
/// would be worse than having no bar at all.
const VOLUME_FIELD: usize = VOLUME_BAR + 7;

/// The volume, as a bar while a change is in flight and a plain number
/// once it has landed.
///
/// The bar exists because the number alone gives no sense of movement:
/// the API takes about eleven seconds to apply a change, so between the
/// keypress and the sound there is nothing to watch. A filling bar at
/// least shows where in the range the change is heading.
fn volume_field(v: u32, pending: bool, on: Style, off: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if pending {
        let cells = bar(v as f64 / 100.0, VOLUME_BAR);
        let filled = cells.chars().filter(|c| *c == '\u{2501}').count();
        let mut chars = cells.chars();
        let done: String = chars.by_ref().take(filled).collect();
        spans.push(Span::styled("\u{266a} ", off));
        spans.push(Span::styled(done, on));
        spans.push(Span::styled(chars.collect::<String>(), off));
        spans.push(Span::styled(format!(" {v:>3}%"), on));
    } else {
        // Padded on the left, so the glyph stays next to its number and
        // only the empty space changes size.
        let text = format!("\u{266a} {v}%");
        let pad = VOLUME_FIELD.saturating_sub(text.chars().count());
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(text, off));
    }
    spans.push(Span::raw("  "));
    spans
}

fn progress(frame: &mut Frame, area: Rect, state: &PlaybackState, envelope: &[f32]) {
    let clock_text = format!(" {} / {} ", clock(state.progress()), clock(state.duration()));
    let bar_width = area.width.saturating_sub(clock_text.len() as u16 + 2) as usize;

    // With a streaming daemon the seek bar is drawn from the track's own
    // amplitude; without one there is no audio to measure, so fall back to a
    // plain progress line.
    if envelope.len() >= 2 && bar_width > 0 {
        let played = (state.fraction() * bar_width as f64).round() as usize;
        frame.render_widget(
            WaveBar { envelope, played, width: bar_width, clock: &clock_text },
            area,
        );
    } else {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!(" {}", clock(state.progress())), Style::new().fg(DIM)),
                Span::raw(" "),
                Span::styled(bar(state.fraction(), bar_width), Style::new().fg(ACCENT)),
                Span::styled(format!(" {}", clock(state.duration())), Style::new().fg(DIM)),
            ])),
            area,
        );
    }
}

/// Idle: the stage gets everything except a track line and the progress bar.
fn idle(frame: &mut Frame, area: Rect, app: &App, state: Option<&PlaybackState>) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    stage(frame, chunks[0], app, state);

    let Some(state) = state else { return };
    let item = state.item.as_ref();
    // No clock here: the progress bar below already carries one, and two
    // copies of the same time a line apart reads as a rendering fault.
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(10)])
        .split(chunks[1]);

    let name = item.map(PlayingItem::name).unwrap_or("\u{2014}");
    let byline = item.map(PlayingItem::byline).unwrap_or_default();
    let line =
        if byline.is_empty() { format!(" {name}") } else { format!(" {name}  \u{b7}  {byline}") };
    frame.render_widget(Paragraph::new(Span::styled(line, Style::new().fg(DIM))), split[0]);

    progress(frame, chunks[2], state, &app.envelope);
}

/// The browse layer. Centred over the stage rather than beside it, so
/// closing it gives the whole screen back.
/// Rows of content the palette will draw, borders excluded.
///
/// Used to size the box to what is in it. A queue of three in a box built
/// for twenty is mostly empty box.
fn palette_rows(app: &App) -> usize {
    /// The query box above search results: its own bordered line.
    const SEARCH_BOX: usize = 3;
    // An empty list still draws one line, saying so.
    let listed = |n: usize| n.max(1);
    match &app.view {
        View::Queue => listed(app.queue.len() + usize::from(app.playing_row())),
        View::Devices => listed(app.devices.len()),
        View::Search => SEARCH_BOX + listed(rows_with_headings(&app.entries).len()),
        v if v.is_entry_list() => listed(rows_with_headings(&app.entries).len()),
        _ => 1,
    }
}

fn palette(frame: &mut Frame, area: Rect, app: &App) {
    /// One line of content between the borders -- enough for the "nothing
    /// here" an empty list draws, and no more.
    const MIN_HEIGHT: u16 = 3;

    let width = (area.width as u32 * 78 / 100).clamp(30, 110) as u16;
    let ceiling = ((area.height as u32 * 82 / 100).max(MIN_HEIGHT as u32) as u16).min(area.height);
    // While a list is still arriving, take the full height rather than
    // sizing to the "Loading..." line and jumping open a moment later.
    let height = if app.loading {
        ceiling
    } else {
        (palette_rows(app) as u16 + 2).clamp(MIN_HEIGHT, ceiling)
    };
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    };

    frame.render_widget(Clear, popup);
    match &app.view {
        View::Queue => queue(frame, popup, app),
        View::Devices => devices(frame, popup, app),
        View::Search => search(frame, popup, app),
        v if v.is_entry_list() => entry_list(frame, popup, app, &app.list_title()),
        _ => {}
    }
}

fn pane(title: &str, focused: bool) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focused { ACCENT } else { DIM }))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(if focused { ACCENT } else { Color::Reset }),
        ))
}

fn queue(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::Main;
    let playing = app.playback().and_then(|s| s.item.clone());
    let title = format!("Queue ({})", app.queue.len());
    let block = pane(&title, focused);

    if app.queue.is_empty() && playing.is_none() {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new("Queue is empty.")
                .style(Style::new().fg(DIM))
                .alignment(Alignment::Center),
            centered_v(inner, 1),
        );
        return;
    }

    let mut items: Vec<ListItem> = Vec::with_capacity(app.queue.len() + 1);
    // What is playing goes first, marked rather than numbered: it is not
    // somewhere to go, it is where you are.
    if let Some(item) = &playing {
        items.push(ListItem::new(Line::from(vec![
            Span::styled("  \u{25b6}  ", Style::new().fg(ACCENT)),
            Span::styled(item.name().to_string(), Style::new().add_modifier(Modifier::BOLD)),
            Span::styled(format!("  \u{b7}  {}", item.byline()), Style::new().fg(DIM)),
        ])));
    }
    items.extend(app.queue.iter().enumerate().map(|(i, item)| {
        ListItem::new(Line::from(vec![
            Span::styled(format!("{:>3}  ", i + 1), Style::new().fg(DIM)),
            Span::raw(item.name().to_string()),
            Span::styled(format!("  \u{b7}  {}", item.byline()), Style::new().fg(DIM)),
        ]))
    }));

    let mut list_state = ListState::default();
    list_state.select(Some(app.queue_index));
    frame.render_stateful_widget(
        List::new(items).block(block).highlight_style(if focused {
            Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        }),
        area,
        &mut list_state,
    );
}

fn devices(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::Main;
    let block = pane("Devices", focused);

    if app.devices.is_empty() {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new("No devices. Open Spotify somewhere.")
                .style(Style::new().fg(DIM))
                .alignment(Alignment::Center),
            centered_v(inner, 1),
        );
        return;
    }

    let width = app.devices.iter().map(|d| d.name.chars().count()).max().unwrap_or(0);
    let items: Vec<ListItem> = app
        .devices
        .iter()
        .map(|d| {
            let volume = match (d.supports_volume, d.volume_percent) {
                (true, Some(v)) => format!("{v}%"),
                _ => "\u{2014}".into(),
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    if d.is_active { " \u{25cf} " } else { " \u{25cb} " },
                    Style::new().fg(if d.is_active { ACCENT } else { DIM }),
                ),
                Span::raw(format!("{:width$}  ", d.name, width = width)),
                Span::styled(format!("{:10} {volume}", d.device_type), Style::new().fg(DIM)),
            ]))
        })
        .collect();

    let mut list_state = ListState::default();
    list_state.select(Some(app.device_index));
    frame.render_stateful_widget(
        List::new(items).block(block).highlight_style(if focused {
            Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        }),
        area,
        &mut list_state,
    );
}

/// Where to break the key list into two columns.
///
/// At a blank row nearest the middle, so a group of related keys is not
/// torn across the gap. The exact middle if there are no blanks.
fn split_point(rows: &[(&str, &str)]) -> usize {
    let middle = rows.len().div_ceil(2);
    rows.iter()
        .enumerate()
        .filter(|(_, (key, label))| key.is_empty() && label.is_empty())
        .map(|(i, _)| i)
        .min_by_key(|i| i.abs_diff(middle))
        .map(|i| i + 1)
        .unwrap_or(middle)
}

fn help(frame: &mut Frame, area: Rect, app: &App) {
    let rows = [
        ("space", "play / pause"),
        ("n / b", "next / previous"),
        ("s", "shuffle"),
        ("r", "cycle repeat"),
        ("v", "cycle the stage"),
        ("- / =", "volume"),
        ("< / >, \u{21e7}\u{2190}\u{2192}", "seek"),
        ("", ""),
        ("l", "liked songs"),
        ("a", "albums"),
        ("p", "playlists"),
        ("Q", "queue (q quits)"),
        ("d", "devices"),
        ("/", "search"),
        ("A", "add a playlist from a link"),
        ("Tab", "open/close browser"),
        ("", ""),
        ("Enter", "play the list here"),
        ("t", "play just this track"),
        ("e", "queue this track"),
        ("E", "queue the whole list"),
        (".", "save / unsave"),
        ("", ""),
        ("j / k, \u{2193} \u{2191}", "move"),
        ("g / G", "top / bottom"),
        ("Ctrl-u/d", "page"),
        ("Backspace", "leave a playlist"),
        ("", ""),
        ("R", "refresh"),
        ("Esc", "dismiss / back out"),
        ("q, Ctrl-c", "quit"),
    ];

    // The daemon is a separate process with its own lifetime, so its build
    // is a different fact from ours and both belong here.
    let daemon = match (&app.daemon_version, app.connected_to_daemon) {
        (Some(v), _) => v.clone(),
        (None, true) => "connected, build unknown".to_string(),
        (None, false) => "not in use (direct to the Web API)".to_string(),
    };
    let build = [
        (String::new(), String::new()),
        ("boombox".to_string(), boombox_core::build_info::long()),
        ("daemon".to_string(), daemon),
    ];

    /// Widths inside one column: the key, then its description.
    const KEY: usize = 15;
    const LABEL: usize = 21;
    /// One column, including the two spaces of indent in front of it.
    const COLUMN: usize = 2 + KEY + 1 + LABEL;

    // Two columns, because one stopped fitting. At 29 rows plus the build
    // lines this wanted 34 of them, so an ordinary 24-row terminal lost the
    // bottom third -- silently, since a clipped popup still looks like a
    // popup. Side by side it needs 20.
    let split = split_point(&rows);
    let (left, right) = rows.split_at(split);

    let cell = |row: Option<&(&str, &str)>| match row {
        Some((key, label)) if !key.is_empty() => vec![
            Span::styled(format!("  {key:<KEY$} "), Style::new().fg(ACCENT)),
            Span::raw(format!("{label:<LABEL$}")),
        ],
        _ => vec![Span::raw(" ".repeat(COLUMN))],
    };

    let body: Vec<Line> = (0..left.len().max(right.len()))
        .map(|i| {
            let mut spans = cell(left.get(i));
            spans.extend(cell(right.get(i)));
            Line::from(spans)
        })
        .collect();

    let width = (COLUMN * 2 + 2) as u16;
    let height = (body.len() + build.len() + 2) as u16;
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    };

    let lines: Vec<Line> = body
        .into_iter()
        .chain(build.into_iter().map(|(key, label)| {
            Line::from(vec![
                Span::styled(format!("  {key:<KEY$} "), Style::new().fg(ACCENT)),
                // Dimmed so the build stamp reads as reference rather than
                // as another key binding.
                Span::styled(label, Style::new().fg(DIM)),
            ])
        }))
        .collect();

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(ACCENT))
                .title(" Keys ".bold()),
        ),
        popup,
    );
}

/// The paged list shared by Liked Songs, Albums, Playlists, a playlist's
/// contents and search results.
/// Width of the row number and the gap after it.
const INDEX_COLUMN: usize = 6;

/// One drawn line: either an entry, or a heading introducing a run of them.
enum Row<'a> {
    Heading(String),
    Item(usize, &'a Entry),
}

/// Inserts a heading wherever the kind changes.
///
/// Search returns tracks, albums, artists and playlists concatenated, and
/// without headings an artist row is indistinguishable from a track that
/// happens to have no duration. Only for mixed lists: a "Tracks" heading
/// over Liked Songs would be noise, since everything in it is a track.
fn rows_with_headings(entries: &[Entry]) -> Vec<Row<'_>> {
    let mixed = entries.windows(2).any(|w| w[0].kind != w[1].kind);
    let mut rows = Vec::with_capacity(entries.len() + 4);
    let mut previous = None;
    for (i, entry) in entries.iter().enumerate() {
        if mixed && previous != Some(entry.kind) {
            // How many of this kind follow, so the heading carries a count
            // that is true of what is actually on screen.
            let run = entries[i..].iter().take_while(|o| o.kind == entry.kind).count();
            rows.push(Row::Heading(format!("{} ({run})", entry.kind.plural())));
            previous = Some(entry.kind);
        }
        rows.push(Row::Item(i, entry));
    }
    rows
}

fn entry_list(frame: &mut Frame, area: Rect, app: &App, title: &str) {
    let focused = app.focus == Focus::Main;
    // Search results count what is loaded, not Spotify's total: that total
    // is four unrelated counts added together, and it moves between pages.
    let heading = if app.view == View::Search && !app.entries.is_empty() {
        format!("{title} ({})", app.entries.len())
    } else if app.entry_total > 0 {
        format!("{title} ({} of {})", app.entries.len(), app.entry_total)
    } else {
        title.to_string()
    };
    let block = pane(&heading, focused);

    if app.entries.is_empty() {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let message = if app.loading { "Loading\u{2026}" } else { "Nothing here." };
        frame.render_widget(
            Paragraph::new(message).style(Style::new().fg(DIM)).alignment(Alignment::Center),
            centered_v(inner, 1),
        );
        return;
    }

    // Shared title column, so the subtitles of rows that have one line up.
    let width = app.entries.iter().map(|e| e.title.chars().count()).max().unwrap_or(0).min(38);
    // What a row with nothing else to show may use instead. Artists are the
    // case: Spotify sends no genres, no followers and no popularity for
    // them under Development Mode -- not even in the full artist object --
    // so their second column can never be filled, and holding a name to 38
    // characters to line up with a column that will always be blank just
    // truncates the one thing the row does have.
    let bare_width = (area.width as usize).saturating_sub(2 + INDEX_COLUMN);
    let rows = rows_with_headings(&app.entries);

    // The cursor still counts entries, not drawn lines, so every bit of
    // selection and paging logic stays unaware of headings -- only this
    // lookup knows they exist.
    let selected = rows
        .iter()
        .position(|r| matches!(r, Row::Item(i, _) if *i == app.entry_index))
        .unwrap_or(0);

    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| match row {
            Row::Heading(label) => ListItem::new(Line::from(Span::styled(
                format!("  {label}"),
                Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
            ))),
            Row::Item(i, e) => {
                let duration =
                    e.duration_ms.map(|ms| format!("  {}", clock(ms))).unwrap_or_default();
                let mut spans = vec![Span::styled(
                    format!("{:>1$}  ", i + 1, INDEX_COLUMN - 2),
                    Style::new().fg(DIM),
                )];
                if e.subtitle.is_empty() && duration.is_empty() {
                    spans.push(Span::raw(truncate(&e.title, bare_width)));
                } else {
                    spans.push(Span::raw(format!(
                        "{:width$}",
                        truncate(&e.title, width),
                        width = width
                    )));
                    spans.push(Span::styled(
                        format!("  {}{}", truncate(&e.subtitle, 28), duration),
                        Style::new().fg(DIM),
                    ));
                }
                ListItem::new(Line::from(spans))
            }
        })
        .collect();

    let mut list_state = ListState::default();
    list_state.select(Some(selected));
    frame.render_stateful_widget(
        List::new(items).block(block).highlight_style(if focused {
            Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        }),
        area,
        &mut list_state,
    );
}

fn search(frame: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3)])
        .split(area);

    let box_style = if app.is_typing() { Style::new().fg(ACCENT) } else { Style::new().fg(DIM) };
    let cursor = if app.is_typing() { "\u{2588}" } else { "" };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" \u{276f} ", Style::new().fg(ACCENT)),
            Span::raw(app.query.clone()),
            Span::styled(cursor, Style::new().fg(ACCENT)),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(box_style)
                .title(Span::styled(" Search ", box_style)),
        ),
        rows[0],
    );

    if app.query.trim().is_empty() && app.entries.is_empty() {
        let block = pane("Results", false);
        let inner = block.inner(rows[1]);
        frame.render_widget(block, rows[1]);
        frame.render_widget(
            Paragraph::new(
                "Type a query and press Enter.\n\nSpotify caps search at 10 results per type.",
            )
            .style(Style::new().fg(DIM))
            .alignment(Alignment::Center),
            centered_v(inner, 3),
        );
        return;
    }
    entry_list(frame, rows[1], app, "Results");
}

/// Eight vertical eighths, so each cell resolves the bar height to 1/8th of a
/// row rather than jumping a whole character at a time.
const EIGHTHS: [char; 8] = [
    '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}', '\u{2588}',
];

/// A one-row seek bar drawn from the track's own amplitude. The played
/// portion is accented, the rest dim, so it reads as progress and as shape at
/// the same time.
struct WaveBar<'a> {
    envelope: &'a [f32],
    played: usize,
    width: usize,
    clock: &'a str,
}

impl ratatui::widgets::Widget for WaveBar<'_> {
    fn render(self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        if area.height == 0 || self.width == 0 {
            return;
        }
        for x in 0..self.width {
            let idx = x * self.envelope.len() / self.width;
            let value = self.envelope[idx.min(self.envelope.len() - 1)].clamp(0.0, 1.0);

            // A silent-but-played column still needs a baseline, or the bar
            // develops holes wherever the track went quiet.
            let level = (value * 7.0).round() as usize;
            let ch = if level == 0 { '\u{2581}' } else { EIGHTHS[level] };

            let style =
                if x < self.played { Style::new().fg(ACCENT) } else { Style::new().fg(DIM) };
            if let Some(cell) = buf.cell_mut((area.x + 1 + x as u16, area.y)) {
                cell.set_char(ch).set_style(style);
            }
        }
        // The clock sits after the bar, same as the plain version.
        let start = area.x + 1 + self.width as u16;
        for (i, c) in self.clock.chars().enumerate() {
            if let Some(cell) = buf.cell_mut((start + i as u16, area.y)) {
                cell.set_char(c).set_style(Style::new().fg(DIM));
            }
        }
    }
}

/// The waveform trace, on a braille canvas: 2x4 dots per cell, so a pane of
/// 80x20 draws at an effective 160x80 and the line reads as a curve rather
/// than a staircase.
fn scope(frame: &mut Frame, area: Rect, app: &App) {
    use ratatui::symbols::Marker;
    use ratatui::widgets::canvas::{Canvas, Line as CanvasLine};

    if app.waveform.len() < 2 {
        frame.render_widget(
            Paragraph::new("Waiting for audio\u{2026}")
                .style(Style::new().fg(DIM))
                .alignment(Alignment::Center),
            centered_v(area, 1),
        );
        return;
    }

    // Triggering and gain are applied where the state lives, in App.
    let points = app.waveform.clone();
    let trails: Vec<Vec<f32>> = app.scope_trails.iter().cloned().collect();
    let palette = app.palette;
    let last = (points.len() - 1) as f64;

    frame.render_widget(
        Canvas::default()
            .marker(Marker::Braille)
            .x_bounds([0.0, last])
            .y_bounds([-1.0, 1.0])
            .paint(move |ctx| {
                // Older sweeps first, so the current trace draws over them. A
                // phosphor scope does not clear between sweeps, and that
                // afterglow is most of why one looks alive: the space around
                // the trace shows where the signal has just been.
                for (age, trail) in trails.iter().enumerate() {
                    let freshness = (age + 1) as f32 / (trails.len() + 1) as f32;
                    let colour = palette.ghost(freshness);
                    ctx.layer();
                    for (i, pair) in trail.windows(2).enumerate() {
                        ctx.draw(&CanvasLine {
                            x1: i as f64,
                            y1: pair[0] as f64,
                            x2: (i + 1) as f64,
                            y2: pair[1] as f64,
                            color: colour,
                        });
                    }
                }

                // Fill from the axis to each sample. A bare outline is
                // accurate and weightless; a filled trace has body.
                //
                ctx.layer();
                for (i, value) in points.iter().enumerate() {
                    let v = *value as f64;
                    if v.abs() <= QUIET_SAMPLE {
                        continue;
                    }
                    ctx.draw(&CanvasLine {
                        x1: i as f64,
                        y1: 0.0,
                        x2: i as f64,
                        y2: v,
                        // Colour by how far the sample swings, so peaks read
                        // hot and the body stays cool. Kept to the upper part
                        // of the ramp, whose dark end would vanish.
                        color: palette.at(0.42 + 0.58 * value.abs()),
                    });
                }

                // Segments rather than points: at steep slopes, plotting only
                // the samples leaves visible gaps in the outline.
                ctx.layer();
                for (i, pair) in points.windows(2).enumerate() {
                    ctx.draw(&CanvasLine {
                        x1: i as f64,
                        y1: pair[0] as f64,
                        x2: (i + 1) as f64,
                        y2: pair[1] as f64,
                        // The bright end of the same ramp rather than plain
                        // white, so the trace belongs to the track's palette
                        // like everything else does.
                        color: palette.highlight(),
                    });
                }
            }),
        area,
    );
}

/// Samples smaller than this are not filled at all.
///
/// The fill runs through the axis rather than stopping short of it, so a loud
/// passage is a continuous shape. What made the centre stand out was silence:
/// with nothing else drawn, a few hundred fills all touching y = 0 and nothing
/// else is a bright line on an empty pane. Skipping them leaves silence blank,
/// which is what it should look like.
const QUIET_SAMPLE: f64 = 0.02;

/// Below this a cell is left undrawn, so the terminal's own background shows
/// through and the display works on a light theme.
const QUIET: f32 = 0.05;

/// Time on the horizontal axis (newest at the right), frequency on the
/// vertical (low at the bottom), magnitude as colour.
struct Waterfall<'a> {
    history: &'a std::collections::VecDeque<Vec<f32>>,
    palette: Palette,
}

impl ratatui::widgets::Widget for Waterfall<'_> {
    fn render(self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        if area.width == 0 || area.height == 0 || self.history.is_empty() {
            return;
        }
        // A half block carries two independently coloured pixels -- foreground
        // for the top half, background for the bottom -- so the same number of
        // cells resolves twice as many frequency bins.
        let pixels = area.height as usize * 2;

        for x in 0..area.width {
            // Right edge is the newest frame; run backwards from there so a
            // pane wider than the history leaves the left blank rather than
            // stretching.
            let age = (area.width - 1 - x) as usize;
            if age >= self.history.len() {
                continue;
            }
            let frame = &self.history[self.history.len() - 1 - age];
            if frame.is_empty() {
                continue;
            }
            // Scale each time column against its own loudest band. Absolute
            // scaling paints the whole low end solid, because that is where
            // music genuinely keeps its energy — true, and unreadable. Relative
            // scaling turns each moment into a ridge that moves, which is the
            // shape the eye can actually follow.
            let loudest = frame.iter().cloned().fold(0.0f32, f32::max).max(1e-6);

            for row in 0..area.height as usize {
                // Pixel indices counted from the bottom, low frequency first.
                let upper = pixels - 1 - row * 2;
                let lower = upper - 1;
                let top = contrast(bin_value(frame, upper, pixels) / loudest);
                let bottom = contrast(bin_value(frame, lower, pixels) / loudest);

                // Leave the quietest cells untouched so the terminal's own
                // background shows through and light themes still work.
                let (ch, fg, bg) = match (top >= QUIET, bottom >= QUIET) {
                    (false, false) => continue,
                    (true, true) => ('\u{2580}', self.palette.at(top), self.palette.at(bottom)),
                    (true, false) => ('\u{2580}', self.palette.at(top), Color::Reset),
                    (false, true) => ('\u{2584}', self.palette.at(bottom), Color::Reset),
                };
                if let Some(cell) = buf.cell_mut((area.x + x, area.y + row as u16)) {
                    cell.set_char(ch).set_style(Style::new().fg(fg).bg(bg));
                }
            }
        }
    }
}

/// Band value at pixel row `index` of `total`.
///
/// Takes the loudest band in range when there are more bands than rows, which
/// keeps peaks intact; interpolates only when the rows outnumber the bands, to
/// avoid flat steps. Interpolating in both directions was blurring detail the
/// waterfall depends on.
fn bin_value(frame: &[f32], index: usize, total: usize) -> f32 {
    if frame.is_empty() || total == 0 {
        return 0.0;
    }
    if frame.len() >= total {
        let lo = index * frame.len() / total;
        let hi = ((index + 1) * frame.len() / total).max(lo + 1).min(frame.len());
        return frame[lo..hi].iter().cloned().fold(0.0f32, f32::max);
    }
    if frame.len() < 2 || total < 2 {
        return frame[0];
    }
    let scaled = index as f32 / (total - 1) as f32 * (frame.len() - 1) as f32;
    let lower = scaled.floor() as usize;
    let upper = (lower + 1).min(frame.len() - 1);
    let blend = scaled - lower as f32;
    frame[lower] * (1.0 - blend) + frame[upper] * blend
}

// --- Waterfall look. These two are taste, not correctness: BLACK_POINT sets
// how much of each column is treated as silence, CONTRAST_GAMMA how sharply
// what remains separates. Raise BLACK_POINT for a sparser, more skeletal
// display; lower it for a fuller one. ---

/// Black point for the waterfall, applied after each column is scaled against
/// its own peak: bands below this fraction of the loudest are silence.
///
/// The bands arrive normalised against a -70dB floor, which is a very wide
/// range — generous for a bar chart, where a short bar is still legible, and
/// wrong for a heat map, where every cell with any energy at all gets painted
/// and the result is fog.
const BLACK_POINT: f32 = 0.30;

/// Levels adjustment: drop the bottom of the range entirely, then stretch what
/// is left and lift the shoulder so peaks separate from the body.
const CONTRAST_GAMMA: f32 = 1.4;

fn contrast(value: f32) -> f32 {
    let v = value.clamp(0.0, 1.0);
    ((v - BLACK_POINT) / (1.0 - BLACK_POINT)).clamp(0.0, 1.0).powf(CONTRAST_GAMMA)
}

struct Bars<'a> {
    bands: &'a [f32],
    peaks: &'a [f32],
    palette: Palette,
}

impl ratatui::widgets::Widget for Bars<'_> {
    fn render(self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        if area.width == 0 || area.height == 0 || self.bands.is_empty() {
            return;
        }
        let rows = area.height as usize;

        for x in 0..area.width {
            // Map this column onto a band; more columns than bands simply
            // widens each bar rather than leaving gaps.
            let idx = x as usize * self.bands.len() / area.width as usize;
            let value = self.bands[idx.min(self.bands.len() - 1)].clamp(0.0, 1.0);

            let eighths = (value * (rows * 8) as f32).round() as usize;
            let full = eighths / 8;
            let remainder = eighths % 8;

            // The falling marker sits one row above the bar's own top.
            let peak = self.peaks.get(idx).copied().unwrap_or(0.0).clamp(0.0, 1.0);
            let peak_row = if peak > 0.0 {
                let cells = ((peak * rows as f32).ceil() as usize).clamp(1, rows);
                Some(rows - cells)
            } else {
                None
            };

            for row in 0..rows {
                // Row 0 is the top of the pane; bars grow upward from the bottom.
                let from_bottom = rows - 1 - row;
                let ch = if from_bottom < full {
                    EIGHTHS[7]
                } else if from_bottom == full && remainder > 0 {
                    EIGHTHS[remainder - 1]
                } else if peak_row == Some(row) {
                    // Only draw the cap where the bar itself is not.
                    '\u{2594}'
                } else {
                    continue;
                };
                let style = if ch == '\u{2594}' {
                    Style::new().fg(DIM)
                } else {
                    // Gradient along the bar's height rather than keyed to
                    // its total, so a tall bar shows the whole ramp and
                    // level reads from colour as well as from length.
                    Style::new()
                        .fg(self.palette.at(from_bottom as f32 / (rows.max(1) - 1).max(1) as f32))
                };
                if let Some(cell) = buf.cell_mut((area.x + x, area.y + row as u16)) {
                    cell.set_char(ch).set_style(style);
                }
            }
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

/// Vertically centres a `height`-tall region inside `area`.
/// The box for adding a playlist from a link.
///
/// Modal and centred rather than a line inside the list, because it is the
/// one place in the app where the terminal's own paste is the input
/// method, and a prompt that looks like a row invites typing into the
/// wrong thing.
fn link_prompt(frame: &mut Frame, area: Rect, buffer: &str) {
    const HEIGHT: u16 = 4;
    let width = area.width.saturating_sub(8).clamp(24, 72);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + area.height.saturating_sub(HEIGHT) / 3;
    let popup = Rect { x, y, width, height: HEIGHT };

    // The end of the link, not the start: every Spotify link opens with
    // the same thirty characters, so a long one scrolled from the left
    // shows nothing that tells them apart.
    let room = usize::from(width.saturating_sub(5));
    let shown: String = match buffer.chars().count() > room {
        true => buffer.chars().skip(buffer.chars().count() - room).collect(),
        false => buffer.to_string(),
    };

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(" \u{276f} ", Style::new().fg(ACCENT)),
                Span::raw(shown),
                Span::styled("\u{2588}", Style::new().fg(ACCENT)),
            ]),
            // Says that Enter plays as well as adds. "Add" alone would
            // be a quiet overpromise, and the surprise would land as
            // music starting when none was asked for.
            Line::from(Span::styled(
                "   Paste a Spotify link \u{b7} \u{21b5} add and play \u{b7} \u{238b} cancel",
                Style::new().fg(DIM),
            )),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(ACCENT))
                .title(Span::styled(" Add a playlist ", Style::new().fg(ACCENT))),
        ),
        popup,
    );
}

fn centered_v(area: Rect, height: u16) -> Rect {
    let height = height.min(area.height);
    Rect {
        x: area.x + 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width: area.width.saturating_sub(4),
        height,
    }
}

fn bar(fraction: f64, width: usize) -> String {
    let filled = (fraction.clamp(0.0, 1.0) * width as f64).round() as usize;
    (0..width).map(|i| if i < filled { '\u{2501}' } else { '\u{2500}' }).collect()
}

fn clock(ms: u64) -> String {
    let total = ms / 1000;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders the help popup and returns it as plain text.
    fn rendered_help(app: &App) -> String {
        rendered_help_at(app, 80, 40)
    }

    fn rendered_help_at(app: &App, w: u16, h: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| help(frame, frame.area(), app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(w as usize)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render(app: &App, w: u16, h: u16) -> String {
        let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        t.draw(|f| {
            draw(f, app);
        })
        .unwrap();
        t.backend()
            .buffer()
            .content()
            .chunks(w as usize)
            .map(|r| r.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn playing_app() -> App {
        let mut app = App::new(5, true);
        app.set_playback(Some(crate::app::tests::playing_state(62, false, true)));
        app
    }

    /// The bar is the one thing that never moves. Whatever the stage is
    /// doing, the clock and the progress line stay where the eye expects.
    /// The legend is the whole point of the mnemonic scheme: the key is
    /// only easy to remember if it is visible next to the word.
    fn of_kind(title: &str, kind: boombox_core::api::EntryKind) -> Entry {
        Entry {
            title: title.into(),
            subtitle: String::new(),
            uri: Some(format!("spotify:x:{title}")),
            duration_ms: None,
            kind,
        }
    }

    fn artist(name: &str) -> Entry {
        Entry {
            title: name.into(),
            // Empty, and permanently so: Spotify sends no genres,
            // followers or popularity for an artist under Development Mode.
            subtitle: String::new(),
            uri: Some("spotify:artist:x".into()),
            duration_ms: None,
            kind: boombox_core::api::EntryKind::Artist,
        }
    }

    fn list_text(entries: Vec<Entry>, w: u16, h: u16) -> String {
        let mut app = playing_app();
        app.view = crate::app::View::Search;
        let total = entries.len() as u32;
        app.set_entries(entries, total, false);
        app.browse_open = true;
        render(&app, w, h)
    }

    /// A name held to the shared title column while the columns beside it
    /// are empty loses characters for nothing. Artists are the case that
    /// cannot be fixed any other way -- there is no data to put there.
    #[test]
    fn a_row_with_nothing_beside_it_may_use_the_whole_width() {
        let long = "The Extraordinarily Long-Named Orchestra featuring Several Guests";
        let text = list_text(vec![artist(long)], 110, 20);
        assert!(text.contains(long), "the name should survive in full:\n{text}");
    }

    /// It must not come at the cost of the rows that do line up.
    #[test]
    fn rows_with_a_subtitle_keep_their_shared_column() {
        let entries = vec![
            of_kind("Aurora", boombox_core::api::EntryKind::Track),
            of_kind("A much longer track title here", boombox_core::api::EntryKind::Track),
        ];
        let mut with_subtitles = entries;
        for e in &mut with_subtitles {
            e.subtitle = "Meadow".into();
            e.duration_ms = Some(1000);
        }
        let text = list_text(with_subtitles, 110, 20);
        let columns: Vec<usize> = text.lines().filter_map(|l| l.find("Meadow")).collect();
        assert!(columns.len() >= 2, "both rows should show the subtitle:\n{text}");
        assert!(columns.windows(2).all(|w| w[0] == w[1]), "subtitles should line up: {columns:?}");
    }

    /// The reported problem: search concatenates four kinds, so an artist
    /// row cannot be told from a track that happens to have no duration.
    #[test]
    fn a_mixed_list_gets_a_heading_per_kind_with_its_count() {
        use boombox_core::api::EntryKind::{Album, Artist, Track};
        let entries = vec![
            of_kind("Aurora", Track),
            of_kind("Nimbus", Track),
            of_kind("Horizons", Album),
            of_kind("Meadow", Artist),
        ];
        let rows = rows_with_headings(&entries);
        let headings: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Heading(h) => Some(h.as_str()),
                Row::Item(..) => None,
            })
            .collect();
        assert_eq!(headings, ["Tracks (2)", "Albums (1)", "Artists (1)"]);
    }

    /// The playlists view carries two sections that are both, underneath,
    /// playlists. They are told apart by kind, so the existing heading
    /// machinery separates them with no special case.
    #[test]
    fn recents_are_headed_separately_from_your_own_playlists() {
        use boombox_core::api::EntryKind::{Playlist, Recent};
        let entries = vec![
            of_kind("Daily Mix 1", Recent),
            of_kind("Discover Weekly", Recent),
            of_kind("Mixtape", Playlist),
        ];
        let rows = rows_with_headings(&entries);
        let headings: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Heading(h) => Some(h.as_str()),
                Row::Item(..) => None,
            })
            .collect();
        assert_eq!(headings, ["Recently played (2)", "Playlists (1)"]);
    }

    /// Liked Songs is all tracks. A "Tracks" heading over it says nothing
    /// and costs a line.
    #[test]
    fn a_list_of_one_kind_gets_no_headings() {
        use boombox_core::api::EntryKind::Track;
        let entries = vec![of_kind("Aurora", Track), of_kind("Nimbus", Track)];
        let rows = rows_with_headings(&entries);
        assert!(rows.iter().all(|r| matches!(r, Row::Item(..))), "no headings wanted");
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn a_single_entry_gets_no_heading() {
        let one = [of_kind("Aurora", boombox_core::api::EntryKind::Track)];
        assert_eq!(rows_with_headings(&one).len(), 1);
    }

    #[test]
    fn an_empty_list_produces_no_rows() {
        assert!(rows_with_headings(&[]).is_empty());
    }

    /// Every entry still appears, in order, with its original index -- the
    /// headings must not renumber or drop anything.
    #[test]
    fn headings_do_not_disturb_the_entries_or_their_indices() {
        use boombox_core::api::EntryKind::{Album, Track};
        let entries = vec![of_kind("a", Track), of_kind("b", Album), of_kind("c", Album)];
        let rows = rows_with_headings(&entries);
        let items: Vec<usize> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Item(i, _) => Some(*i),
                Row::Heading(_) => None,
            })
            .collect();
        assert_eq!(items, [0, 1, 2]);
    }

    /// The cursor counts entries, not drawn lines. If the two ever get
    /// confused the highlight lands on the wrong row, or on a heading.
    #[test]
    fn the_highlight_follows_the_entry_not_the_drawn_line() {
        use boombox_core::api::EntryKind::{Album, Track};
        let mut app = playing_app();
        app.view = crate::app::View::Search;
        app.set_entries(vec![of_kind("Aurora", Track), of_kind("Horizons", Album)], 2, false);
        app.browse_open = true;
        // Entry 1 is the album, which is the fourth drawn line: heading,
        // track, heading, album.
        app.entry_index = 1;
        let rows = rows_with_headings(&app.entries);
        let selected = rows.iter().position(|r| matches!(r, Row::Item(i, _) if *i == 1)).unwrap();
        assert_eq!(selected, 3, "two headings sit above the second entry");
        assert!(matches!(rows[selected], Row::Item(1, _)));
    }

    fn width_of(spans: &[Span<'_>]) -> usize {
        spans.iter().map(|s| s.content.chars().count()).sum()
    }

    /// A number alone gives no sense of movement, and the API takes about
    /// eleven seconds to apply the change, so between the keypress and the
    /// sound there would be nothing to watch.
    #[test]
    fn the_volume_becomes_a_bar_while_a_change_is_in_flight() {
        let plain = Style::new();
        let settled = volume_field(62, false, plain, plain);
        let changing = volume_field(62, true, plain, plain);

        let text: String = changing.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains('\u{2501}'), "expected a bar: {text}");
        let text: String = settled.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains('\u{2501}'), "settled shows a number only: {text}");
        assert!(text.contains("62%"), "{text}");
    }

    /// A status row that rearranges itself every time you touch the volume
    /// would be worse than having no bar at all.
    #[test]
    fn the_volume_field_is_the_same_width_either_way() {
        let plain = Style::new();
        let settled = width_of(&volume_field(62, false, plain, plain));
        for v in [0, 7, 62, 100] {
            assert_eq!(
                width_of(&volume_field(v, true, plain, plain)),
                settled,
                "bar at {v} must match the settled width"
            );
            assert_eq!(
                width_of(&volume_field(v, false, plain, plain)),
                settled,
                "number at {v} must match too"
            );
        }
    }

    #[test]
    fn the_bar_fills_in_proportion_to_the_volume() {
        let plain = Style::new();
        let filled = |v| {
            volume_field(v, true, plain, plain)
                .iter()
                .map(|s| s.content.chars().filter(|c| *c == '\u{2501}').count())
                .sum::<usize>()
        };
        assert_eq!(filled(0), 0);
        assert_eq!(filled(100), VOLUME_BAR);
        assert!(filled(50) > 0 && filled(50) < VOLUME_BAR);
        assert!(filled(75) > filled(25));
    }

    /// End to end through the real bar: pressing a volume key turns it
    /// into a bar, and it stays a number otherwise.
    #[test]
    fn pressing_volume_switches_the_indicator_and_the_number_returns() {
        // Only the status line: the seek bar below it is drawn with the
        // same glyph, so looking at the whole screen proves nothing.
        fn status_line(app: &App) -> String {
            render(app, 104, 8)
                .lines()
                .find(|l| l.contains("[s]huffle"))
                .unwrap_or_default()
                .to_string()
        }

        let mut app = playing_app();
        assert!(!status_line(&app).contains('\u{2501}'), "starts as a number");

        app.update(crate::action::Action::VolumeUp);
        assert!(status_line(&app).contains('\u{2501}'), "a press shows the bar");

        // The API catches up and reports the new figure.
        app.expire_volume_coalesce();
        app.flush_volume();
        app.set_playback(Some(crate::app::tests::playing_state(67, false, true)));
        let line = status_line(&app);
        assert!(!line.contains('\u{2501}'), "and the number comes back: {line}");
        assert!(line.contains("67%"), "{line}");
    }

    fn checkerboard(n: usize) -> std::sync::Arc<crate::artwork::Cover> {
        let pixels = (0..n * n)
            .map(|i| {
                let (x, y) = (i % n, i / n);
                if (x / 8 + y / 8) % 2 == 0 { [220, 80, 60] } else { [30, 30, 40] }
            })
            .collect();
        std::sync::Arc::new(crate::artwork::Cover::from_pixels(n as u32, pixels))
    }

    /// Half blocks put two pixels in a cell, and cells are about twice as
    /// tall as wide, so a square cover needs twice as many columns as rows.
    /// Getting this wrong gives a cover stretched into a letterbox.
    #[test]
    fn the_cover_area_is_square_once_the_cell_shape_is_accounted_for() {
        let area = Rect { x: 0, y: 0, width: 100, height: 20 };
        let cover = cover_area(area).expect("room for a cover");
        assert_eq!(cover.width, cover.height * 2, "{cover:?}");
    }

    const DARK: [u8; 3] = [0, 0, 0];
    const LIGHT: [u8; 3] = [255, 255, 255];

    /// Any of the glyphs a cover can be drawn with.
    fn is_block(symbol: &str) -> bool {
        symbol.chars().next().is_some_and(|c| QUADRANTS.contains(&c) && c != ' ')
    }

    fn has_cover(text: &str) -> bool {
        text.chars().any(|c| QUADRANTS.contains(&c) && c != ' ')
    }

    /// What each of the four quadrants ends up showing.
    fn shown(quads: [[u8; 3]; 4]) -> [[u8; 3]; 4] {
        let (ch, fg, bg) = quadrant(quads);
        let mask = QUADRANTS.iter().position(|q| *q == ch).expect("a known glyph");
        let rgb = |c: Color| match c {
            Color::Rgb(r, g, b) => [r, g, b],
            other => panic!("expected rgb, got {other:?}"),
        };
        std::array::from_fn(|i| if mask >> i & 1 == 1 { rgb(fg) } else { rgb(bg) })
    }

    /// The bitmask has to line up with the glyph, or the cover comes out
    /// scrambled in a way that still looks like a picture.
    ///
    /// Asserted on what is drawn rather than on which glyph is chosen:
    /// every mask has a complement that paints exactly the same cell with
    /// the two colours swapped, so demanding a particular one of the pair
    /// would be testing an arbitrary choice rather than the result.
    #[test]
    fn each_arrangement_of_quadrants_draws_the_right_corners() {
        let cases = [
            [LIGHT, DARK, DARK, DARK],
            [DARK, LIGHT, DARK, DARK],
            [DARK, DARK, LIGHT, DARK],
            [DARK, DARK, DARK, LIGHT],
            [LIGHT, LIGHT, DARK, DARK],
            [DARK, DARK, LIGHT, LIGHT],
            [LIGHT, DARK, LIGHT, DARK],
            [DARK, LIGHT, DARK, LIGHT],
            [LIGHT, DARK, DARK, LIGHT],
            [DARK, LIGHT, LIGHT, DARK],
            [LIGHT, LIGHT, LIGHT, DARK],
            [LIGHT, LIGHT, DARK, LIGHT],
            [LIGHT, DARK, LIGHT, LIGHT],
            [DARK, LIGHT, LIGHT, LIGHT],
        ];
        for quads in cases {
            assert_eq!(shown(quads), quads, "two colours must be reproduced exactly");
        }
    }

    /// A cell with nothing to distinguish must not have an edge invented
    /// in it -- splitting noise would add detail that is not in the image.
    #[test]
    fn a_flat_cell_is_drawn_solid() {
        let (ch, fg, bg) = quadrant([[80, 90, 100]; 4]);
        assert_eq!(ch, '\u{2588}');
        assert_eq!(fg, Color::Rgb(80, 90, 100));
        assert_eq!(fg, bg);
    }

    /// The point of the change: detail finer than one cell across now
    /// survives, where half blocks could only take one sample per cell
    /// width and flattened it.
    #[test]
    fn detail_narrower_than_a_cell_survives() {
        // Vertical stripes one half-cell wide at the rendered size.
        let n = 64usize;
        let cover = std::sync::Arc::new(crate::artwork::Cover::from_pixels(
            n as u32,
            (0..n * n).map(|i| if (i % n).is_multiple_of(2) { DARK } else { LIGHT }).collect(),
        ));
        let mut app = playing_app();
        app.set_cover(Some("test".into()), Some(cover));

        let backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();

        // Left/right split glyphs are the ones that can hold a vertical
        // edge inside a single cell. Half blocks have no such glyph.
        let held: usize = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .filter(|c| matches!(c.symbol(), "\u{258c}" | "\u{2590}" | "\u{259a}" | "\u{259e}"))
            .count();
        assert!(held > 0, "vertical detail inside a cell should survive");
    }

    /// Quadrants place more detail than half blocks, but more detail is
    /// not automatically a better picture -- a bad two-colour split can
    /// look busier while being less faithful. This measures it: rebuild
    /// what each method actually puts on screen and compare both against
    /// the image they were drawn from.
    #[test]
    fn quadrants_reproduce_the_image_more_faithfully_than_half_blocks() {
        let n = 96usize;
        // Something photo-like: smooth gradients crossed by hard edges.
        let src: Vec<[u8; 3]> = (0..n * n)
            .map(|i| {
                let (x, y) = ((i % n) as f32, (i / n) as f32);
                let base = (x / n as f32 * 200.0) as u8;
                let ring = if ((x - 48.0).powi(2) + (y - 48.0).powi(2)).sqrt() % 11.0 < 5.0 {
                    55
                } else {
                    0
                };
                [base.saturating_add(ring), (y / n as f32 * 180.0) as u8, 120]
            })
            .collect();
        let cover = crate::artwork::Cover::from_pixels(n as u32, src);

        // Rebuild both renderings at two sub-pixels per cell in each
        // direction, so they are compared on the same grid.
        let (cols, rows) = (24usize, 12usize);
        let mut quad_err = 0f64;
        let mut half_err = 0f64;
        let mut count = 0f64;
        for y in 0..rows {
            for x in 0..cols {
                let (u0, u2) = (x as f32 / cols as f32, (x + 1) as f32 / cols as f32);
                let (v0, v2) = (y as f32 / rows as f32, (y + 1) as f32 / rows as f32);
                let (u1, v1) = ((u0 + u2) / 2.0, (v0 + v2) / 2.0);
                let truth = [
                    cover.sample_area(u0, u1, v0, v1),
                    cover.sample_area(u1, u2, v0, v1),
                    cover.sample_area(u0, u1, v1, v2),
                    cover.sample_area(u1, u2, v1, v2),
                ];

                // What quadrants put on screen.
                let (ch, fg, bg) = quadrant(truth);
                let mask = QUADRANTS.iter().position(|q| *q == ch).unwrap();
                let shown = |i: usize| if mask >> i & 1 == 1 { fg } else { bg };

                // What half blocks put there: one colour per half, so both
                // sub-pixels in a half share it.
                let top = cover.sample_area(u0, u2, v0, v1);
                let bottom = cover.sample_area(u0, u2, v1, v2);

                for (i, expected) in truth.iter().enumerate() {
                    let q = match shown(i) {
                        Color::Rgb(r, g, b) => [r, g, b],
                        _ => [0, 0, 0],
                    };
                    let h = if i < 2 { top } else { bottom };
                    for c in 0..3 {
                        quad_err += (f64::from(q[c]) - f64::from(expected[c])).abs();
                        half_err += (f64::from(h[c]) - f64::from(expected[c])).abs();
                        count += 1.0;
                    }
                }
            }
        }
        let (quad, half) = (quad_err / count, half_err / count);
        assert!(quad < half, "quadrants should be closer to the image: {quad:.2} vs {half:.2}");
        eprintln!("  mean error per channel -- quadrants {quad:.2}, half blocks {half:.2}");
    }

    /// Filling the stage made the cover the loudest thing on screen and
    /// showed off the coarseness rather than the picture.
    #[test]
    fn the_cover_leaves_the_stage_room_to_breathe() {
        let area = Rect { x: 0, y: 0, width: 100, height: 19 };
        let cover = cover_area(area).expect("room for a cover");
        assert!(cover.height < area.height, "must not fill the height");
        assert!(
            cover.height * 2 > area.height,
            "but should still be the larger part: {cover:?} in {area:?}"
        );
    }

    /// Past a point a cover is merely large rather than better -- the
    /// source is only 128 pixels.
    #[test]
    fn a_huge_terminal_does_not_get_a_huge_cover() {
        let cover = cover_area(Rect { x: 0, y: 0, width: 400, height: 120 }).unwrap();
        assert!(cover.height <= 20, "{cover:?}");
    }

    /// Below a certain size a cover is a smudge, and the text it crowds
    /// out is more use than the picture.
    #[test]
    fn a_cramped_stage_gets_no_cover_at_all() {
        assert!(cover_area(Rect { x: 0, y: 0, width: 100, height: 8 }).is_none(), "too short");
        // A common terminal height must keep its cover, small though it is.
        assert!(cover_area(Rect { x: 0, y: 0, width: 100, height: 15 }).is_some(), "20-row term");
        assert!(cover_area(Rect { x: 0, y: 0, width: 20, height: 20 }).is_none(), "too narrow");
        assert!(cover_area(Rect { x: 0, y: 0, width: 0, height: 0 }).is_none());
    }

    /// The cover has to actually be drawn from the image: a bug in the
    /// sampling would give a flat rectangle, which still looks deliberate.
    #[test]
    fn the_cover_is_drawn_with_the_colours_of_the_image() {
        let mut app = playing_app();
        app.set_cover(Some("test".into()), Some(checkerboard(128)));

        let backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();

        let colours: std::collections::HashSet<_> = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .filter(|c| is_block(c.symbol()))
            .map(|c| (c.fg, c.bg))
            .collect();
        assert!(colours.len() > 1, "a checkerboard must not render as one flat colour");
        assert!(
            colours.iter().any(|(fg, _)| *fg == Color::Rgb(220, 80, 60)),
            "the image colours should reach the screen: {colours:?}"
        );
    }

    /// Without a cover the card is what it always was, centred.
    #[test]
    fn the_card_falls_back_to_centred_text_with_no_cover() {
        let app = playing_app();
        let text = render(&app, 100, 24);
        assert!(!has_cover(&text), "no cover drawn");
        assert!(text.contains('x'), "the track is still named");
    }

    /// A cover arriving must not shove the track text off the screen.
    #[test]
    fn the_track_is_still_named_beside_the_cover() {
        let mut app = playing_app();
        app.set_cover(Some("test".into()), Some(checkerboard(128)));
        let text = render(&app, 100, 24);
        assert!(has_cover(&text), "cover drawn");
        assert!(text.contains('x'), "and the track still named");
    }

    fn queue_of(n: usize) -> App {
        let mut app = playing_app();
        app.view = crate::app::View::Queue;
        app.set_queue(
            (0..n)
                .map(|i| {
                    serde_json::from_str::<boombox_core::api::PlayingItem>(&format!(
                        r#"{{"type":"track","name":"row {i}","uri":"spotify:track:{i}",
                             "duration_ms":1000,"artists":[{{"name":"a"}}],
                             "album":{{"name":"b"}}}}"#
                    ))
                    .unwrap()
                })
                .collect(),
        );
        app.browse_open = true;
        app
    }

    fn popup_height(app: &App, w: u16, h: u16) -> usize {
        render(app, w, h)
            .lines()
            .filter(|l| l.contains('\u{2502}') || l.contains('\u{256d}') || l.contains('\u{2570}'))
            .count()
    }

    /// The box was a flat proportion of the screen whatever was in it, so
    /// a short queue sat in a mostly empty frame.
    #[test]
    fn the_palette_is_as_tall_as_its_contents() {
        for rows in [1usize, 2, 6, 9] {
            assert_eq!(
                popup_height(&queue_of(rows), 84, 26),
                // The now-playing row, the upcoming ones, and two borders.
                rows + 3,
                "{rows} rows plus the current track plus two borders"
            );
        }
    }

    /// An empty list still says so, on one line.
    #[test]
    fn an_empty_palette_is_three_lines() {
        assert_eq!(popup_height(&queue_of(0), 84, 26), 3);
    }

    /// A list longer than the screen stops at the screen.
    #[test]
    fn a_long_list_is_capped_rather_than_overflowing() {
        let tall = popup_height(&queue_of(500), 84, 26);
        assert!(tall < 26, "must fit the terminal, got {tall}");
        assert!(tall > 10, "and should still use most of it, got {tall}");
    }

    /// Sizing to a "Loading..." line would open the box small and jump it
    /// wide a moment later.
    #[test]
    fn a_loading_palette_takes_the_full_height_rather_than_flapping() {
        let mut app = queue_of(0);
        app.view = crate::app::View::Liked;
        app.loading = true;
        let loading = popup_height(&app, 84, 26);
        assert!(loading > 10, "should open at full height, got {loading}");
    }

    #[test]
    fn the_legend_shows_each_key_inside_its_own_word() {
        let text = render(&playing_app(), 110, 12);
        for expected in ["[l]iked", "[a]lbums", "[p]laylists", "[Q]ueue", "[d]evices"] {
            assert!(text.contains(expected), "missing {expected} in:\n{text}");
        }
    }

    /// Different keys apply with the browser open, and showing the closed
    /// set there would be actively misleading.
    #[test]
    fn the_legend_follows_the_context() {
        let mut app = playing_app();
        app.view = crate::app::View::Liked;
        app.set_entries(vec![crate::app::tests::entry("Aurora")], 898, false);
        app.browse_open = true;
        let text = render(&app, 110, 12);
        assert!(text.contains("[e]nqueue"), "row actions apply here: {text}");
        assert!(!text.contains("[a]lbums"), "and the closed set does not");
    }

    /// The only route to a Daily Mix, so it has to be visible in the view
    /// where the added playlist appears -- and only there, where it means
    /// something.
    #[test]
    fn adding_a_playlist_is_offered_in_the_playlists_view_only() {
        let mut app = playing_app();
        app.browse_open = true;
        app.set_entries(vec![crate::app::tests::entry("Aurora")], 10, false);

        app.view = crate::app::View::Playlists;
        let text = render(&app, 110, 12);
        assert!(text.contains("[A]dd playlist"), "{text}");

        app.view = crate::app::View::Liked;
        let text = render(&app, 110, 12);
        assert!(!text.contains("[A]dd playlist"), "nothing to add to here: {text}");
    }

    /// "Add" on its own would be a quiet overpromise: Enter also starts
    /// playing, and music beginning unasked is a bad surprise.
    #[test]
    fn the_prompt_says_that_it_plays_as_well_as_adds() {
        let mut app = playing_app();
        app.view = crate::app::View::Playlists;
        app.browse_open = true;
        app.update(crate::action::Action::AddPlaylist);

        let text = render(&app, 110, 16);
        assert!(text.contains("Add a playlist"), "{text}");
        assert!(text.contains("add and play"), "{text}");
    }

    /// The one hint shown exactly when someone is stuck. It named `5` for
    /// a long while after the numbered keys were gone.
    #[test]
    fn the_empty_player_names_a_key_that_exists() {
        let text = render(&App::new(5, false), 110, 20);
        assert!(text.contains("press d to pick a device"), "{text}");
    }

    /// Inside a playlist nothing said where it was opened from, and the way
    /// back was only in the help screen.
    #[test]
    fn inside_a_playlist_the_title_says_where_it_came_from_and_the_legend_how_back() {
        let mut app = playing_app();
        app.update(crate::action::Action::OpenPlaylists);
        app.set_entries(
            vec![boombox_core::api::Entry {
                title: "Mixtape".into(),
                subtitle: "Alex".into(),
                uri: Some("spotify:playlist:1ExamplePlaylistId0001".into()),
                duration_ms: None,
                kind: boombox_core::api::EntryKind::Playlist,
            }],
            1,
            false,
        );
        app.update(crate::action::Action::Select);
        app.set_entries(vec![crate::app::tests::entry("Aurora")], 1, false);

        let text = render(&app, 110, 20);
        assert!(text.contains("Playlists \u{203a} Mixtape"), "{text}");
        assert!(text.contains("[\u{232b}] back"), "{text}");
    }

    /// Spotify's search total is four unrelated counts added together, and it
    /// moves between pages -- "35 of 80", then "74 of 202".
    #[test]
    fn search_results_are_counted_without_the_summed_total() {
        let mut app = playing_app();
        app.update(crate::action::Action::OpenSearch);
        app.set_entries(vec![crate::app::tests::entry("Aurora")], 800, false);
        let text = render(&app, 110, 20);
        assert!(text.contains("Results (1)"), "{text}");
        assert!(!text.contains("of 800"), "{text}");
    }

    /// The status row doubles as the legend for the keys that change it,
    /// so there is no second place to look up how to turn shuffle on.
    #[test]
    fn the_status_row_names_the_keys_that_change_it() {
        let text = render(&playing_app(), 110, 12);
        assert!(text.contains("[s]huffle"), "{text}");
        assert!(text.contains("[r]epeat"), "{text}");
    }

    /// The dot at the end of the status row is the only sign of whether a
    /// daemon is attached, so a device name beside it must not push it off.
    #[test]
    fn the_status_row_keeps_the_daemon_dot_after_the_device() {
        for (connected, dot) in [(true, '\u{25cf}'), (false, '\u{25cb}')] {
            let mut app = App::new(5, connected);
            app.set_playback(Some(crate::app::tests::playing_state(62, false, true)));
            for width in [80, 104] {
                let text = render(&app, width, 8);
                let line = text.lines().find(|l| l.contains('\u{21c4}') || l.contains("[s]huffle"));
                let line = line.unwrap_or_default();
                assert!(line.contains('\u{25b8}'), "the device is shown at {width}: {line}");
                assert!(line.trim_end().ends_with(dot), "and the dot after it at {width}: {line}");
            }
        }
    }

    /// Narrow terminals lose the words rather than the state.
    #[test]
    fn a_narrow_terminal_falls_back_to_icons() {
        let text = render(&playing_app(), 80, 8);
        assert!(!text.contains("[s]huffle"), "no room for words: {text}");
        assert!(text.contains("\u{21c4}"), "but the state is still shown: {text}");
    }

    /// A toast replaces the legend for its four seconds rather than
    /// covering the stage.
    #[test]
    fn a_toast_takes_the_legends_line() {
        let mut app = playing_app();
        app.info("added to queue");
        let text = render(&app, 110, 12);
        assert!(text.contains("added to queue"), "{text}");
        assert!(!text.contains("[l]iked"), "the legend stands aside: {text}");
    }

    #[test]
    fn the_player_bar_is_pinned_across_every_stage() {
        let mut app = playing_app();
        assert!(render(&app, 72, 16).contains("1:00"), "stage: now playing");

        app.cycle_visual();
        app.set_spectrum(vec![0.5; 64]);
        assert!(render(&app, 72, 16).contains("1:00"), "stage: visualising");

        app.browse_open = true;
        app.view = crate::app::View::Liked;
        app.set_entries(vec![crate::app::tests::entry("Aurora")], 898, false);
        let text = render(&app, 72, 16);
        assert!(text.contains("1:00"), "and behind the browser");
        assert!(text.contains("Aurora"), "which is drawn over the stage");
    }

    /// Browsing is a layer, so closing it gives the whole screen back
    /// rather than collapsing a pane.
    #[test]
    fn closing_the_browser_leaves_nothing_behind() {
        let mut app = playing_app();
        app.view = crate::app::View::Liked;
        app.set_entries(vec![crate::app::tests::entry("Aurora")], 898, false);
        app.browse_open = true;
        assert!(render(&app, 72, 16).contains("Aurora"));
        app.browse_open = false;
        assert!(!render(&app, 72, 16).contains("Aurora"));
    }

    #[test]
    fn idle_keeps_the_clock_and_the_progress_bar_and_drops_the_rest() {
        let mut app = playing_app();
        app.cycle_visual();
        app.set_spectrum(vec![0.5; 64]);
        app.last_input = std::time::Instant::now() - std::time::Duration::from_secs(30);
        assert!(app.is_idle());

        let text = render(&app, 72, 16);
        assert_eq!(text.matches("1:00").count(), 1, "exactly one clock, not two: {text}");
        assert!(text.contains("\u{2501}"), "and the progress bar: {text}");
        assert!(!text.contains("\u{21c4}"), "but the shuffle/repeat row goes: {text}");
    }

    /// An empty byline must not leave a separator pointing at nothing.
    #[test]
    fn the_idle_line_omits_the_separator_when_there_is_no_byline() {
        let mut app = playing_app();
        app.cycle_visual();
        app.set_spectrum(vec![0.5; 64]);
        app.last_input = std::time::Instant::now() - std::time::Duration::from_secs(30);
        assert!(!render(&app, 72, 16).contains("\u{b7}  \n"), "dangling separator");
    }

    #[test]
    fn help_names_the_build_this_binary_was_made_from() {
        let app = App::new(5, false);
        let text = rendered_help(&app);
        assert!(text.contains(boombox_core::build_info::VERSION), "{text}");
        if !boombox_core::build_info::COMMIT.is_empty() {
            assert!(text.contains(boombox_core::build_info::COMMIT), "{text}");
        }
    }

    /// The daemon is a separate process that outlives the binary which
    /// started it, so its build is the one that actually answers requests.
    #[test]
    fn help_names_the_daemon_build_separately() {
        let mut app = App::new(5, true);
        app.daemon_version = Some("9.9.9 (deadbeef)".into());
        let text = rendered_help(&app);
        assert!(text.contains("daemon"), "{text}");
        assert!(text.contains("9.9.9 (deadbeef)"), "{text}");
    }

    #[test]
    fn help_says_when_no_daemon_is_in_use() {
        let text = rendered_help(&App::new(5, false));
        assert!(text.contains("not in use"), "{text}");
    }

    /// A daemon predating version reporting still connects, and saying
    /// "unknown" is more honest than showing our own build in its place.
    #[test]
    fn help_admits_when_the_daemon_build_is_unknown() {
        let text = rendered_help(&App::new(5, true));
        assert!(text.contains("build unknown"), "{text}");
    }

    #[test]
    fn help_still_lists_the_key_bindings() {
        let text = rendered_help(&App::new(5, false));
        assert!(text.contains("play / pause"), "{text}");
        assert!(text.contains("cycle repeat"), "{text}");
    }

    /// The help grew past the height of an ordinary terminal and started
    /// losing its bottom third -- silently, because a clipped popup still
    /// looks like a popup. It has to fit the smallest screen worth
    /// supporting, and the last row is the proof.
    #[test]
    fn the_help_fits_an_eighty_by_twenty_four_terminal() {
        let app = playing_app();
        let text = rendered_help_at(&app, 80, 24);
        assert!(text.contains("quit"), "the last key row is missing:\n{text}");
        assert!(text.contains("daemon"), "the build lines are missing:\n{text}");
        assert!(text.lines().any(|l| l.contains('\u{256f}')), "no bottom border:\n{text}");
    }

    /// Two columns only help if the rows actually sit side by side.
    #[test]
    fn the_keys_are_laid_out_in_two_columns() {
        let text = rendered_help_at(&playing_app(), 100, 30);
        let paired = text.lines().filter(|l| l.contains("space") && l.contains("Enter")).count();
        assert_eq!(paired, 1, "the two columns should share lines:\n{text}");
    }

    /// Splitting mid-group would put "albums" under the playback keys.
    #[test]
    fn the_split_falls_on_a_gap_between_groups() {
        let rows = [("a", "1"), ("b", "2"), ("", ""), ("c", "3"), ("d", "4")];
        assert_eq!(split_point(&rows), 3, "just past the blank");
        let none = [("a", "1"), ("b", "2"), ("c", "3")];
        assert_eq!(split_point(&none), 2, "no blank: the middle");
    }

    #[test]
    fn bar_fills_proportionally_and_keeps_its_width() {
        assert_eq!(bar(0.0, 10).chars().filter(|c| *c == '\u{2501}').count(), 0);
        assert_eq!(bar(0.5, 10).chars().filter(|c| *c == '\u{2501}').count(), 5);
        assert_eq!(bar(1.0, 10).chars().filter(|c| *c == '\u{2501}').count(), 10);
        assert_eq!(bar(0.37, 10).chars().count(), 10);
    }

    #[test]
    fn bar_survives_a_zero_width_pane() {
        assert_eq!(bar(0.5, 0), "");
    }

    #[test]
    fn contrast_discards_the_quiet_end_and_keeps_the_peak() {
        assert_eq!(contrast(0.0), 0.0);
        assert_eq!(contrast(BLACK_POINT), 0.0, "the black point is silence");
        assert!(contrast(BLACK_POINT - 0.05) == 0.0, "and everything below it");
        assert!((contrast(1.0) - 1.0).abs() < 1e-6, "the peak stays the peak");
    }

    #[test]
    fn contrast_is_monotonic_so_louder_never_draws_darker() {
        let mut previous = 0.0;
        for step in 0..=40 {
            let v = contrast(step as f32 / 40.0);
            assert!(v >= previous, "dipped at {step}: {v} < {previous}");
            previous = v;
        }
    }

    #[test]
    fn bin_value_takes_peaks_when_bands_outnumber_rows() {
        // 4 bands into 2 rows: each row must keep the louder of its pair.
        let frame = [0.1, 0.9, 0.2, 0.3];
        assert!((bin_value(&frame, 0, 2) - 0.9).abs() < 1e-6);
        assert!((bin_value(&frame, 1, 2) - 0.3).abs() < 1e-6);
    }

    #[test]
    fn bin_value_interpolates_when_rows_outnumber_bands() {
        let frame = [0.0, 1.0];
        assert!((bin_value(&frame, 2, 5) - 0.5).abs() < 1e-6, "midpoint blends");
    }

    #[test]
    fn bin_value_survives_degenerate_input() {
        assert_eq!(bin_value(&[], 0, 4), 0.0);
        assert_eq!(bin_value(&[0.7], 0, 0), 0.0);
        assert!((bin_value(&[0.7], 3, 5) - 0.7).abs() < 1e-6);
    }

    #[test]
    fn centering_never_escapes_a_tiny_area() {
        let area = Rect { x: 0, y: 0, width: 3, height: 1 };
        let c = centered_v(area, 10);
        assert!(c.height <= area.height);
        assert!(c.width <= area.width);
    }
}
