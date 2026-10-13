use crate::{
    state::model::{Category, Context, ContextId},
    ui::single_line_input::LineInput,
};
use ratatui::widgets::{ListState, TableState};

#[derive(Clone, Debug, PartialEq)]
pub enum PageState {
    Library {
        state: LibraryPageUIState,
    },
    Context {
        id: Option<ContextId>,
        context_page_type: ContextPageType,
        state: Option<ContextPageUIState>,
    },
    Search {
        line_input: LineInput,
        current_query: String,
        state: SearchPageUIState,
    },
    Lyrics {
        track_uri: String,
        track: String,
        artists: String,
    },
    Browse {
        state: BrowsePageUIState,
    },
    Queue {
        scroll_offset: usize,
    },
    CommandHelp {
        scroll_offset: usize,
    },
    Logs {
        scroll_offset: usize,
    },
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum PageType {
    Library,
    Context,
    Search,
    Browse,
    Lyrics,
    Queue,
    CommandHelp,
    Logs,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LibraryPageUIState {
    pub playlist_list: ListState,
    pub saved_album_list: ListState,
    pub followed_artist_list: ListState,
    pub saved_show_list: ListState,
    pub focus: LibraryFocusState,
    pub playlist_folder_id: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SearchPageUIState {
    pub track_list: ListState,
    pub album_list: ListState,
    pub artist_list: ListState,
    pub playlist_list: ListState,
    pub show_list: ListState,
    pub episode_list: ListState,
    pub focus: SearchFocusState,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ContextPageType {
    CurrentPlaying,
    Browsing(ContextId),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ContextPageUIState {
    Playlist {
        track_table: TableState,
    },
    Album {
        track_table: TableState,
    },
    Artist {
        top_track_table: TableState,
        album_table: TableState,
        related_artist_list: ListState,
        liked_track_table: TableState,
        focus: ArtistFocusState,
    },
    Tracks {
        track_table: TableState,
    },
    Show {
        episode_table: TableState,
        focus: ShowFocusState,
        description_scroll: u16,
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LibraryFocusState {
    Playlists,
    SavedAlbums,
    FollowedArtists,
    SavedShows,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ArtistFocusState {
    TopTracks,
    Albums,
    RelatedArtists,
    LikedSongs,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ShowFocusState {
    Episodes,
    Details,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SearchFocusState {
    Input,
    Tracks,
    Albums,
    Artists,
    Playlists,
    Shows,
    Episodes,
}

#[derive(Clone, Debug, PartialEq)]
pub enum BrowsePageUIState {
    CategoryList {
        state: ListState,
    },
    CategoryPlaylistList {
        category: Category,
        state: ListState,
    },
}

pub enum MutableWindowState<'a> {
    Table(&'a mut TableState),
    List(&'a mut ListState),
    Scroll(&'a mut usize),
}

impl PageState {
    /// The type of the page.
    pub fn page_type(&self) -> PageType {
        match self {
            PageState::Library { .. } => PageType::Library,
            PageState::Context { .. } => PageType::Context,
            PageState::Search { .. } => PageType::Search,
            PageState::Browse { .. } => PageType::Browse,
            PageState::Lyrics { .. } => PageType::Lyrics,
            PageState::Queue { .. } => PageType::Queue,
            PageState::CommandHelp { .. } => PageType::CommandHelp,
            PageState::Logs { .. } => PageType::Logs,
        }
    }

    /// Select a `id`-th item in the currently focused window of the page.
    pub fn select(&mut self, id: usize) {
        if let Some(mut state) = self.focus_window_state_mut() {
            state.select(id);
        }
    }

    /// The selected item's position in the currently focused window of the page.
    pub fn selected(&mut self) -> Option<usize> {
        self.focus_window_state_mut()
            .map(|state| state.selected())?
    }

    /// Like [`Self::selected`], but reads the underlying window cursor even while focus sits
    /// on a non-cursor panel (the show page's description). Anything that *acts* on the
    /// current item must use this — otherwise focus mode silently makes it act on row 0.
    pub fn selected_ignoring_focus(&mut self) -> Option<usize> {
        if let Self::Context {
            state: Some(ContextPageUIState::Show { episode_table, .. }),
            ..
        } = self
        {
            return episode_table.selected();
        }
        self.selected()
    }

    /// Clamp every cursor of the context page into the bounds of `ctx`, so a background
    /// refresh that shrinks a list can never leave a cursor pointing past its end.
    /// Non-context pages and variants that do not match `ctx` are a no-op.
    pub fn clamp_context_selection(&mut self, ctx: &Context) {
        let Self::Context {
            state: Some(state), ..
        } = self
        else {
            return;
        };
        let clamp = Self::clamp_cursor;
        match (state, ctx) {
            (ContextPageUIState::Playlist { track_table }, Context::Playlist { tracks, .. })
            | (ContextPageUIState::Album { track_table }, Context::Album { tracks, .. })
            | (ContextPageUIState::Tracks { track_table }, Context::Tracks { tracks, .. }) => {
                track_table.select(clamp(track_table.selected(), tracks.len()));
            }
            (ContextPageUIState::Show { episode_table, .. }, Context::Show { episodes, .. }) => {
                episode_table.select(clamp(episode_table.selected(), episodes.len()));
            }
            (
                ContextPageUIState::Artist {
                    top_track_table,
                    album_table,
                    related_artist_list,
                    ..
                },
                Context::Artist {
                    top_tracks,
                    albums,
                    related_artists,
                    ..
                },
            ) => {
                top_track_table.select(clamp(top_track_table.selected(), top_tracks.len()));
                album_table.select(clamp(album_table.selected(), albums.len()));
                related_artist_list
                    .select(clamp(related_artist_list.selected(), related_artists.len()));
            }
            _ => {}
        }
    }

    /// Clamp a `TableState`/`ListState` cursor so it cannot point past the end of its list.
    fn clamp_cursor(selected: Option<usize>, len: usize) -> Option<usize> {
        match selected {
            Some(selected) if selected >= len => {
                if len == 0 {
                    None
                } else {
                    Some(len - 1)
                }
            }
            selected => selected,
        }
    }

    /// The currently focused window state of the page.
    pub fn focus_window_state_mut(&mut self) -> Option<MutableWindowState<'_>> {
        match self {
            Self::Library {
                state:
                    LibraryPageUIState {
                        playlist_list,
                        saved_album_list,
                        followed_artist_list,
                        saved_show_list,
                        focus,
                        ..
                    },
            } => Some(match focus {
                LibraryFocusState::Playlists => MutableWindowState::List(playlist_list),
                LibraryFocusState::SavedAlbums => MutableWindowState::List(saved_album_list),
                LibraryFocusState::FollowedArtists => {
                    MutableWindowState::List(followed_artist_list)
                }
                LibraryFocusState::SavedShows => MutableWindowState::List(saved_show_list),
            }),
            Self::Search {
                state:
                    SearchPageUIState {
                        track_list,
                        album_list,
                        artist_list,
                        playlist_list,
                        show_list,
                        episode_list,
                        focus,
                    },
                ..
            } => match focus {
                SearchFocusState::Input => None,
                SearchFocusState::Tracks => Some(MutableWindowState::List(track_list)),
                SearchFocusState::Albums => Some(MutableWindowState::List(album_list)),
                SearchFocusState::Artists => Some(MutableWindowState::List(artist_list)),
                SearchFocusState::Playlists => Some(MutableWindowState::List(playlist_list)),
                SearchFocusState::Shows => Some(MutableWindowState::List(show_list)),
                SearchFocusState::Episodes => Some(MutableWindowState::List(episode_list)),
            },
            Self::Context { state, .. } => state.as_mut().and_then(|state| match state {
                ContextPageUIState::Tracks { track_table }
                | ContextPageUIState::Playlist { track_table }
                | ContextPageUIState::Album { track_table } => {
                    Some(MutableWindowState::Table(track_table))
                }
                ContextPageUIState::Artist {
                    top_track_table,
                    album_table,
                    related_artist_list,
                    liked_track_table,
                    focus,
                } => Some(match focus {
                    ArtistFocusState::TopTracks => MutableWindowState::Table(top_track_table),
                    ArtistFocusState::Albums => MutableWindowState::Table(album_table),
                    ArtistFocusState::RelatedArtists => {
                        MutableWindowState::List(related_artist_list)
                    }
                    ArtistFocusState::LikedSongs => MutableWindowState::Table(liked_track_table),
                }),
                ContextPageUIState::Show {
                    episode_table,
                    focus,
                    ..
                } => match focus {
                    ShowFocusState::Episodes => Some(MutableWindowState::Table(episode_table)),
                    ShowFocusState::Details => None,
                },
            }),
            Self::Browse { state } => match state {
                BrowsePageUIState::CategoryList { state } => Some(MutableWindowState::List(state)),
                BrowsePageUIState::CategoryPlaylistList { state, .. } => {
                    Some(MutableWindowState::List(state))
                }
            },
            Self::Lyrics { .. } => None,
            Self::CommandHelp { scroll_offset }
            | Self::Queue { scroll_offset }
            | Self::Logs { scroll_offset } => Some(MutableWindowState::Scroll(scroll_offset)),
        }
    }
}

impl LibraryPageUIState {
    pub fn new() -> Self {
        Self {
            playlist_list: ListState::default(),
            saved_album_list: ListState::default(),
            followed_artist_list: ListState::default(),
            saved_show_list: ListState::default(),
            focus: LibraryFocusState::Playlists,
            playlist_folder_id: 0,
        }
    }
}

impl SearchPageUIState {
    pub fn new() -> Self {
        Self {
            track_list: ListState::default(),
            album_list: ListState::default(),
            artist_list: ListState::default(),
            playlist_list: ListState::default(),
            show_list: ListState::default(),
            episode_list: ListState::default(),
            focus: SearchFocusState::Input,
        }
    }
}

impl ContextPageType {
    pub fn title(&self) -> String {
        match self {
            ContextPageType::CurrentPlaying => String::from("Current Playing"),
            ContextPageType::Browsing(id) => match id {
                ContextId::Playlist(_) => String::from("Playlist"),
                ContextId::Album(_) => String::from("Album"),
                ContextId::Artist(_) => String::from("Artist"),
                ContextId::Tracks(id) => id.kind.clone(),
                ContextId::Show(_) => String::from("Show"),
            },
        }
    }
}

impl ContextPageUIState {
    pub fn new_playlist() -> Self {
        Self::Playlist {
            track_table: TableState::default(),
        }
    }

    pub fn new_album() -> Self {
        Self::Album {
            track_table: TableState::default(),
        }
    }

    pub fn new_artist() -> Self {
        Self::Artist {
            top_track_table: TableState::default(),
            album_table: TableState::default(),
            related_artist_list: ListState::default(),
            liked_track_table: TableState::default(),
            focus: ArtistFocusState::TopTracks,
        }
    }

    pub fn new_tracks() -> Self {
        Self::Tracks {
            track_table: TableState::default(),
        }
    }

    pub fn new_show() -> Self {
        Self::Show {
            episode_table: TableState::default(),
            focus: ShowFocusState::Episodes,
            description_scroll: 0,
        }
    }
}

impl MutableWindowState<'_> {
    pub fn select(&mut self, id: usize) {
        match self {
            Self::List(state) => state.select(Some(id)),
            Self::Table(state) => state.select(Some(id)),
            Self::Scroll(scroll_offset) => {
                **scroll_offset = id;
            }
        }
    }

    pub fn selected(&self) -> Option<usize> {
        match self {
            Self::List(state) => state.selected(),
            Self::Table(state) => state.selected(),
            Self::Scroll(scroll_offset) => Some(**scroll_offset),
        }
    }
}

pub trait Focusable {
    fn next(&mut self);
    fn previous(&mut self);
}

impl Focusable for PageState {
    fn next(&mut self) {
        match self {
            Self::Search {
                state: SearchPageUIState { focus, .. },
                ..
            } => focus.next(),
            Self::Library {
                state: LibraryPageUIState { focus, .. },
                ..
            } => focus.next(),
            Self::Context {
                state: Some(ContextPageUIState::Artist { focus, .. }),
                ..
            } => focus.next(),
            Self::Context {
                state: Some(ContextPageUIState::Show { focus, .. }),
                ..
            } => focus.next(),
            _ => {}
        }

        // reset the list/table state of the focus window. The show page's episode table is
        // exempt: its cursor drives the description panel, so resetting it on every focus
        // toggle destroys the inspection context (the cursor would snap back to episode 1).
        if !matches!(
            self,
            Self::Context {
                state: Some(ContextPageUIState::Show { .. }),
                ..
            }
        ) {
            if let Some(mut state) = self.focus_window_state_mut() {
                state.select(0);
            }
        }
    }

    fn previous(&mut self) {
        match self {
            Self::Search {
                state: SearchPageUIState { focus, .. },
                ..
            } => focus.previous(),
            Self::Library {
                state: LibraryPageUIState { focus, .. },
                ..
            } => focus.previous(),
            Self::Context {
                state: Some(ContextPageUIState::Artist { focus, .. }),
                ..
            } => focus.previous(),
            Self::Context {
                state: Some(ContextPageUIState::Show { focus, .. }),
                ..
            } => focus.previous(),
            _ => {}
        }

        // reset the list/table state of the focus window; the show page's episode table is
        // exempt (see `next`).
        if !matches!(
            self,
            Self::Context {
                state: Some(ContextPageUIState::Show { .. }),
                ..
            }
        ) {
            if let Some(mut state) = self.focus_window_state_mut() {
                state.select(0);
            }
        }
    }
}

macro_rules! impl_focusable {
	($struct:ty, $([$field:ident, $next_field:ident]),+) => {
		impl Focusable for $struct {
            fn next(&mut self) {
                *self = match self {
                    $(
                        Self::$field => Self::$next_field,
                    )+
                };
            }

            fn previous(&mut self) {
                *self = match self {
                    $(
                        Self::$next_field => Self::$field,
                    )+
                };
            }
        }
	};
}

impl_focusable!(
    LibraryFocusState,
    [Playlists, SavedAlbums],
    [SavedAlbums, FollowedArtists],
    [FollowedArtists, SavedShows],
    [SavedShows, Playlists]
);

impl_focusable!(
    ArtistFocusState,
    [TopTracks, LikedSongs],
    [LikedSongs, Albums],
    [Albums, RelatedArtists],
    [RelatedArtists, TopTracks]
);

impl_focusable!(ShowFocusState, [Episodes, Details], [Details, Episodes]);

impl_focusable!(
    SearchFocusState,
    [Input, Tracks],
    [Tracks, Albums],
    [Albums, Artists],
    [Artists, Playlists],
    [Playlists, Shows],
    [Shows, Episodes],
    [Episodes, Input]
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::model::{
        Album, AlbumId, Artist, ArtistId, Episode, EpisodeId, Playlist, PlaylistId, Show, ShowId,
        Track, TrackId, UserId,
    };
    use rspotify::model::Id;

    fn show_page(selected: usize) -> PageState {
        let mut page = PageState::Context {
            id: None,
            context_page_type: ContextPageType::Browsing(ContextId::Show(
                ShowId::from_id("1").expect("valid id").clone_static(),
            )),
            state: Some(ContextPageUIState::Show {
                episode_table: TableState::default(),
                focus: ShowFocusState::Episodes,
                description_scroll: 0,
            }),
        };
        page.select(selected);
        page
    }

    fn track(id: &str) -> Track {
        Track {
            id: TrackId::from_id(id).expect("valid id").clone_static(),
            name: id.to_owned(),
            artists: vec![],
            album: None,
            duration: std::time::Duration::from_secs(1),
            explicit: false,
            added_at: 0,
        }
    }

    fn album(id: &str) -> Album {
        Album {
            id: AlbumId::from_id(id).expect("valid id").clone_static(),
            release_date: String::new(),
            name: id.to_owned(),
            artists: vec![],
            typ: None,
            added_at: 0,
        }
    }

    fn artist(id: &str) -> Artist {
        Artist {
            id: ArtistId::from_id(id).expect("valid id").clone_static(),
            name: id.to_owned(),
        }
    }

    fn playlist(id: &str) -> Playlist {
        Playlist {
            id: PlaylistId::from_id(id).expect("valid id").clone_static(),
            collaborative: false,
            name: id.to_owned(),
            owner: (
                String::new(),
                UserId::from_id("u1").expect("valid id").clone_static(),
            ),
            desc: String::new(),
            current_folder_id: 0,
            snapshot_id: String::new(),
        }
    }

    fn show(id: &str) -> Show {
        Show {
            id: ShowId::from_id(id).expect("valid id").clone_static(),
            name: id.to_owned(),
            publisher: String::new(),
            #[cfg(feature = "image")]
            image_url: None,
        }
    }

    fn episode(id: &str) -> Episode {
        Episode {
            id: EpisodeId::from_id(id).expect("valid id").clone_static(),
            name: id.to_owned(),
            description: String::new(),
            duration: std::time::Duration::from_secs(1),
            show: None,
            release_date: String::new(),
            #[cfg(feature = "image")]
            image_url: None,
        }
    }

    fn context_page(state: ContextPageUIState) -> PageState {
        PageState::Context {
            id: None,
            context_page_type: ContextPageType::CurrentPlaying,
            state: Some(state),
        }
    }

    #[test]
    fn cursor_survives_full_focus_cycle() {
        let mut page = show_page(5);

        // Tab into the description panel and back out.
        page.next(); // Episodes -> Details
        assert_eq!(
            page.selected(),
            None,
            "Details focus must not expose the table cursor for key routing"
        );
        assert_eq!(page.selected_ignoring_focus(), Some(5));
        page.next(); // Details -> Episodes

        assert_eq!(
            page.selected(),
            Some(5),
            "cursor must survive the focus cycle"
        );
    }

    #[test]
    fn cursor_survives_inspection_and_back() {
        let mut page = show_page(7);

        page.next();
        // While Details is focused, actions must still see the real cursor.
        assert_eq!(page.selected_ignoring_focus(), Some(7));
        page.previous();

        assert_eq!(page.selected(), Some(7));
    }

    #[test]
    fn show_cursor_clamped_to_shorter_context() {
        let mut page = show_page(24);

        page.clamp_context_selection(&Context::Show {
            show: show("s1"),
            episodes: vec![episode("e1"), episode("e2"), episode("e3")],
            total_episodes: 3,
        });

        assert_eq!(
            page.selected(),
            Some(2),
            "a cursor past the end must clamp to the last episode"
        );
    }

    #[test]
    fn show_cursor_clamped_to_empty_context() {
        let mut page = show_page(24);

        page.clamp_context_selection(&Context::Show {
            show: show("s1"),
            episodes: vec![],
            total_episodes: 0,
        });

        assert_eq!(
            page.selected(),
            None,
            "an empty context must clear the cursor, not leave a stale index"
        );
    }

    #[test]
    fn playlist_cursor_clamped_to_context() {
        let mut page = context_page(ContextPageUIState::Playlist {
            track_table: TableState::default(),
        });
        page.select(10);

        page.clamp_context_selection(&Context::Playlist {
            playlist: playlist("p1"),
            tracks: vec![track("t1"), track("t2")],
        });

        assert_eq!(
            page.selected(),
            Some(1),
            "cursor must clamp to tracks.len() - 1"
        );
    }

    #[test]
    fn in_bounds_cursor_and_none_untouched() {
        // A cursor already inside the list is left alone.
        let mut page = context_page(ContextPageUIState::Tracks {
            track_table: TableState::default(),
        });
        page.select(1);
        page.clamp_context_selection(&Context::Tracks {
            tracks: vec![track("t1"), track("t2"), track("t3")],
            desc: String::from("tracks"),
        });
        assert_eq!(
            page.selected(),
            Some(1),
            "an in-bounds cursor must not move"
        );

        // A `None` cursor stays `None`.
        let mut page = context_page(ContextPageUIState::Tracks {
            track_table: TableState::default(),
        });
        page.clamp_context_selection(&Context::Tracks {
            tracks: vec![track("t1")],
            desc: String::from("tracks"),
        });
        assert_eq!(page.selected(), None, "an unset cursor must stay unset");
    }

    #[test]
    fn artist_cursors_clamped_by_their_own_lengths() {
        let mut page = context_page(ContextPageUIState::Artist {
            top_track_table: TableState::default(),
            album_table: TableState::default(),
            related_artist_list: ListState::default(),
            liked_track_table: TableState::default(),
            focus: ArtistFocusState::TopTracks,
        });
        let PageState::Context {
            state:
                Some(ContextPageUIState::Artist {
                    top_track_table,
                    album_table,
                    related_artist_list,
                    liked_track_table,
                    ..
                }),
            ..
        } = &mut page
        else {
            unreachable!()
        };
        top_track_table.select(Some(100));
        album_table.select(Some(50));
        related_artist_list.select(Some(60));
        liked_track_table.select(Some(9));

        page.clamp_context_selection(&Context::Artist {
            artist: artist("a0"),
            top_tracks: vec![track("t1"), track("t2")],
            albums: vec![album("al1")],
            related_artists: vec![artist("a1"), artist("a2"), artist("a3")],
        });

        let PageState::Context {
            state: Some(state), ..
        } = &page
        else {
            unreachable!()
        };
        let ContextPageUIState::Artist {
            top_track_table,
            album_table,
            related_artist_list,
            liked_track_table,
            ..
        } = state
        else {
            unreachable!()
        };
        assert_eq!(
            top_track_table.selected(),
            Some(1),
            "top tracks has length 2"
        );
        assert_eq!(album_table.selected(), Some(0), "albums has length 1");
        assert_eq!(
            related_artist_list.selected(),
            Some(2),
            "related artists has length 3"
        );
        assert_eq!(
            liked_track_table.selected(),
            Some(9),
            "the liked track table must be left untouched"
        );
    }

    #[test]
    fn non_context_page_is_noop() {
        let mut page = PageState::Library {
            state: LibraryPageUIState::new(),
        };
        page.select(3);

        page.clamp_context_selection(&Context::Show {
            show: show("s1"),
            episodes: vec![episode("e1")],
            total_episodes: 1,
        });

        assert_eq!(page.selected(), Some(3), "a library page must be unchanged");
    }

    #[test]
    fn variant_mismatch_is_noop() {
        let mut page = context_page(ContextPageUIState::Playlist {
            track_table: TableState::default(),
        });
        page.select(20);

        page.clamp_context_selection(&Context::Show {
            show: show("s1"),
            episodes: vec![episode("e1")],
            total_episodes: 1,
        });

        assert_eq!(
            page.selected(),
            Some(20),
            "a Playlist page clamped against a Show context must not move"
        );
    }
}
