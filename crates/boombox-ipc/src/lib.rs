//! Line-delimited JSON over a Unix socket, between the `boombox` CLI/TUI and a
//! running `boombox daemon`.
//!
//! One request, one response, one connection. Keeping it stateless means a
//! wedged client can never wedge the daemon, and `socat`/`nc` are enough to
//! debug the protocol by hand.

pub mod protocol;

use std::path::{Path, PathBuf};
use std::time::Duration;

use boombox_core::api::library::{LibraryApi, RecentsApi};
use boombox_core::api::library_models::{
    Page, PlaylistItem, SavedAlbum, SavedShow, SavedTrack, SearchResults, SearchType,
    SimplePlaylist,
};
use boombox_core::api::player::{PlayOptions, PlayerApi, SpectrumApi};
use boombox_core::api::{Device, PlaybackState, Queue, RepeatState};
use boombox_core::error::{Error, Result};
use boombox_core::recent::Recent;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixStream;

pub use protocol::{DaemonStatus, Request, Response, WireError, WireErrorKind};

/// Talks to a running daemon. Implements [`PlayerApi`], so the front ends
/// cannot tell whether they are speaking to the daemon or to Spotify.
pub struct IpcClient {
    path: PathBuf,
}

impl IpcClient {
    /// Succeeds only if a daemon is actually listening, so callers can treat
    /// an error as "no daemon, go direct".
    pub async fn connect(path: &Path) -> Result<Self> {
        // Probing now means the caller learns immediately, rather than
        // discovering a dead socket midway through a command.
        let _ = UnixStream::connect(path).await.map_err(|e| {
            tracing::debug!("no daemon at {}: {e}", path.display());
            Error::Io(e)
        })?;
        Ok(Self { path: path.to_path_buf() })
    }

    pub async fn request(&self, request: Request) -> Result<Response> {
        exchange(&self.path, request).await
    }

    /// Round-trips a request on a fresh connection to `path`.
    pub async fn oneshot(path: &Path, request: Request) -> Result<Response> {
        exchange(path, request).await
    }

    async fn unit(&self, request: Request) -> Result<()> {
        match self.request(request).await? {
            Response::Unit => Ok(()),
            Response::Error(e) => Err(e.into()),
            other => Err(unexpected(&other)),
        }
    }
}

/// One request on a fresh connection, bounded by that request's timeout.
///
/// The whole exchange is timed, not just the connect. Connecting proves
/// nothing about whether the daemon is alive: the kernel accepts into the
/// listen backlog on the process's behalf, so a daemon that is stopped or
/// deadlocked still "accepts" -- and then never answers. Before this, that
/// hung every command, including `boombox daemon --status` and `--stop`, which
/// are the ones you would reach for to find out why.
async fn exchange(path: &Path, request: Request) -> Result<Response> {
    let limit = request.timeout();
    exchange_within(path, request, limit).await
}

/// [`exchange`] with the allowance given explicitly, so tests can use one
/// measured in milliseconds instead of waiting out the real thing.
async fn exchange_within(path: &Path, request: Request, limit: Duration) -> Result<Response> {
    let attempt = async {
        let stream = UnixStream::connect(path).await?;
        send_request(stream, request).await
    };
    match tokio::time::timeout(limit, attempt).await {
        Ok(result) => result,
        // Rounded up, so a sub-second allowance never claims "within 0s".
        Err(_) => Err(Error::DaemonNotAnswering(limit.as_secs_f64().ceil() as u64)),
    }
}

async fn send_request(stream: UnixStream, request: Request) -> Result<Response> {
    let (read_half, mut write_half) = stream.into_split();

    let mut line = serde_json::to_string(&request)?;
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;

    let mut reader = BufReader::new(read_half);
    let mut buf = String::new();
    if reader.read_line(&mut buf).await? == 0 {
        return Err(Error::Api {
            status: 0,
            message: "daemon closed the connection without replying".into(),
        });
    }
    Ok(serde_json::from_str(&buf)?)
}

fn unexpected(response: &Response) -> Error {
    Error::Api { status: 0, message: format!("unexpected daemon response: {}", response.name()) }
}

impl PlayerApi for IpcClient {
    async fn playback_state(&self) -> Result<Option<PlaybackState>> {
        match self.request(Request::PlaybackState).await? {
            Response::PlaybackState(state) => Ok(state),
            Response::Error(e) => Err(e.into()),
            other => Err(unexpected(&other)),
        }
    }

