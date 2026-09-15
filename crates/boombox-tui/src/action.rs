/// What a keypress means. Separating this from the key that produced it keeps
/// the keymap a pure function and makes the update logic testable without a
/// terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Quit,
    ToggleHelp,
    Dismiss,

    FocusNext,
    FocusPrevious,
    Up,
    Down,
    PageUp,
    PageDown,
    Top,
    Bottom,
    Select,

    PlayPause,
    NextTrack,
    PreviousTrack,
    SeekForward,
    SeekBackward,
    VolumeUp,
    VolumeDown,
    ToggleShuffle,
    CycleRepeat,

    /// Open the browse palette on a given list, or close it. The stage is
    /// never a destination, so there is no "go to now playing".
    OpenSearch,
    /// Open the prompt for adding a playlist from a link.
    AddPlaylist,
    OpenLiked,
    OpenAlbums,
    OpenPlaylists,
    OpenQueue,
    OpenDevices,
    ToggleBrowse,
    CloseBrowse,
    CycleVisual,
    Refresh,
    Save,
    /// Play the row under the cursor on its own, rather than as part of
    /// the list it came from -- which is what `Select` now does.
    PlayTrackOnly,
    /// Append the row under the cursor to the play queue.
    Enqueue,
    /// Append the whole loaded list. Slow: the API takes one track per
    /// call, so this is a background job rather than a request.
    EnqueueAll,
    Back,

    /// Text entry, used while the search box has focus.
    Char(char),
    Backspace,
    Submit,
}

impl Action {
    /// Actions that reach Spotify, and so must be dispatched off the render
    /// loop rather than awaited inline.
    pub fn is_remote(self) -> bool {
        matches!(
            self,
            Self::PlayPause
                | Self::NextTrack
                | Self::PreviousTrack
                | Self::SeekForward
                | Self::SeekBackward
                | Self::VolumeUp
                | Self::VolumeDown
                | Self::ToggleShuffle
                | Self::CycleRepeat
                | Self::Select
                | Self::Refresh
                | Self::Save
                | Self::PlayTrackOnly
                | Self::Enqueue
                | Self::EnqueueAll
                | Self::Submit
        )
    }
}
