use crate::{
    command,
    state::{
        model::{Album, Artist, Episode, EpisodeId, Playlist, Show, Track, TrackId},
        ItemId,
    },
};
use rspotify::model::PlaylistId;

use super::{Cursor, TextInput};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistCreateCurrentField {
    Name,
    Desc,
}

#[derive(Debug)]
pub enum PopupState {
    Search {
        query: String,
    },
    UserPlaylistList(PlaylistPopupAction, Cursor),
    UserFollowedArtistList(Cursor),
    UserSavedAlbumList(Cursor),
    DeviceList(Cursor),
    ArtistList(ArtistPopupAction, Vec<Artist>, Cursor),
    ThemeList(Vec<String>, Cursor),
    ActionList(Box<ActionListItem>, Cursor),
    PlaylistCreate {
        name: TextInput,
        desc: TextInput,
        current_field: PlaylistCreateCurrentField,
    },
    ConfirmAction {
        message: String,
        action: ConfirmableAction,
    },
}

#[derive(Debug, Clone)]
pub enum ConfirmableAction {
    DeleteTrackFromPlaylist {
        playlist_id: PlaylistId<'static>,
        track_id: TrackId<'static>,
    },
    DeleteFromLibrary(ItemId),
}

#[derive(Debug, Clone)]
pub enum ActionListItem {
    Track(Track, Vec<command::Action>),
    Artist(Artist, Vec<command::Action>),
    Album(Album, Vec<command::Action>),
    Playlist(Playlist, Vec<command::Action>),
    Show(Show, Vec<command::Action>),
    Episode(Episode, Vec<command::Action>),
}

/// An action on an item in a playlist popup list
#[derive(Debug)]
pub enum PlaylistPopupAction {
    Browse {
        folder_id: usize,
        search_query: String,
    },
    AddTrack {
        folder_id: usize,
        track_id: TrackId<'static>,
        search_query: String,
    },
    AddEpisode {
        folder_id: usize,
        episode_id: EpisodeId<'static>,
        search_query: String,
    },
}

/// An action on an item in an artist popup list
#[derive(Copy, Clone, Debug)]
pub enum ArtistPopupAction {
    Browse,
    ShowActions,
}

impl PopupState {
    /// Gets the (immutable) cursor of a list popup
    #[must_use]
    pub fn list_cursor(&self) -> Option<&Cursor> {
        match self {
            Self::DeviceList(list_state)
            | Self::UserPlaylistList(.., list_state)
            | Self::UserFollowedArtistList(list_state)
            | Self::UserSavedAlbumList(list_state)
            | Self::ArtistList(.., list_state)
            | Self::ThemeList(.., list_state)
            | Self::ActionList(.., list_state) => Some(list_state),
            Self::Search { .. } | Self::PlaylistCreate { .. } | Self::ConfirmAction { .. } => None,
        }
    }

    /// Gets the (mutable) cursor of a list popup
    pub fn list_cursor_mut(&mut self) -> Option<&mut Cursor> {
        match self {
            Self::DeviceList(list_state)
            | Self::UserPlaylistList(.., list_state)
            | Self::UserFollowedArtistList(list_state)
            | Self::UserSavedAlbumList(list_state)
            | Self::ArtistList(.., list_state)
            | Self::ThemeList(.., list_state)
            | Self::ActionList(.., list_state) => Some(list_state),
            Self::Search { .. } | Self::PlaylistCreate { .. } | Self::ConfirmAction { .. } => None,
        }
    }

    /// gets the selected position of a (list) popup
    #[must_use]
    pub fn list_selected(&self) -> Option<usize> {
        match self.list_cursor() {
            None => None,
            Some(state) => state.selected(),
        }
    }

    /// selects a position in a (list) popup
    pub fn list_select(&mut self, id: Option<usize>) {
        match self.list_cursor_mut() {
            None => {}
            Some(state) => state.select(id),
        }
    }
}

impl ActionListItem {
    #[must_use]
    pub fn n_actions(&self) -> usize {
        match self {
            ActionListItem::Track(.., actions)
            | ActionListItem::Artist(.., actions)
            | ActionListItem::Album(.., actions)
            | ActionListItem::Playlist(.., actions)
            | ActionListItem::Show(.., actions)
            | ActionListItem::Episode(.., actions) => actions.len(),
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            ActionListItem::Track(track, ..) => &track.name,
            ActionListItem::Artist(artist, ..) => &artist.name,
            ActionListItem::Album(album, ..) => &album.name,
            ActionListItem::Playlist(playlist, ..) => &playlist.name,
            ActionListItem::Show(show, ..) => &show.name,
            ActionListItem::Episode(episode, ..) => &episode.name,
        }
    }

    #[must_use]
    pub fn actions_desc(&self) -> Vec<String> {
        match self {
            ActionListItem::Track(.., actions)
            | ActionListItem::Artist(.., actions)
            | ActionListItem::Album(.., actions)
            | ActionListItem::Playlist(.., actions)
            | ActionListItem::Show(.., actions)
            | ActionListItem::Episode(.., actions) => {
                actions.iter().map(|a| format!("{a:?}")).collect::<Vec<_>>()
            }
        }
    }
}