    async fn devices(&self) -> Result<Vec<Device>> {
        match self.request(Request::Devices).await? {
            Response::Devices(devices) => Ok(devices),
            Response::Error(e) => Err(e.into()),
            other => Err(unexpected(&other)),
        }
    }

    async fn play(&self, opts: PlayOptions) -> Result<()> {
        self.unit(Request::Play(opts)).await
    }

    async fn pause(&self) -> Result<()> {
        self.unit(Request::Pause).await
    }

    async fn next(&self) -> Result<()> {
        self.unit(Request::Next).await
    }

    async fn previous(&self) -> Result<()> {
        self.unit(Request::Previous).await
    }

    async fn seek(&self, position_ms: u64) -> Result<()> {
        self.unit(Request::Seek { position_ms }).await
    }

    async fn set_volume(&self, percent: u32) -> Result<()> {
        self.unit(Request::SetVolume { percent }).await
    }

    async fn set_shuffle(&self, on: bool) -> Result<()> {
        self.unit(Request::SetShuffle { on }).await
    }

    async fn set_repeat(&self, state: RepeatState) -> Result<()> {
        self.unit(Request::SetRepeat { state }).await
    }

    async fn transfer(&self, device_id: &str, play: bool) -> Result<()> {
        self.unit(Request::Transfer { device_id: device_id.to_string(), play }).await
    }

    async fn queue(&self) -> Result<Queue> {
        match self.request(Request::Queue).await? {
            Response::Queue(q) => Ok(q),
            Response::Error(e) => Err(e.into()),
            other => Err(unexpected(&other)),
        }
    }

    async fn add_to_queue(&self, uri: &str) -> Result<()> {
        self.unit(Request::AddToQueue { uri: uri.to_string() }).await
    }
}

impl LibraryApi for IpcClient {
    async fn search(
        &self,
        query: &str,
        types: &[SearchType],
        limit: u32,
        offset: u32,
    ) -> Result<SearchResults> {
        match self
            .request(Request::Search {
                query: query.to_string(),
                types: types.to_vec(),
                limit,
                offset,
            })
            .await?
        {
            Response::Search(results) => Ok(*results),
            Response::Error(e) => Err(e.into()),
            other => Err(unexpected(&other)),
        }
    }

    async fn my_playlists(&self, limit: u32, offset: u32) -> Result<Page<SimplePlaylist>> {
        match self.request(Request::MyPlaylists { limit, offset }).await? {
            Response::Playlists(page) => Ok(page),
            Response::Error(e) => Err(e.into()),
            other => Err(unexpected(&other)),
        }
    }

    async fn playlist(&self, _id: &str) -> Result<SimplePlaylist> {
        // Nothing needs this over IPC yet; the daemon would just forward it.
        Err(Error::Api { status: 0, message: "playlist metadata is not served over IPC".into() })
    }

    async fn playlist_items(
        &self,
        id: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Page<PlaylistItem>> {
        match self.request(Request::PlaylistItems { id: id.to_string(), limit, offset }).await? {
            Response::PlaylistItems(page) => Ok(page),
            Response::Error(e) => Err(e.into()),
            other => Err(unexpected(&other)),
        }
    }

    async fn saved_tracks(&self, limit: u32, offset: u32) -> Result<Page<SavedTrack>> {
        match self.request(Request::SavedTracks { limit, offset }).await? {
            Response::SavedTracks(page) => Ok(page),
            Response::Error(e) => Err(e.into()),
            other => Err(unexpected(&other)),
        }
    }

    async fn saved_albums(&self, limit: u32, offset: u32) -> Result<Page<SavedAlbum>> {
        match self.request(Request::SavedAlbums { limit, offset }).await? {
            Response::SavedAlbums(page) => Ok(page),
            Response::Error(e) => Err(e.into()),
            other => Err(unexpected(&other)),
        }
    }

    async fn saved_shows(&self, _limit: u32, _offset: u32) -> Result<Page<SavedShow>> {
        Err(Error::Api { status: 0, message: "shows are not served over IPC".into() })
    }

    async fn library_contains(&self, uris: &[String]) -> Result<Vec<bool>> {
        match self.request(Request::LibraryContains { uris: uris.to_vec() }).await? {
            Response::Contains(flags) => Ok(flags),
            Response::Error(e) => Err(e.into()),
            other => Err(unexpected(&other)),
        }
    }

    async fn library_add(&self, uris: &[String]) -> Result<()> {
        self.unit(Request::LibraryAdd { uris: uris.to_vec() }).await
    }

    async fn library_remove(&self, uris: &[String]) -> Result<()> {
        self.unit(Request::LibraryRemove { uris: uris.to_vec() }).await
    }
}

impl SpectrumApi for IpcClient {
    async fn spectrum(&self, bands: u16) -> Result<Vec<f32>> {
        match self.request(Request::Spectrum { bands }).await? {
            Response::Spectrum(v) => Ok(v),
            Response::Error(e) => Err(e.into()),
            other => Err(unexpected(&other)),
        }
    }

