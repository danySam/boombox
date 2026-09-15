use anyhow::{Result, bail};
use boombox_core::api::{PlaybackState, PlayingItem};

/// `1:47`, or `1:02:03` once it runs past an hour.
pub fn clock(ms: u64) -> String {
    let total = ms / 1000;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// Accepts `90`, `90s`, `1:30`, `1:02:03`, and relative `+30` / `-10`.
/// Relative values are seconds. The result is clamped into the track.
pub fn parse_position(input: &str, current_ms: u64, duration_ms: u64) -> Result<u64> {
    let raw = input.trim();
    if raw.is_empty() {
        bail!("expected a position such as 1:45, 90, +30 or -10");
    }

    let (sign, rest) = match raw.as_bytes()[0] {
        b'+' => (1i64, &raw[1..]),
        b'-' => (-1i64, &raw[1..]),
        _ => (0i64, raw),
    };

    let seconds = parse_clock_seconds(rest)?;
    let target =
        if sign == 0 { seconds as i64 } else { (current_ms / 1000) as i64 + sign * seconds as i64 };

    let clamped = target.max(0) as u64 * 1000;
    Ok(if duration_ms > 0 { clamped.min(duration_ms) } else { clamped })
}

fn parse_clock_seconds(s: &str) -> Result<u64> {
    let s = s.strip_suffix('s').unwrap_or(s);
    if s.is_empty() {
        bail!("expected a number of seconds or a m:ss timestamp");
    }

    let mut total: u64 = 0;
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() > 3 {
        bail!("`{s}` has too many colons to be a timestamp");
    }
    for (i, part) in parts.iter().enumerate() {
        let value: u64 = part
            .parse()
            .map_err(|_| anyhow::anyhow!("`{s}` is not a number or a m:ss timestamp"))?;
        // Only the leading component may exceed 59 -- `90:00` is 90 minutes,
        // but `1:75` is a typo.
        if i > 0 && value >= 60 {
            bail!("`{s}` has a component of 60 or more");
        }
        total = total * 60 + value;
    }
    Ok(total)
}

/// Accepts `60`, `60%`, and relative `+10` / `-5`. Clamped to 0..=100.
pub fn parse_volume(input: &str, current: u32) -> Result<u32> {
    let raw = input.trim().strip_suffix('%').unwrap_or(input.trim());
    if raw.is_empty() {
        bail!("expected a volume such as 60, +10 or -5");
    }

    let (sign, rest) = match raw.as_bytes()[0] {
        b'+' => (1i64, &raw[1..]),
        b'-' => (-1i64, &raw[1..]),
        _ => (0i64, raw),
    };

    let value: i64 = rest.parse().map_err(|_| anyhow::anyhow!("`{input}` is not a volume"))?;
    let target = if sign == 0 { value } else { current as i64 + sign * value };
    Ok(target.clamp(0, 100) as u32)
}

pub fn progress_bar(fraction: f64, width: usize) -> String {
    let filled = (fraction.clamp(0.0, 1.0) * width as f64).round() as usize;
    let mut bar = String::with_capacity(width * 3);
    for i in 0..width {
        bar.push(if i < filled { '\u{2501}' } else { '\u{2500}' });
    }
    bar
}

pub fn status_glyph(is_playing: bool) -> &'static str {
    if is_playing { "\u{25b6}" } else { "\u{23f8}" }
}

/// The default single-line `boombox now`. One line, no padding, safe to pipe.
pub fn now_line(state: &PlaybackState) -> String {
    let item = state.item.as_ref();
    let title = item.map(PlayingItem::name).unwrap_or("nothing");
    let byline = item.map(PlayingItem::byline).unwrap_or_default();
    let head =
        if byline.is_empty() { title.to_string() } else { format!("{title} \u{b7} {byline}") };

    format!(
        "{}  {}  {}/{}",
        status_glyph(state.is_playing),
        head,
        clock(state.progress()),
        clock(state.duration())
    )
}

