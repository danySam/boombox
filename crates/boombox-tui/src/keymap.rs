use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::action::Action;

/// Mnemonic keys, in two families that deliberately do not overlap.
///
/// Playback is verbs -- what to do to the music -- and browsing is nouns:
/// which list to look at. Numbering the nouns, as this used to, meant
/// remembering an arbitrary order that carried no meaning. A letter that
/// matches the word carries its own reminder.
///
/// The one compromise is the queue. `q` quits in every terminal program
/// there is and breaking that would cost more than it buys, so the queue
/// takes `Q`. Getting the queue when you meant to quit is harmless; the
/// reverse would not be.
///
/// Returns `None` for anything unbound so the caller can decide whether to
/// ignore it or treat it as a dismiss.
pub fn map(key: KeyEvent) -> Option<Action> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    Some(match (key.code, ctrl, shift) {
        (KeyCode::Char('c'), true, _) => Action::Quit,
        (KeyCode::Char('q'), false, _) => Action::Quit,
        (KeyCode::Char('?'), _, _) => Action::ToggleHelp,
        (KeyCode::Esc, _, _) => Action::Dismiss,

        // Movement. Vim keys and arrows, unchanged.
        (KeyCode::Char('u'), true, _) => Action::PageUp,
        (KeyCode::Char('d'), true, _) => Action::PageDown,
        (KeyCode::PageUp, _, _) => Action::PageUp,
        (KeyCode::PageDown, _, _) => Action::PageDown,
        (KeyCode::Char('g'), false, _) | (KeyCode::Home, _, _) => Action::Top,
        (KeyCode::Char('G'), _, _) | (KeyCode::End, _, _) => Action::Bottom,
        (KeyCode::Char('j'), false, _) | (KeyCode::Down, _, _) => Action::Down,
        (KeyCode::Char('k'), false, _) | (KeyCode::Up, _, _) => Action::Up,
        (KeyCode::Enter, _, _) => Action::Select,
        (KeyCode::Backspace, _, _) => Action::Back,

        // Verbs: what to do to the music.
        (KeyCode::Char(' '), _, _) => Action::PlayPause,
        (KeyCode::Char('n'), false, _) => Action::NextTrack,
        (KeyCode::Char('b'), false, _) => Action::PreviousTrack,
        (KeyCode::Right, _, true) => Action::SeekForward,
        (KeyCode::Left, _, true) => Action::SeekBackward,
        (KeyCode::Char('>'), _, _) => Action::SeekForward,
        (KeyCode::Char('<'), _, _) => Action::SeekBackward,
        (KeyCode::Char('=' | '+'), _, _) => Action::VolumeUp,
        (KeyCode::Char('-' | '_'), _, _) => Action::VolumeDown,
        (KeyCode::Char('s'), false, _) => Action::ToggleShuffle,
        (KeyCode::Char('r'), false, _) => Action::CycleRepeat,
        (KeyCode::Char('v'), false, _) => Action::CycleVisual,

        // Nouns: which list to look at. Each is the word's own first letter.
        (KeyCode::Char('l'), false, _) => Action::OpenLiked,
        (KeyCode::Char('a'), false, _) => Action::OpenAlbums,
        (KeyCode::Char('p'), false, _) => Action::OpenPlaylists,
        (KeyCode::Char('d'), false, _) => Action::OpenDevices,
        (KeyCode::Char('Q'), _, _) => Action::OpenQueue,
        (KeyCode::Char('/'), _, _) => Action::OpenSearch,
        // `A` because the word is "add" and `a` is already albums -- the
        // same shift-for-the-bigger-version pairing as `e`/`E`.
        (KeyCode::Char('A'), _, _) => Action::AddPlaylist,
        (KeyCode::Tab, _, _) => Action::ToggleBrowse,

        // Acting on the row under the cursor.
        (KeyCode::Char('.'), _, _) => Action::Save,
        // Lower case acts on the track, upper case on the whole list.
        (KeyCode::Char('t'), false, _) => Action::PlayTrackOnly,
        (KeyCode::Char('e'), false, _) => Action::Enqueue,
        (KeyCode::Char('E'), _, _) => Action::EnqueueAll,
        (KeyCode::Char('R'), _, _) | (KeyCode::F(5), _, _) => Action::Refresh,

        _ => return None,
    })
}

