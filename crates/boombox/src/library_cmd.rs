use std::io::IsTerminal as _;
use std::str::FromStr as _;
use std::sync::Arc;

use anyhow::{Result, bail};
use boombox_core::api::library::{LibraryApi, SEARCH_MAX_LIMIT};
use boombox_core::api::player::PlayerApi;
use boombox_core::api::{Entry, SearchResults, SearchType};
use boombox_core::{Client, Config};
use clap::{Args, Subcommand};

#[derive(Args)]
pub struct SearchArgs {
    /// What to look for
    pub query: Vec<String>,

    /// track, album, artist, playlist, show, episode (comma separated)
    #[arg(long, default_value = "track")]
    pub types: String,

    /// Results per type. Spotify's hard cap is 10.
    #[arg(long, default_value_t = SEARCH_MAX_LIMIT)]
    pub limit: u32,

    /// Skip this many results, for paging past the cap
    #[arg(long, default_value_t = 0)]
    pub offset: u32,

    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand)]
pub enum PlaylistCommand {
    /// List your playlists
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show a playlist's tracks
    Show {
        /// Playlist name, id, or URI
        name: String,
        #[arg(long, default_value_t = 50)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
}

pub enum LibraryCommand {
    Search(SearchArgs),
    Playlist(PlaylistCommand),
    Liked { limit: u32, json: bool },
    Albums { json: bool },
    Like { uri: Option<String> },
    Unlike { uri: Option<String> },
}

pub async fn run(cmd: LibraryCommand, direct: bool) -> Result<()> {
    let config = Config::load()?;

    // Route through the daemon when there is one. Not for speed -- the daemon
    // does not cache library reads -- but so a single process owns the token.
    // Spotify rotates the refresh token, and two independent refreshers racing
    // each other will eventually invalidate the session.
    if !direct && let Some(ipc) = crate::daemon::try_connect(&config).await {
        return dispatch(&ipc, cmd).await;
    }

    let auth = Arc::new(boombox_core::Auth::from_config(&config)?);
    let client = Client::new(auth);
    dispatch(&client, cmd).await
}

async fn dispatch<A>(client: &A, cmd: LibraryCommand) -> Result<()>
where
    A: LibraryApi + PlayerApi,
{
    match cmd {
        LibraryCommand::Search(args) => search(client, args).await,
        LibraryCommand::Playlist(sub) => playlist(client, sub).await,
        LibraryCommand::Liked { limit, json } => liked(client, limit, json).await,
        LibraryCommand::Albums { json } => albums(client, json).await,
        LibraryCommand::Like { uri } => set_saved(client, uri, true).await,
        LibraryCommand::Unlike { uri } => set_saved(client, uri, false).await,
    }
}

async fn search<A: LibraryApi>(client: &A, args: SearchArgs) -> Result<()> {
    let query = args.query.join(" ");
    if query.trim().is_empty() {
        bail!("nothing to search for");
    }

    let types = args
        .types
        .split(',')
        .map(SearchType::from_str)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!(e))?;

    if args.limit > SEARCH_MAX_LIMIT {
        eprintln!(
            "boombox: Spotify caps search at {SEARCH_MAX_LIMIT} per type; asking for {}",
            SEARCH_MAX_LIMIT
        );
    }

    let results = client.search(&query, &types, args.limit, args.offset).await?;

    if args.json {
        println!("{}", serde_json::to_string(&results)?);
        return Ok(());
    }

    let sections = flatten(&results);
    if sections.iter().all(|(_, entries, _)| entries.is_empty()) {
        eprintln!("boombox: nothing found for `{query}`");
        return Ok(());
    }

    for (label, entries, total) in sections {
        if entries.is_empty() {
            continue;
        }
        let shown = args.offset + entries.len() as u32;
        println!("{label}  ({shown} of {total})");
        print_entries(&entries, args.offset);
        if shown < total {
            println!("   \u{2026} --offset {shown} for more");
        }
        println!();
    }
    Ok(())
}

/// Turns the six optional result sections into a uniform list.
fn flatten(results: &SearchResults) -> Vec<(&'static str, Vec<Entry>, u32)> {
    fn section<T, F>(page: &Option<boombox_core::api::Page<Option<T>>>, f: F) -> (Vec<Entry>, u32)
    where
        F: Fn(&T) -> Entry,
    {
        match page {
            Some(p) => (p.items.iter().flatten().map(&f).collect(), p.total),
            None => (Vec::new(), 0),
        }
    }

    let (tracks, tt) = section(&results.tracks, Entry::from_track);
    let (albums, at) = section(&results.albums, Entry::from_album);
    let (artists, rt) = section(&results.artists, Entry::from_artist);
    let (playlists, pt) = section(&results.playlists, Entry::from_playlist);
    let (shows, st) = section(&results.shows, Entry::from_show);

    vec![
        ("Tracks", tracks, tt),
        ("Albums", albums, at),
        ("Artists", artists, rt),
        ("Playlists", playlists, pt),
        ("Shows", shows, st),
    ]
}