/// Substitutes `{key}` placeholders. Unknown keys are left as written so a
/// typo is visible rather than silently swallowed.
pub fn render_template(template: &str, state: &PlaybackState) -> String {
    let item = state.item.as_ref();
    let mut out = String::with_capacity(template.len() + 32);
    let mut rest = template;

    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let key = &after[..end];

        match placeholder(key, state, item) {
            Some(value) => out.push_str(&value),
            None => {
                out.push('{');
                out.push_str(key);
                out.push('}');
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn placeholder(key: &str, state: &PlaybackState, item: Option<&PlayingItem>) -> Option<String> {
    // {bar} and {bar:N} share a prefix, so match the parameterised form first.
    if let Some(width) = key.strip_prefix("bar:") {
        let width = width.parse().unwrap_or(20);
        return Some(progress_bar(state.fraction(), width));
    }

    Some(match key {
        "title" | "track" => item.map(PlayingItem::name).unwrap_or_default().to_string(),
        "artist" => item.map(PlayingItem::byline).unwrap_or_default(),
        "album" | "show" => item.and_then(PlayingItem::collection).unwrap_or_default().to_string(),
        "uri" => item.and_then(PlayingItem::uri).unwrap_or_default().to_string(),
        "position" | "elapsed" => clock(state.progress()),
        "duration" | "length" => clock(state.duration()),
        "remaining" => clock(state.duration().saturating_sub(state.progress())),
        "pct" => format!("{:.0}", state.fraction() * 100.0),
        "bar" => progress_bar(state.fraction(), 20),
        "status" => status_glyph(state.is_playing).to_string(),
        "state" => if state.is_playing { "playing" } else { "paused" }.to_string(),
        "device" => state.device.as_ref().map(|d| d.name.clone()).unwrap_or_default(),
        "volume" => state.volume().map(|v| v.to_string()).unwrap_or_default(),
        "shuffle" => if state.shuffle_state { "on" } else { "off" }.to_string(),
        "repeat" => state.repeat_state.to_string(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(progress_ms: u64, duration_ms: u64) -> PlaybackState {
        let raw = format!(
            r#"{{"is_playing":true,"progress_ms":{progress_ms},"shuffle_state":true,
                "repeat_state":"context",
                "device":{{"id":"d","name":"MacBook Pro","type":"Computer",
                          "volume_percent":62,"supports_volume":true}},
                "item":{{"type":"track","id":"t","name":"Glass Roads",
                        "uri":"spotify:track:t","duration_ms":{duration_ms},
                        "artists":[{{"name":"Lowtide"}}],
                        "album":{{"name":"II"}}}}}}"#
        );
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn clock_formats_minutes_and_hours() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(107_000), "1:47");
        assert_eq!(clock(252_000), "4:12");
        assert_eq!(clock(3_723_000), "1:02:03");
    }

    #[test]
    fn absolute_positions_parse() {
        assert_eq!(parse_position("90", 0, 300_000).unwrap(), 90_000);
        assert_eq!(parse_position("90s", 0, 300_000).unwrap(), 90_000);
        assert_eq!(parse_position("1:45", 0, 300_000).unwrap(), 105_000);
        assert_eq!(parse_position("1:02:03", 0, 7_200_000).unwrap(), 3_723_000);
    }

    #[test]
    fn relative_positions_apply_to_the_current_spot() {
        assert_eq!(parse_position("+30", 60_000, 300_000).unwrap(), 90_000);
        assert_eq!(parse_position("-10", 60_000, 300_000).unwrap(), 50_000);
    }

    #[test]
    fn positions_clamp_to_the_track() {
        assert_eq!(parse_position("-99", 5_000, 300_000).unwrap(), 0, "cannot go negative");
        assert_eq!(parse_position("+9999", 5_000, 300_000).unwrap(), 300_000, "cannot overshoot");
        // A live stream reports no duration; don't clamp to zero.
        assert_eq!(parse_position("+30", 0, 0).unwrap(), 30_000);
    }

    #[test]
    fn nonsense_positions_are_rejected() {
        for bad in ["", "abc", "1:2:3:4", "1:75", "+"] {
            assert!(parse_position(bad, 0, 300_000).is_err(), "`{bad}` should be rejected");
        }
    }

    #[test]
    fn volumes_parse_absolute_and_relative() {
        assert_eq!(parse_volume("60", 20).unwrap(), 60);
        assert_eq!(parse_volume("60%", 20).unwrap(), 60);
        assert_eq!(parse_volume("+10", 55).unwrap(), 65);
        assert_eq!(parse_volume("-5", 55).unwrap(), 50);
    }

    #[test]
    fn volumes_clamp_to_the_valid_range() {
        assert_eq!(parse_volume("+50", 80).unwrap(), 100);
        assert_eq!(parse_volume("-50", 10).unwrap(), 0);
        assert_eq!(parse_volume("500", 10).unwrap(), 100);
    }

    #[test]
    fn nonsense_volumes_are_rejected() {
        for bad in ["", "loud", "+"] {
            assert!(parse_volume(bad, 50).is_err(), "`{bad}` should be rejected");
        }
    }

    #[test]
    fn progress_bar_fills_proportionally() {
        assert_eq!(progress_bar(0.0, 4).chars().filter(|c| *c == '\u{2501}').count(), 0);
        assert_eq!(progress_bar(0.5, 4).chars().filter(|c| *c == '\u{2501}').count(), 2);
        assert_eq!(progress_bar(1.0, 4).chars().filter(|c| *c == '\u{2501}').count(), 4);
        assert_eq!(progress_bar(0.5, 10).chars().count(), 10);
    }

    #[test]
    fn now_line_is_one_line() {
        let line = now_line(&state(107_000, 252_000));
        assert_eq!(line, "\u{25b6}  Glass Roads \u{b7} Lowtide  1:47/4:12");
        assert!(!line.contains('\n'));
    }

    #[test]
    fn template_substitutes_known_keys() {
        let s = state(107_000, 252_000);
        assert_eq!(render_template("{artist} - {title}", &s), "Lowtide - Glass Roads");
        assert_eq!(render_template("{position}/{duration}", &s), "1:47/4:12");
        assert_eq!(render_template("{album} {device} {volume}", &s), "II MacBook Pro 62");
        assert_eq!(render_template("{shuffle} {repeat} {state}", &s), "on context playing");
        assert_eq!(render_template("{remaining} {pct}", &s), "2:25 42");
    }

    #[test]
    fn template_supports_a_sized_bar() {
        let s = state(50_000, 100_000);
        assert_eq!(render_template("{bar:10}", &s).chars().count(), 10);
        assert_eq!(render_template("{bar}", &s).chars().count(), 20);
    }

    #[test]
    fn unknown_keys_survive_verbatim_so_typos_are_visible() {
        let s = state(0, 1000);
        assert_eq!(render_template("{titel}", &s), "{titel}");
    }

    #[test]
    fn unclosed_brace_is_not_swallowed() {
        let s = state(0, 1000);
        assert_eq!(render_template("a {title", &s), "a {title");
    }

    #[test]
    fn literal_text_without_placeholders_passes_through() {
        let s = state(0, 1000);
        assert_eq!(render_template("no keys here", &s), "no keys here");
    }
}