/// While the search box has focus almost every printable key is text, so the
/// normal bindings would make the box unusable.
pub fn map_typing(key: KeyEvent) -> Option<Action> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    Some(match (key.code, ctrl) {
        (KeyCode::Char('c'), true) => Action::Quit,
        (KeyCode::Esc, _) => Action::Dismiss,
        (KeyCode::Enter, _) => Action::Submit,
        (KeyCode::Backspace, _) => Action::Backspace,
        (KeyCode::Down, _) => Action::Down,
        (KeyCode::Up, _) => Action::Up,
        (KeyCode::Tab, _) => Action::FocusNext,
        (KeyCode::Char(c), false) => Action::Char(c),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn with(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn vim_and_arrows_both_navigate() {
        assert_eq!(map(key(KeyCode::Char('j'))), Some(Action::Down));
        assert_eq!(map(key(KeyCode::Down)), Some(Action::Down));
        assert_eq!(map(key(KeyCode::Char('k'))), Some(Action::Up));
        assert_eq!(map(key(KeyCode::Up)), Some(Action::Up));
    }

    #[test]
    fn ctrl_c_and_q_both_quit() {
        assert_eq!(map(with(KeyCode::Char('c'), KeyModifiers::CONTROL)), Some(Action::Quit));
        assert_eq!(map(key(KeyCode::Char('q'))), Some(Action::Quit));
    }

    #[test]
    fn plain_arrows_navigate_but_shifted_arrows_seek() {
        assert_eq!(map(with(KeyCode::Right, KeyModifiers::SHIFT)), Some(Action::SeekForward));
        assert_eq!(map(with(KeyCode::Left, KeyModifiers::SHIFT)), Some(Action::SeekBackward));
    }

    #[test]
    fn ctrl_u_and_ctrl_d_page_together() {
        // Ctrl-d is half-page-down in every list UI with vim keys, and Ctrl-c
        // already covers "get me out", so paging wins the binding.
        assert_eq!(map(with(KeyCode::Char('u'), KeyModifiers::CONTROL)), Some(Action::PageUp));
        assert_eq!(map(with(KeyCode::Char('d'), KeyModifiers::CONTROL)), Some(Action::PageDown));
    }

    #[test]
    fn transport_keys_are_bound() {
        assert_eq!(map(key(KeyCode::Char(' '))), Some(Action::PlayPause));
        assert_eq!(map(key(KeyCode::Char('n'))), Some(Action::NextTrack));
        assert_eq!(map(key(KeyCode::Char('b'))), Some(Action::PreviousTrack));
        assert_eq!(map(key(KeyCode::Char('s'))), Some(Action::ToggleShuffle));
        assert_eq!(map(key(KeyCode::Char('r'))), Some(Action::CycleRepeat));
        assert_eq!(map(key(KeyCode::Char('='))), Some(Action::VolumeUp));
        assert_eq!(map(key(KeyCode::Char('-'))), Some(Action::VolumeDown));
    }

    #[test]
    /// Each list opens on the first letter of its own name, so the key is
    /// recoverable from the word rather than from a remembered order.
    fn each_list_opens_on_its_own_initial() {
        assert_eq!(map(key(KeyCode::Char('l'))), Some(Action::OpenLiked));
        assert_eq!(map(key(KeyCode::Char('a'))), Some(Action::OpenAlbums));
        assert_eq!(map(key(KeyCode::Char('p'))), Some(Action::OpenPlaylists));
        assert_eq!(map(key(KeyCode::Char('d'))), Some(Action::OpenDevices));
        assert_eq!(map(key(KeyCode::Char('/'))), Some(Action::OpenSearch));
    }

    /// The one place the scheme bends. `q` quitting is near-universal, so
    /// the queue takes the shifted key rather than the other way round.
    #[test]
    fn q_still_quits_and_the_queue_takes_the_shifted_key() {
        assert_eq!(map(key(KeyCode::Char('q'))), Some(Action::Quit));
        assert_eq!(map(key(KeyCode::Char('Q'))), Some(Action::OpenQueue));
    }

    /// The digits carried no meaning, which is why they were hard to hold
    /// on to. They are gone rather than kept as silent aliases.
    #[test]
    fn the_old_digits_are_unbound() {
        for c in "12345".chars() {
            assert_eq!(map(key(KeyCode::Char(c))), None, "{c} should be unbound");
        }
    }

    /// Shift means "the whole list": one rule rather than two bindings.
    #[test]
    fn shift_widens_a_row_action_to_the_list() {
        assert_eq!(map(key(KeyCode::Char('e'))), Some(Action::Enqueue));
        assert_eq!(map(key(KeyCode::Char('E'))), Some(Action::EnqueueAll));
    }

    #[test]
    fn t_plays_the_track_on_its_own() {
        assert_eq!(map(key(KeyCode::Char('t'))), Some(Action::PlayTrackOnly));
    }

    #[test]
    fn playback_verbs_and_row_actions_do_not_collide_with_the_lists() {
        assert_eq!(map(key(KeyCode::Char('s'))), Some(Action::ToggleShuffle));
        assert_eq!(map(key(KeyCode::Char('r'))), Some(Action::CycleRepeat));
        assert_eq!(map(key(KeyCode::Char('v'))), Some(Action::CycleVisual));
        assert_eq!(map(key(KeyCode::Char('b'))), Some(Action::PreviousTrack));
        assert_eq!(map(key(KeyCode::Char('e'))), Some(Action::Enqueue));
    }

    /// The stage is not a destination, so nothing navigates to it.
    #[test]
    fn tab_toggles_the_browser_rather_than_switching_panes() {
        assert_eq!(map(key(KeyCode::Tab)), Some(Action::ToggleBrowse));
    }

    #[test]
    fn typing_mode_sends_characters_instead_of_commands() {
        // 'n' is next-track normally; in the search box it must be a letter.
        assert_eq!(map(key(KeyCode::Char('n'))), Some(Action::NextTrack));
        assert_eq!(map_typing(key(KeyCode::Char('n'))), Some(Action::Char('n')));
        assert_eq!(map_typing(key(KeyCode::Char(' '))), Some(Action::Char(' ')));
        assert_eq!(map_typing(key(KeyCode::Char('q'))), Some(Action::Char('q')));
    }

    #[test]
    fn typing_mode_still_allows_escape_submit_and_ctrl_c() {
        assert_eq!(map_typing(key(KeyCode::Esc)), Some(Action::Dismiss));
        assert_eq!(map_typing(key(KeyCode::Enter)), Some(Action::Submit));
        assert_eq!(map_typing(key(KeyCode::Backspace)), Some(Action::Backspace));
        assert_eq!(map_typing(with(KeyCode::Char('c'), KeyModifiers::CONTROL)), Some(Action::Quit));
    }

    #[test]
    fn typing_mode_lets_arrows_move_through_results() {
        assert_eq!(map_typing(key(KeyCode::Down)), Some(Action::Down));
        assert_eq!(map_typing(key(KeyCode::Up)), Some(Action::Up));
    }

    #[test]
    fn unbound_keys_map_to_nothing() {
        assert_eq!(map(key(KeyCode::Char('z'))), None);
        assert_eq!(map(key(KeyCode::F(9))), None);
    }

    #[test]
    fn remote_actions_are_flagged_and_local_ones_are_not() {
        assert!(Action::NextTrack.is_remote());
        assert!(Action::VolumeUp.is_remote());
        assert!(!Action::Down.is_remote());
        assert!(!Action::Quit.is_remote());
        assert!(!Action::ToggleHelp.is_remote());
    }
}