fn print_entries(entries: &[Entry], offset: u32) {
    let width = entries.iter().map(|e| e.title.chars().count()).max().unwrap_or(0).min(40);
    for (i, e) in entries.iter().enumerate() {
        let duration =
            e.duration_ms.map(|ms| format!("  {}", crate::fmt::clock(ms))).unwrap_or_default();
        println!(
            "{:>3}  {:width$}  {}{}  {}",
            offset as usize + i + 1,
            truncate(&e.title, width),
            truncate(&e.subtitle, 30),
            duration,
            e.uri.as_deref().unwrap_or(""),
            width = width,
        );
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

async fn playlist<A: LibraryApi>(client: &A, cmd: PlaylistCommand) -> Result<()> {
    match cmd {
        PlaylistCommand::List { json } => {
            let page = client.my_playlists(50, 0).await?;
            if json {
                println!("{}", serde_json::to_string(&page)?);
                return Ok(());
            }
            if page.is_empty() {
                println!("no playlists");
                return Ok(());
            }
            let entries: Vec<Entry> = page.items.iter().map(Entry::from_playlist).collect();
            print_entries(&entries, 0);
            Ok(())
        }
        PlaylistCommand::Show { name, limit, json } => {
            let id = resolve_playlist(client, &name).await?;
            let page = client.playlist_items(&id, limit, 0).await?;
            if json {
                println!("{}", serde_json::to_string(&page)?);
                return Ok(());
            }
            let entries: Vec<Entry> = page
                .items
                .iter()
                .filter_map(|row| row.item.as_ref())
                .filter_map(Entry::from_playing_item)
                .collect();
            if entries.is_empty() {
                bail!(
                    "no readable items. Since February 2026 Spotify only serves the \
                     contents of playlists you own or collaborate on."
                );
            }
            print_entries(&entries, 0);
            if page.has_more() {
                println!("   \u{2026} {} of {} shown", entries.len(), page.total);
            }
            Ok(())
        }
    }
}

/// Accepts a URI, a bare id, or a name matched against your own playlists.
async fn resolve_playlist<A: LibraryApi>(client: &A, needle: &str) -> Result<String> {
    if let Some(id) = needle.strip_prefix("spotify:playlist:") {
        return Ok(id.to_string());
    }
    // Spotify ids are 22 base62 characters.
    if needle.len() == 22 && needle.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Ok(needle.to_string());
    }

    let page = client.my_playlists(50, 0).await?;
    let lower = needle.to_lowercase();
    let matches: Vec<&boombox_core::api::SimplePlaylist> =
        page.items.iter().filter(|p| p.name.to_lowercase().contains(&lower)).collect();

    match matches.len() {
        1 => Ok(matches[0].id.clone()),
        0 => bail!(
            "no playlist matching `{needle}`. Yours: {}",
            page.items.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
        ),
        _ => bail!(
            "`{needle}` matches {}: {}",
            matches.len(),
            matches.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
        ),
    }
}

async fn liked<A: LibraryApi>(client: &A, limit: u32, json: bool) -> Result<()> {
    let page = client.saved_tracks(limit, 0).await?;
    if json {
        println!("{}", serde_json::to_string(&page)?);
        return Ok(());
    }
    let entries: Vec<Entry> = page.items.iter().map(|s| Entry::from_track(&s.track)).collect();
    print_entries(&entries, 0);
    println!("   {} of {} saved tracks", entries.len(), page.total);
    Ok(())
}

async fn albums<A: LibraryApi>(client: &A, json: bool) -> Result<()> {
    let page = client.saved_albums(50, 0).await?;
    if json {
        println!("{}", serde_json::to_string(&page)?);
        return Ok(());
    }
    let entries: Vec<Entry> = page.items.iter().map(|s| Entry::from_album(&s.album)).collect();
    print_entries(&entries, 0);
    println!("   {} of {} saved albums", entries.len(), page.total);
    Ok(())
}

/// With no URI, acts on whatever is playing.
async fn set_saved<A: LibraryApi + PlayerApi>(
    client: &A,
    uri: Option<String>,
    save: bool,
) -> Result<()> {
    let uri = match uri {
        Some(u) => boombox_core::uri::normalise(&u).ok_or_else(|| {
            anyhow::anyhow!(
                "expected a Spotify URI or share link, got `{u}`\n\
                 e.g. spotify:track:... or https://open.spotify.com/track/..."
            )
        })?,
        None => {
            let state =
                client.playback_state().await?.ok_or(boombox_core::Error::NoActiveDevice)?;
            state
                .item
                .as_ref()
                .and_then(|i| i.uri())
                .ok_or_else(|| anyhow::anyhow!("nothing is playing"))?
                .to_string()
        }
    };

    let uris = vec![uri.clone()];
    if save {
        client.library_add(&uris).await?;
    } else {
        client.library_remove(&uris).await?;
    }

    if std::io::stdout().is_terminal() {
        println!("{}  {uri}", if save { "\u{2665}" } else { "\u{2661}" });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_strings_and_marks_long_ones() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefghij", 5), "abcd\u{2026}");
        assert_eq!(truncate("abcdefghij", 5).chars().count(), 5);
    }

    #[test]
    fn truncate_counts_characters_not_bytes() {
        // Would panic on a byte slice.
        assert_eq!(truncate("Noëlle café", 6).chars().count(), 6);
    }
}