    async fn waveform(&self, points: u16) -> Result<Vec<f32>> {
        match self.request(Request::Waveform { points }).await? {
            Response::Waveform(v) => Ok(v),
            Response::Error(e) => Err(e.into()),
            other => Err(unexpected(&other)),
        }
    }

    async fn envelope(&self, points: u16) -> Result<Vec<f32>> {
        match self.request(Request::Envelope { points }).await? {
            Response::Envelope(v) => Ok(v),
            Response::Error(e) => Err(e.into()),
            other => Err(unexpected(&other)),
        }
    }

    async fn audio_stalled_secs(&self) -> Result<Option<u64>> {
        match self.request(Request::Ping).await? {
            Response::Pong(status) => Ok(status.audio_stalled_secs),
            Response::Error(e) => Err(e.into()),
            other => Err(unexpected(&other)),
        }
    }
}

impl RecentsApi for IpcClient {
    async fn recents(&self) -> Result<Vec<Recent>> {
        match self.request(Request::Recents).await? {
            Response::Recents(items) => Ok(items),
            Response::Error(e) => Err(e.into()),
            other => Err(unexpected(&other)),
        }
    }

    async fn remember(&self, uri: &str) -> Result<()> {
        self.unit(Request::Remember { uri: uri.to_string() }).await
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use tokio::net::UnixListener;

    use super::*;

    /// A socket path unique to this test run, short enough for `sun_path`.
    fn socket(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("boombox-ipc-{}-{name}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Stands in for a daemon waiting on Spotify: reads the request, then
    /// replies after `after`.
    fn reply_after(listener: UnixListener, after: Duration) {
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (read, mut write) = stream.into_split();
            let mut line = String::new();
            let _ = BufReader::new(read).read_line(&mut line).await;
            tokio::time::sleep(after).await;
            let _ = write.write_all(b"{\"reply\":\"unit\"}\n").await;
        });
    }

    /// The case that used to hang forever. A listener that is bound and
    /// never accepts is exactly what a stopped daemon looks like from
    /// outside: the connection succeeds, and then nothing.
    #[tokio::test]
    async fn a_daemon_that_never_answers_times_out_instead_of_hanging() {
        let path = socket("silent");
        let _listener = UnixListener::bind(&path).unwrap();

        let started = Instant::now();
        let err =
            exchange_within(&path, Request::Ping, Duration::from_millis(200)).await.unwrap_err();
        assert!(matches!(err, Error::DaemonNotAnswering(1)), "{err}");
        assert!(started.elapsed() < Duration::from_secs(2), "waited {:?}", started.elapsed());
        let _ = std::fs::remove_file(&path);
    }

    /// A daemon that is slow because Spotify is must not be cut off.
    #[tokio::test]
    async fn a_reply_inside_the_allowance_is_not_cut_off() {
        let path = socket("slow");
        reply_after(UnixListener::bind(&path).unwrap(), Duration::from_millis(300));

        let response =
            exchange_within(&path, Request::Pause, Duration::from_secs(5)).await.unwrap();
        assert!(matches!(response, Response::Unit), "{}", response.name());
        let _ = std::fs::remove_file(&path);
    }

    /// The same slow reply given too little time reads as a stuck daemon --
    /// which is why a forwarded request must never get the local allowance.
    #[tokio::test]
    async fn a_reply_outside_the_allowance_is_reported_as_not_answering() {
        let path = socket("late");
        reply_after(UnixListener::bind(&path).unwrap(), Duration::from_millis(800));

        let err =
            exchange_within(&path, Request::Pause, Duration::from_millis(100)).await.unwrap_err();
        assert!(matches!(err, Error::DaemonNotAnswering(_)), "{err}");
        let _ = std::fs::remove_file(&path);
    }
}
