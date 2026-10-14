//! Command execution.
//!
//! Turns a [`Command`]/[`Action`] plus the current view model into `ClientRequest`s and
//! [`ViewState`] mutations. The event layer maps raw input into commands; everything the
//! command then *does* lives here so it stays independent of the frontend.

use crate::{
    client::{ClientRequest, PlayerRequest},
    clipboard::{execute_copy_command, get_clipboard_content},
    command::{
        self, construct_album_actions, construct_artist_actions, construct_playlist_actions,
        construct_show_actions, Action, ActionContext, ActionTarget, Command,
    },
    config,
    state::{
        ActionListItem, Album, AlbumId, Artist, ArtistFocusState, ArtistId, ArtistPopupAction,
        BrowsePageUIState, ConfirmableAction, Context, ContextId, ContextPageType,
        ContextPageUIState, Cursor, DataReadGuard, Episode, Focusable, Id, Item, ItemId,
        LibraryFocusState, LibraryPageUIState, MutableWindowState, PageState, PlayableId, Playback,
        PlaylistCreateCurrentField, PlaylistFolderItem, PlaylistId, PlaylistPopupAction,
        PopupState, SearchPageUIState, SharedState, Show, ShowFocusState, ShowId, TextInput, Track,
        TrackId, TrackOrder, TracksId, ViewStateGuard, USER_LIKED_TRACKS_ID,
        USER_RECENTLY_PLAYED_TRACKS_ID, USER_TOP_TRACKS_ID,
    },
    utils::{map_join, parse_uri},
};
use anyhow::{Context as _, Result};
use rand::RngExt;

pub fn handle_action_in_context(
    action: Action,
    context: ActionContext,
    client_pub: &flume::Sender<ClientRequest>,
    data: &DataReadGuard,
    view: &mut ViewStateGuard<'_>,
) -> Result<bool> {
    match context {
        ActionContext::Track(track) => match action {
            Action::GoToAlbum => {
                if let Some(album) = track.album {
                    let context_id = ContextId::Album(
                        AlbumId::from_uri(&parse_uri(&album.id.uri()))?.into_static(),
                    );
                    view.new_page(PageState::Context {
                        id: None,
                        context_page_type: ContextPageType::Browsing(context_id),
                        state: None,
                    });
                    return Ok(true);
                }
                Ok(false)
            }
            Action::GoToArtist => {
                handle_go_to_artist(track.artists, view);
                Ok(true)
            }
            Action::AddToQueue => {
                client_pub.send(ClientRequest::AddPlayableToQueue(track.id.into()))?;
                view.popup = None;
                Ok(true)
            }
            Action::CopyLink => {
                let track_url = format!("https://open.spotify.com/track/{}", track.id.id());
                execute_copy_command(track_url)?;
                view.popup = None;
                Ok(true)
            }
            Action::AddToPlaylist => {
                client_pub.send(ClientRequest::GetUserPlaylists)?;
                view.popup = Some(PopupState::UserPlaylistList(
                    PlaylistPopupAction::AddTrack {
                        folder_id: 0,
                        track_id: track.id,
                        search_query: String::new(),
                    },
                    Cursor::default(),
                ));
                Ok(true)
            }
            Action::ToggleLiked => {
                if data.user_data.is_liked_track(&track) {
                    client_pub.send(ClientRequest::DeleteFromLibrary(ItemId::Track(track.id)))?;
                } else {
                    client_pub.send(ClientRequest::AddToLibrary(Item::Track(track)))?;
                }
                view.popup = None;
                Ok(true)
            }
            Action::AddToLiked => {
                client_pub.send(ClientRequest::AddToLibrary(Item::Track(track)))?;
                view.popup = None;
                Ok(true)
            }
            Action::DeleteFromLiked => {
                client_pub.send(ClientRequest::DeleteFromLibrary(ItemId::Track(track.id)))?;
                view.popup = None;
                Ok(true)
            }
            Action::GoToRadio => {
                handle_go_to_radio(&track.id.uri(), &track.name, view, client_pub)?;
                Ok(true)
            }
            Action::ShowActionsOnArtist => {
                handle_show_actions_on_artist(track.artists, data, view);
                Ok(true)
            }
            Action::ShowActionsOnAlbum => {
                if let Some(album) = track.album {
                    let context = ActionContext::Album(album.clone());
                    view.popup = Some(PopupState::ActionList(
                        Box::new(ActionListItem::Album(
                            album,
                            context.get_available_actions(data),
                        )),
                        Cursor::default(),
                    ));
                    return Ok(true);
                }
                Ok(false)
            }
            Action::DeleteFromPlaylist => {
                if let PageState::Context {
                    id: Some(ContextId::Playlist(playlist_id)),
                    ..
                } = view.current_page()
                {
                    view.popup = Some(PopupState::ConfirmAction {
                        message: format!("Delete {} from this playlist?", track.name),
                        action: ConfirmableAction::DeleteTrackFromPlaylist {
                            playlist_id: playlist_id.clone_static(),
                            track_id: track.id,
                        },
                    });
                }
                Ok(true)
            }
            _ => Ok(false),
        },
        ActionContext::Album(album) => match action {
            Action::GoToArtist => {
                handle_go_to_artist(album.artists, view);
                Ok(true)
            }
            Action::GoToRadio => {
                handle_go_to_radio(&album.id.uri(), &album.name, view, client_pub)?;
                Ok(true)
            }
            Action::ShowActionsOnArtist => {
                handle_show_actions_on_artist(album.artists, data, view);
                Ok(true)
            }
            Action::AddToLibrary => {
                client_pub.send(ClientRequest::AddToLibrary(Item::Album(album)))?;
                view.popup = None;
                Ok(true)
            }
            Action::DeleteFromLibrary => {
                view.popup = Some(PopupState::ConfirmAction {
                    message: format!("Delete {} from your library?", album.name),
                    action: ConfirmableAction::DeleteFromLibrary(ItemId::Album(album.id)),
                });
                Ok(true)
            }
            Action::CopyLink => {
                let album_url = format!("https://open.spotify.com/album/{}", album.id.id());
                execute_copy_command(album_url)?;
                view.popup = None;
                Ok(true)
            }
            Action::AddToQueue => {
                client_pub.send(ClientRequest::AddAlbumToQueue(album.id))?;
                view.popup = None;
                Ok(true)
            }
            _ => Ok(false),
        },
        ActionContext::Artist(artist) => match action {
            Action::Follow => {
                client_pub.send(ClientRequest::AddToLibrary(Item::Artist(artist)))?;
                view.popup = None;
                Ok(true)
            }
            Action::Unfollow => {
                client_pub.send(ClientRequest::DeleteFromLibrary(ItemId::Artist(artist.id)))?;
                view.popup = None;
                Ok(true)
            }
            Action::CopyLink => {
                let artist_url = format!("https://open.spotify.com/artist/{}", artist.id.id());
                execute_copy_command(artist_url)?;
                view.popup = None;
                Ok(true)
            }
            Action::GoToRadio => {
                handle_go_to_radio(&artist.id.uri(), &artist.name, view, client_pub)?;
                Ok(true)
            }
            _ => Ok(false),
        },
        ActionContext::Playlist(playlist) => match action {
            Action::AddToLibrary => {
                client_pub.send(ClientRequest::AddToLibrary(Item::Playlist(playlist)))?;
                view.popup = None;
                Ok(true)
            }
            Action::GoToRadio => {
                handle_go_to_radio(&playlist.id.uri(), &playlist.name, view, client_pub)?;
                Ok(true)
            }
            Action::CopyLink => {
                let playlist_url =
                    format!("https://open.spotify.com/playlist/{}", playlist.id.id());
                execute_copy_command(playlist_url)?;
                view.popup = None;
                Ok(true)
            }
            Action::DeleteFromLibrary => {
                view.popup = Some(PopupState::ConfirmAction {
                    message: format!("Delete {} from your library?", playlist.name),
                    action: ConfirmableAction::DeleteFromLibrary(ItemId::Playlist(playlist.id)),
                });
                Ok(true)
            }
            _ => Ok(false),
        },
        ActionContext::Show(show) => match action {
            Action::CopyLink => {
                let show_url = format!("https://open.spotify.com/show/{}", show.id.id());
                execute_copy_command(show_url)?;
                view.popup = None;
                Ok(true)
            }
            Action::AddToLibrary => {
                client_pub.send(ClientRequest::AddToLibrary(Item::Show(show)))?;
                view.popup = None;
                Ok(true)
            }
            Action::DeleteFromLibrary => {
                view.popup = Some(PopupState::ConfirmAction {
                    message: format!("Delete {} from your library?", show.name),
                    action: ConfirmableAction::DeleteFromLibrary(ItemId::Show(show.id)),
                });
                Ok(true)
            }
            _ => Ok(false),
        },
        ActionContext::Episode(episode) => match action {
            Action::GoToShow => {
                if let Some(show) = episode.show {
                    let context_id = ContextId::Show(
                        ShowId::from_uri(&parse_uri(&show.id.uri()))?.into_static(),
                    );
                    view.new_page(PageState::Context {
                        id: None,
                        context_page_type: ContextPageType::Browsing(context_id),
                        state: None,
                    });
                    return Ok(true);
                }
                Ok(false)
            }
            Action::AddToQueue => {
                client_pub.send(ClientRequest::AddPlayableToQueue(episode.id.into()))?;
                view.popup = None;
                Ok(true)
            }
            Action::CopyLink => {
                let episode_url = format!("https://open.spotify.com/episode/{}", episode.id.id());
                execute_copy_command(episode_url)?;
                view.popup = None;
                Ok(true)
            }
            Action::AddToPlaylist => {
                client_pub.send(ClientRequest::GetUserPlaylists)?;
                view.popup = Some(PopupState::UserPlaylistList(
                    PlaylistPopupAction::AddEpisode {
                        folder_id: 0,
                        episode_id: episode.id,
                        search_query: String::new(),
                    },
                    Cursor::default(),
                ));
                Ok(true)
            }
            Action::ShowActionsOnShow => {
                if let Some(show) = episode.show {
                    let context = ActionContext::Show(show.clone());
                    view.popup = Some(PopupState::ActionList(
                        Box::new(ActionListItem::Show(
                            show,
                            context.get_available_actions(data),
                        )),
                        Cursor::default(),
                    ));
                    return Ok(true);
                }
                Ok(false)
            }
            _ => Ok(false),
        },
        // TODO: support actions for playlist folders
        ActionContext::PlaylistFolder(_) => Ok(false),
    }
}

fn handle_go_to_radio(
    seed_uri: &str,
    seed_name: &str,
    view: &mut ViewStateGuard<'_>,
    client_pub: &flume::Sender<ClientRequest>,
) -> anyhow::Result<()> {
    let radio_id = TracksId::new(format!("radio:{seed_uri}"), format!("{seed_name} Radio"));
    view.new_page(PageState::Context {
        id: None,
        context_page_type: ContextPageType::Browsing(ContextId::Tracks(radio_id.clone())),
        state: None,
    });
    client_pub.send(ClientRequest::GetContext(ContextId::Tracks(radio_id)))?;
    Ok(())
}

fn handle_go_to_artist(artists: Vec<Artist>, view: &mut ViewStateGuard<'_>) {
    if artists.len() == 1 {
        let context_id = ContextId::Artist(artists[0].id.clone());
        view.new_page(PageState::Context {
            id: None,
            context_page_type: ContextPageType::Browsing(context_id),
            state: None,
        });
    } else {
        view.popup = Some(PopupState::ArtistList(
            ArtistPopupAction::Browse,
            artists,
            Cursor::default(),
        ));
    }
}

fn handle_show_actions_on_artist(
    artists: Vec<Artist>,
    data: &DataReadGuard,
    view: &mut ViewStateGuard<'_>,
) {
    if artists.len() == 1 {
        let actions = construct_artist_actions(&artists[0], data);
        view.popup = Some(PopupState::ActionList(
            Box::new(ActionListItem::Artist(artists[0].clone(), actions)),
            Cursor::default(),
        ));
    } else {
        view.popup = Some(PopupState::ArtistList(
            ArtistPopupAction::ShowActions,
            artists,
            Cursor::default(),
        ));
    }
}

/// Handle a global action, currently this is only used to target
/// the currently playing item instead of the selection.
pub fn handle_global_action(
    action: Action,
    target: ActionTarget,
    client_pub: &flume::Sender<ClientRequest>,
    state: &SharedState,
    view: &mut ViewStateGuard<'_>,
) -> Result<bool> {
    if target == ActionTarget::PlayingTrack {
        // the shared order: `data` before `player` (see `state::State`)
        let data = state.data.read();
        let player = state.player.read();

        if let Some(currently_playing) = player.currently_playing() {
            match currently_playing {
                rspotify::model::PlayableItem::Track(track) => {
                    if let Some(track) = Track::try_from_full_track(track.clone()) {
                        return handle_action_in_context(
                            action,
                            ActionContext::Track(track),
                            client_pub,
                            &data,
                            view,
                        );
                    }
                }
                rspotify::model::PlayableItem::Episode(episode) => {
                    return handle_action_in_context(
                        action,
                        ActionContext::Episode(episode.clone().into()),
                        client_pub,
                        &data,
                        view,
                    );
                }
                rspotify::model::PlayableItem::Unknown(_) => {
                    return Ok(false);
                }
            }
        }
    }

    Ok(false)
}

/// Resolve the seek step for the current playback item. Podcasts/episodes use a larger default
/// step than tracks.
fn current_seek_duration(state: &SharedState) -> u16 {
    let configs = config::get_config();
    let is_episode = state
        .player
        .read()
        .currently_playing()
        .is_some_and(|item| matches!(item, rspotify::model::PlayableItem::Episode(_)));
    if is_episode {
        configs.app_config.seek_duration_secs_podcast
    } else {
        configs.app_config.seek_duration_secs
    }
}

/// Handle a global command that is not specific to any page/popup
///
/// # Panics
///
/// Panics if an internal seek-duration conversion fails, which cannot happen for the
/// fixed, bounded durations used here.
pub fn handle_global_command(
    command: Command,
    client_pub: &flume::Sender<ClientRequest>,
    state: &SharedState,
    view: &mut ViewStateGuard<'_>,
) -> Result<bool> {
    match command {
        Command::Quit => {
            view.is_running = false;
        }
        Command::NextTrack => {
            client_pub.send(ClientRequest::Player(PlayerRequest::NextTrack))?;
        }
        Command::PreviousTrack => {
            client_pub.send(ClientRequest::Player(PlayerRequest::PreviousTrack))?;
        }
        Command::ResumePause => {
            client_pub.send(ClientRequest::Player(PlayerRequest::ResumePause))?;
        }
        Command::Repeat => {
            client_pub.send(ClientRequest::Player(PlayerRequest::Repeat))?;
        }
        Command::Shuffle => {
            client_pub.send(ClientRequest::Player(PlayerRequest::Shuffle))?;
        }
        Command::VolumeChange { offset } => {
            let mut volume_target = state.volume_target.lock();
            let base_u8 = volume_target
                .or_else(|| {
                    state
                        .player
                        .read()
                        .buffered_playback
                        .as_ref()
                        .and_then(|p| p.volume.map(|v| v as u8))
                })
                .unwrap_or(0);
            let base = i32::from(base_u8);
            let new_volume = std::cmp::min(base + offset, 100_i32).max(0);
            *volume_target = Some(new_volume as u8);
            client_pub.send(ClientRequest::Player(PlayerRequest::Volume(
                new_volume as u8,
            )))?;
        }
        Command::Mute => {
            client_pub.send(ClientRequest::Player(PlayerRequest::ToggleMute))?;
        }
        Command::SeekStart => {
            client_pub.send(ClientRequest::Player(PlayerRequest::SeekTrack(
                chrono::TimeDelta::try_seconds(0).unwrap(),
            )))?;
        }

        Command::SeekForward { duration } => {
            if let Some(progress) = state.player.read().playback_progress() {
                let duration = duration.unwrap_or(current_seek_duration(state));
                client_pub.send(ClientRequest::Player(PlayerRequest::SeekTrack(
                    progress + chrono::Duration::try_seconds(i64::from(duration)).unwrap(),
                )))?;
            }
        }
        Command::SeekBackward { duration } => {
            if let Some(progress) = state.player.read().playback_progress() {
                let duration = duration.unwrap_or(current_seek_duration(state));
                client_pub.send(ClientRequest::Player(PlayerRequest::SeekTrack(
                    std::cmp::max(
                        chrono::Duration::zero(),
                        progress - chrono::Duration::try_seconds(i64::from(duration)).unwrap(),
                    ),
                )))?;
            }
        }
        Command::OpenCommandHelp => {
            view.new_page(PageState::CommandHelp { scroll_offset: 0 });
        }
        Command::OpenLogs => {
            view.new_page(PageState::Logs { scroll_offset: 0 });
        }
        Command::RefreshPlayback => {
            client_pub.send(ClientRequest::GetCurrentPlayback)?;
        }
        Command::ShowActionsOnCurrentTrack => {
            if let Some(currently_playing) = state.player.read().currently_playing() {
                match currently_playing {
                    rspotify::model::PlayableItem::Track(track) => {
                        if let Some(track) = Track::try_from_full_track(track.clone()) {
                            let data = state.data.read();
                            let actions = command::construct_track_actions(&track, &data);
                            view.popup = Some(PopupState::ActionList(
                                Box::new(ActionListItem::Track(track, actions)),
                                Cursor::default(),
                            ));
                        }
                    }
                    rspotify::model::PlayableItem::Episode(episode) => {
                        let episode = episode.clone().into();
                        let data = state.data.read();
                        let actions = command::construct_episode_actions(&episode, &data);
                        view.popup = Some(PopupState::ActionList(
                            Box::new(ActionListItem::Episode(episode, actions)),
                            Cursor::default(),
                        ));
                    }
                    rspotify::model::PlayableItem::Unknown(_) => {}
                }
            }
        }
        Command::CurrentlyPlayingContextPage => {
            view.new_page(PageState::Context {
                id: None,
                context_page_type: ContextPageType::CurrentPlaying,
                state: None,
            });
        }
        Command::BrowseUserPlaylists => {
            client_pub.send(ClientRequest::GetUserPlaylists)?;
            view.popup = Some(PopupState::UserPlaylistList(
                PlaylistPopupAction::Browse {
                    folder_id: 0,
                    search_query: String::new(),
                },
                Cursor::default(),
            ));
        }
        Command::BrowseUserFollowedArtists => {
            client_pub.send(ClientRequest::GetUserFollowedArtists)?;
            view.popup = Some(PopupState::UserFollowedArtistList(Cursor::default()));
        }
        Command::BrowseUserSavedAlbums => {
            client_pub.send(ClientRequest::GetUserSavedAlbums)?;
            view.popup = Some(PopupState::UserSavedAlbumList(Cursor::default()));
        }
        Command::TopTrackPage => {
            view.new_page(PageState::Context {
                id: None,
                context_page_type: ContextPageType::Browsing(ContextId::Tracks(
                    USER_TOP_TRACKS_ID.to_owned(),
                )),
                state: None,
            });
            client_pub.send(ClientRequest::GetContext(ContextId::Tracks(
                USER_TOP_TRACKS_ID.to_owned(),
            )))?;
        }
        Command::RecentlyPlayedTrackPage => {
            view.new_page(PageState::Context {
                id: None,
                context_page_type: ContextPageType::Browsing(ContextId::Tracks(
                    USER_RECENTLY_PLAYED_TRACKS_ID.to_owned(),
                )),
                state: None,
            });
            client_pub.send(ClientRequest::GetContext(ContextId::Tracks(
                USER_RECENTLY_PLAYED_TRACKS_ID.to_owned(),
            )))?;
        }
        Command::LikedTrackPage => {
            view.new_page(PageState::Context {
                id: None,
                context_page_type: ContextPageType::Browsing(ContextId::Tracks(
                    USER_LIKED_TRACKS_ID.to_owned(),
                )),
                state: None,
            });
            client_pub.send(ClientRequest::GetContext(ContextId::Tracks(
                USER_LIKED_TRACKS_ID.to_owned(),
            )))?;
        }
        Command::LibraryPage => {
            view.new_page(PageState::Library {
                state: LibraryPageUIState::new(),
            });
        }
        Command::SearchPage => {
            view.new_page(PageState::Search {
                line_input: TextInput::default(),
                current_query: String::new(),
                state: SearchPageUIState::new(),
            });
        }
        Command::BrowsePage => {
            view.new_page(PageState::Browse {
                state: BrowsePageUIState::CategoryList {
                    state: Cursor::default(),
                },
            });
            client_pub.send(ClientRequest::GetBrowseCategories)?;
        }
        Command::PreviousPage => {
            if view.history.len() > 1 {
                view.history.pop();
                view.popup = None;
            }
        }
        Command::OpenSpotifyLinkFromClipboard => {
            let content = get_clipboard_content().context("get clipboard's content")?;
            let re = regex::Regex::new(
                r"https://open.spotify.com/(?P<type>.*?)/(?P<id>[[:alnum:]]*).*",
            )?;
            if let Some(cap) = re.captures(&content) {
                let typ = cap.name("type").expect("valid capture").as_str();
                let id = cap.name("id").expect("valid capture").as_str();
                match typ {
                    // for track link, play the song
                    "track" => {
                        let id = TrackId::from_id(id)?.into_static();

                        // Clear Tracks context when playing from clipboard link
                        state.player.write().currently_playing_tracks_id = None;

                        client_pub.send(ClientRequest::Player(PlayerRequest::StartPlayback(
                            Playback::URIs(vec![id.into()], None),
                            None,
                        )))?;
                    }
                    // for playlist/artist/album link, go to the corresponding context page
                    "playlist" => {
                        let id = PlaylistId::from_id(id)?.into_static();
                        view.new_page(PageState::Context {
                            id: None,
                            context_page_type: ContextPageType::Browsing(ContextId::Playlist(id)),
                            state: None,
                        });
                    }
                    "artist" => {
                        let id = ArtistId::from_id(id)?.into_static();
                        view.new_page(PageState::Context {
                            id: None,
                            context_page_type: ContextPageType::Browsing(ContextId::Artist(id)),
                            state: None,
                        });
                    }
                    "album" => {
                        let id = AlbumId::from_id(id)?.into_static();
                        view.new_page(PageState::Context {
                            id: None,
                            context_page_type: ContextPageType::Browsing(ContextId::Album(id)),
                            state: None,
                        });
                    }
                    e => anyhow::bail!("unsupported Spotify type {e}!"),
                }
            } else {
                tracing::warn!("clipboard's content ({content}) is not a valid Spotify link!");
            }
        }
        Command::LyricsPage => {
            if let Some(rspotify::model::PlayableItem::Track(track)) =
                state.player.read().currently_playing()
            {
                if let Some(id) = &track.id {
                    let artists = map_join(&track.artists, |a| &a.name, ", ");
                    view.new_page(PageState::Lyrics {
                        track_uri: id.uri(),
                        track: track.name.clone(),
                        artists,
                    });

                    client_pub.send(ClientRequest::GetLyrics {
                        track_id: id.clone_static(),
                    })?;
                }
            }
        }
        Command::SwitchDevice => {
            view.popup = Some(PopupState::DeviceList(Cursor::default()));
            client_pub.send(ClientRequest::GetDevices)?;
        }
        #[cfg(feature = "streaming")]
        Command::RestartIntegratedClient => {
            client_pub.send(ClientRequest::RestartIntegratedClient)?;
        }
        Command::FocusNextWindow => {
            if !view.has_focused_popup() {
                view.current_page_mut().next();
            }
        }
        Command::FocusPreviousWindow => {
            if !view.has_focused_popup() {
                view.current_page_mut().previous();
            }
        }
        Command::Queue => {
            view.new_page(PageState::Queue { scroll_offset: 0 });
            client_pub.send(ClientRequest::GetCurrentUserQueue)?;
        }
        Command::CreatePlaylist => {
            view.popup = Some(PopupState::PlaylistCreate {
                name: TextInput::default(),
                desc: TextInput::default(),
                current_field: PlaylistCreateCurrentField::Name,
            });
        }
        Command::JumpToCurrentTrackInContext => {
            let track_id = match state.player.read().currently_playing() {
                Some(rspotify::model::PlayableItem::Track(track)) => {
                    PlayableId::Track(track.id.clone().expect("all non-local tracks have ids"))
                }
                Some(rspotify::model::PlayableItem::Episode(episode)) => {
                    PlayableId::Episode(episode.id.clone())
                }
                Some(rspotify::model::PlayableItem::Unknown(_)) | None => return Ok(false),
            };

            if let PageState::Context {
                id: Some(context_id),
                ..
            } = view.current_page()
            {
                let context_track_pos = state
                    .data
                    .read()
                    .context_tracks(context_id)
                    .and_then(|tracks| tracks.iter().position(|t| t.id.uri() == track_id.uri()));

                if let Some(p) = context_track_pos {
                    view.current_page_mut().select(p);
                }
            }
        }
        Command::ClosePopup => {
            view.popup = None;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

pub fn handle_action_for_library_page(
    action: Action,
    client_pub: &flume::Sender<ClientRequest>,
    view: &mut ViewStateGuard<'_>,
    state: &SharedState,
) -> Result<bool> {
    let data = state.data.read();
    let (focus_state, folder_id) = match view.current_page() {
        PageState::Library { state } => (state.focus, state.playlist_folder_id),
        _ => anyhow::bail!("expect a library page state"),
    };
    match focus_state {
        LibraryFocusState::Playlists => handle_action_for_selected_item(
            action,
            &view
                .search_filtered_items(&data.user_data.folder_playlists_items(folder_id))
                .into_iter()
                .copied()
                .collect::<Vec<_>>(),
            &data,
            view,
            client_pub,
        ),
        LibraryFocusState::SavedAlbums => handle_action_for_selected_item(
            action,
            &view.search_filtered_items(&data.user_data.saved_albums),
            &data,
            view,
            client_pub,
        ),
        LibraryFocusState::FollowedArtists => handle_action_for_selected_item(
            action,
            &view.search_filtered_items(&data.user_data.followed_artists),
            &data,
            view,
            client_pub,
        ),
        LibraryFocusState::SavedShows => handle_action_for_selected_item(
            action,
            &view.search_filtered_items(&data.user_data.saved_shows),
            &data,
            view,
            client_pub,
        ),
    }
}

pub fn handle_command_for_library_page(
    command: Command,
    client_pub: &flume::Sender<ClientRequest>,
    view: &mut ViewStateGuard<'_>,
    state: &SharedState,
    count_prefix: Option<usize>,
) -> Result<bool> {
    if command == Command::Search {
        view.new_search_popup();
        return Ok(true);
    }

    let (focus_state, folder_id) = match view.current_page() {
        PageState::Library { state } => (state.focus, state.playlist_folder_id),
        _ => anyhow::bail!("expect a library page state"),
    };

    if command == Command::SortLibraryAlphabetically {
        let mut data = state.data.write();

        // Sort playlists alphabetically, keeping folders on top
        data.user_data.playlists.sort_by(|a, b| match (a, b) {
            (PlaylistFolderItem::Folder(_), PlaylistFolderItem::Playlist(_)) => {
                std::cmp::Ordering::Less
            }
            (PlaylistFolderItem::Playlist(_), PlaylistFolderItem::Folder(_)) => {
                std::cmp::Ordering::Greater
            }
            _ => a
                .to_string()
                .to_lowercase()
                .cmp(&b.to_string().to_lowercase()),
        });

        // Sort albums alphabetically
        data.user_data
            .saved_albums
            .sort_by_key(|x| x.name.to_lowercase());

        // Sort artists alphabetically
        data.user_data
            .followed_artists
            .sort_by_key(|x| x.name.to_lowercase());

        // Sort shows alphabetically
        data.user_data
            .saved_shows
            .sort_by_key(|x| x.name.to_lowercase());
    }

    if command == Command::SortLibraryByRecent {
        let mut data = state.data.write();

        // Sort playlists by `current_folder_id` and then by `snapshot_id`
        data.user_data.playlists.sort_by(|a, b| {
            match (a, b) {
                (PlaylistFolderItem::Playlist(p1), PlaylistFolderItem::Playlist(p2)) => {
                    if p1.current_folder_id == p2.current_folder_id {
                        p1.snapshot_id.cmp(&p2.snapshot_id)
                    } else {
                        p1.current_folder_id.cmp(&p2.current_folder_id)
                    }
                }
                (PlaylistFolderItem::Folder(_), PlaylistFolderItem::Playlist(_)) => {
                    std::cmp::Ordering::Less
                }
                (PlaylistFolderItem::Playlist(_), PlaylistFolderItem::Folder(_)) => {
                    std::cmp::Ordering::Greater
                }
                _ => std::cmp::Ordering::Equal, // Keep folders in place
            }
        });

        // Sort albums by recent addition
        data.user_data
            .saved_albums
            .sort_by_key(|a| std::cmp::Reverse(a.added_at));
    }

    match focus_state {
        LibraryFocusState::Playlists => {
            let data = state.data.read();
            Ok(handle_command_for_playlist_list_window(
                command,
                &view
                    .search_filtered_items(&data.user_data.folder_playlists_items(folder_id))
                    .into_iter()
                    .copied()
                    .collect::<Vec<_>>(),
                &data,
                view,
                count_prefix,
            ))
        }
        LibraryFocusState::SavedAlbums => {
            // Use a read lock for the function call
            let data = state.data.read();
            handle_command_for_album_list_window(
                command,
                &view.search_filtered_items(&data.user_data.saved_albums),
                &data,
                view,
                client_pub,
                count_prefix,
            )
        }
        LibraryFocusState::FollowedArtists => {
            // Handle artist-specific commands
            let data = state.data.read();
            Ok(handle_command_for_artist_list_window(
                command,
                &view.search_filtered_items(&data.user_data.followed_artists),
                &data,
                view,
                count_prefix,
            ))
        }
        LibraryFocusState::SavedShows => {
            let data = state.data.read();
            Ok(handle_command_for_show_list_window(
                command,
                &view.search_filtered_items(&data.user_data.saved_shows),
                &data,
                view,
                count_prefix,
            ))
        }
    }
}

pub fn handle_command_for_context_page(
    command: Command,
    client_pub: &flume::Sender<ClientRequest>,
    view: &mut ViewStateGuard<'_>,
    state: &SharedState,
    count_prefix: Option<usize>,
) -> Result<bool> {
    match command {
        Command::Search => {
            view.new_search_popup();
            Ok(true)
        }
        Command::ShowActionsOnCurrentContext => {
            let context_id = match view.current_page() {
                PageState::Context { id, .. } => match id {
                    None => return Ok(false),
                    Some(id) => id,
                },
                _ => anyhow::bail!("expect a context page"),
            };
            let data = state.data.read();

            match data.caches.context.get(&context_id.uri()) {
                Some(context) => match context {
                    Context::Playlist { playlist, .. } => {
                        let actions = construct_playlist_actions(playlist, &data);
                        view.popup = Some(PopupState::ActionList(
                            Box::new(ActionListItem::Playlist(playlist.clone(), actions)),
                            Cursor::default(),
                        ));
                        Ok(true)
                    }
                    Context::Album { album, .. } => {
                        let actions = construct_album_actions(album, &data);
                        view.popup = Some(PopupState::ActionList(
                            Box::new(ActionListItem::Album(album.clone(), actions)),
                            Cursor::default(),
                        ));
                        Ok(true)
                    }
                    Context::Artist { artist, .. } => {
                        let actions = construct_artist_actions(artist, &data);
                        view.popup = Some(PopupState::ActionList(
                            Box::new(ActionListItem::Artist(artist.clone(), actions)),
                            Cursor::default(),
                        ));
                        Ok(true)
                    }
                    Context::Show { show, .. } => {
                        let actions = construct_show_actions(show, &data);
                        view.popup = Some(PopupState::ActionList(
                            Box::new(ActionListItem::Show(show.clone(), actions)),
                            Cursor::default(),
                        ));
                        Ok(true)
                    }
                    Context::Tracks { tracks: _, desc: _ } => Ok(false),
                },
                None => Ok(false),
            }
        }
        _ => handle_command_for_focused_context_window(
            command,
            client_pub,
            view,
            state,
            count_prefix,
        ),
    }
}

pub fn handle_action_for_browse_page(
    action: Action,
    client_pub: &flume::Sender<ClientRequest>,
    view: &mut ViewStateGuard<'_>,
    state: &SharedState,
) -> Result<bool> {
    let data = state.data.read();

    match view.current_page() {
        PageState::Browse { state } => match state {
            BrowsePageUIState::CategoryPlaylistList { category, .. } => {
                let Some(playlists) = data.browse.category_playlists.get(&category.id) else {
                    return Ok(false);
                };

                let page_state = view.current_page_mut();
                let selected = page_state.selected().unwrap_or_default();
                if selected >= playlists.len() {
                    return Ok(false);
                }

                handle_action_in_context(
                    action,
                    playlists[selected].clone().into(),
                    client_pub,
                    &data,
                    view,
                )?;

                Ok(true)
            }
            BrowsePageUIState::CategoryList { .. } => Ok(false),
        },
        _ => anyhow::bail!("expect a browse page state"),
    }
}

pub fn handle_command_for_browse_page(
    command: Command,
    client_pub: &flume::Sender<ClientRequest>,
    view: &mut ViewStateGuard<'_>,
    state: &SharedState,
    count_prefix: Option<usize>,
) -> Result<bool> {
    let data = state.data.read();

    let len = match view.current_page() {
        PageState::Browse { state } => match state {
            BrowsePageUIState::CategoryList { .. } => {
                view.search_filtered_items(&data.browse.categories).len()
            }
            BrowsePageUIState::CategoryPlaylistList { category, .. } => data
                .browse
                .category_playlists
                .get(&category.id)
                .map(|v| view.search_filtered_items(v).len())
                .unwrap_or_default(),
        },
        _ => anyhow::bail!("expect a browse page state"),
    };

    let page_state = view.current_page_mut();
    let selected = page_state.selected().unwrap_or_default();
    if selected >= len {
        return Ok(false);
    }

    if handle_navigation_command(command, page_state, selected, len, count_prefix) {
        return Ok(true);
    }
    match command {
        Command::ChooseSelected => match page_state {
            PageState::Browse { state } => match state {
                BrowsePageUIState::CategoryList { .. } => {
                    let categories = view.search_filtered_items(&data.browse.categories);
                    client_pub.send(ClientRequest::GetBrowseCategoryPlaylists(
                        categories[selected].clone(),
                    ))?;
                    view.new_page(PageState::Browse {
                        state: BrowsePageUIState::CategoryPlaylistList {
                            category: categories[selected].clone(),
                            state: Cursor::default(),
                        },
                    });
                }
                BrowsePageUIState::CategoryPlaylistList { category, .. } => {
                    let playlists =
                        data.browse
                            .category_playlists
                            .get(&category.id)
                            .context(format!(
                                "expect to have playlists data for {category} category"
                            ))?;
                    let context_id = ContextId::Playlist(
                        view.search_filtered_items(playlists)[selected].id.clone(),
                    );
                    view.new_page(PageState::Context {
                        id: None,
                        context_page_type: ContextPageType::Browsing(context_id),
                        state: None,
                    });
                }
            },
            _ => anyhow::bail!("expect a browse page state"),
        },
        Command::Search => {
            view.new_search_popup();
        }
        _ => return Ok(false),
    }
    Ok(true)
}

pub fn handle_command_for_queue_page(
    command: Command,
    view: &mut ViewStateGuard<'_>,
    count_prefix: Option<usize>,
) -> bool {
    let scroll_offset = match view.current_page() {
        PageState::Queue { scroll_offset } => *scroll_offset,
        _ => return false,
    };
    handle_navigation_command(
        command,
        view.current_page_mut(),
        scroll_offset,
        10000,
        count_prefix,
    )
}

pub fn handle_command_for_command_help_page(
    command: Command,
    view: &mut ViewStateGuard<'_>,
    count_prefix: Option<usize>,
) -> bool {
    let scroll_offset = match view.current_page() {
        PageState::CommandHelp { scroll_offset } => *scroll_offset,
        _ => return false,
    };
    if command == Command::Search {
        view.new_search_popup();
        return true;
    }
    handle_navigation_command(
        command,
        view.current_page_mut(),
        scroll_offset,
        10000,
        count_prefix,
    )
}

pub fn handle_command_for_logs_page(
    command: Command,
    view: &mut ViewStateGuard<'_>,
    count_prefix: Option<usize>,
) -> bool {
    let scroll_offset = match view.current_page() {
        PageState::Logs { scroll_offset } => *scroll_offset,
        _ => return false,
    };
    handle_navigation_command(
        command,
        view.current_page_mut(),
        scroll_offset,
        10000,
        count_prefix,
    )
}

pub fn handle_navigation_command(
    command: Command,
    page: &mut PageState,
    id: usize,
    len: usize,
    count: Option<usize>,
) -> bool {
    if len == 0 {
        return false;
    }

    let configs = config::get_config();
    match command {
        Command::SelectNextOrScrollDown => {
            let offset = count.unwrap_or(1);
            page.select(std::cmp::min(id + offset, len - 1));
            true
        }
        Command::SelectPreviousOrScrollUp => {
            let offset = count.unwrap_or(1);
            page.select(id.saturating_sub(offset));
            true
        }
        Command::PageSelectNextOrScrollDown => {
            let page_size = configs.app_config.page_size_in_rows;
            let offset = count.unwrap_or(1) * page_size;
            page.select(std::cmp::min(id + offset, len - 1));
            true
        }
        Command::PageSelectPreviousOrScrollUp => {
            let page_size = configs.app_config.page_size_in_rows;
            let offset = count.unwrap_or(1) * page_size;
            page.select(id.saturating_sub(offset));
            true
        }
        Command::SelectLastOrScrollToBottom => {
            page.select(len - 1);
            true
        }
        Command::SelectFirstOrScrollToTop => {
            page.select(0);
            true
        }
        _ => false,
    }
}

pub fn handle_action_for_focused_context_page(
    action: Action,
    client_pub: &flume::Sender<ClientRequest>,
    view: &mut ViewStateGuard<'_>,
    state: &SharedState,
) -> Result<bool> {
    let PageState::Context { id: Some(id), .. } = view.current_page() else {
        return Ok(false);
    };

    let data = state.data.read();
    match data.caches.context.get(&id.uri()) {
        Some(Context::Artist {
            artist,
            top_tracks,
            albums,
            related_artists,
        }) => {
            let PageState::Context {
                state: Some(ContextPageUIState::Artist { focus, .. }),
                ..
            } = view.current_page()
            else {
                return Ok(false);
            };

            match focus {
                ArtistFocusState::Albums => handle_action_for_selected_item(
                    action,
                    &view.search_filtered_items(albums),
                    &data,
                    view,
                    client_pub,
                ),
                ArtistFocusState::RelatedArtists => handle_action_for_selected_item(
                    action,
                    &view.search_filtered_items(related_artists),
                    &data,
                    view,
                    client_pub,
                ),
                ArtistFocusState::TopTracks => handle_action_for_selected_item(
                    action,
                    &view.search_filtered_items(top_tracks),
                    &data,
                    view,
                    client_pub,
                ),
                ArtistFocusState::LikedSongs => {
                    let liked = data.user_data.liked_tracks_by_artist(artist);
                    let filtered = view.search_filtered_items(&liked);
                    handle_action_for_selected_item(action, &filtered, &data, view, client_pub)
                }
            }
        }
        Some(
            Context::Album { tracks, .. }
            | Context::Tracks { tracks, .. }
            | Context::Playlist { tracks, .. },
        ) => handle_action_for_selected_item(
            action,
            &view.search_filtered_items(tracks),
            &data,
            view,
            client_pub,
        ),
        Some(Context::Show { episodes, .. }) => handle_action_for_selected_item(
            action,
            &view.search_filtered_items(episodes),
            &data,
            view,
            client_pub,
        ),
        None => Ok(false),
    }
}

pub fn handle_action_for_selected_item<T: Into<ActionContext> + Clone>(
    action: Action,
    items: &[&T],
    data: &DataReadGuard,
    view: &mut ViewStateGuard<'_>,
    client_pub: &flume::Sender<ClientRequest>,
) -> Result<bool> {
    let id = view.current_page_mut().selected().unwrap_or_default();
    if id >= items.len() {
        return Ok(false);
    }

    handle_action_in_context(action, items[id].clone().into(), client_pub, data, view)
}

/// Handle a command for the currently focused context window
///
/// The function will need to determine the focused window then
/// assign the handling job to the window's command handler
pub fn handle_command_for_focused_context_window(
    command: Command,
    client_pub: &flume::Sender<ClientRequest>,
    view: &mut ViewStateGuard<'_>,
    state: &SharedState,
    count_prefix: Option<usize>,
) -> Result<bool> {
    let context_id = match view.current_page() {
        PageState::Context { id, .. } => match id {
            None => return Ok(false),
            Some(id) => id,
        },
        _ => anyhow::bail!("expect a context page"),
    };

    // handle commands that require access to data's mutable state
    {
        let order = match command {
            Command::SortTrackByTitle => Some(TrackOrder::TrackName),
            Command::SortTrackByAlbum => Some(TrackOrder::Album),
            Command::SortTrackByArtists => Some(TrackOrder::Artists),
            Command::SortTrackByAddedDate => Some(TrackOrder::AddedAt),
            Command::SortTrackByDuration => Some(TrackOrder::Duration),
            _ => None,
        };

        // sort ordering commands
        if let Some(order) = order {
            let mut data = state.data.write();
            if let Some(tracks) = data.context_tracks_mut(context_id) {
                tracks.sort_by(|x, y| order.compare(x, y));
            }
            return Ok(true);
        }
        // reverse ordering command
        if command == Command::ReverseTrackOrder {
            let mut data = state.data.write();
            if let Some(tracks) = data.context_tracks_mut(context_id) {
                tracks.reverse();
            }
            return Ok(true);
        }
    }

    let data = state.data.read();

    match data.caches.context.get(&context_id.uri()) {
        Some(context) => match context {
            Context::Artist {
                artist,
                top_tracks,
                albums,
                related_artists,
            } => {
                let PageState::Context {
                    state: Some(ContextPageUIState::Artist { focus, .. }),
                    ..
                } = view.current_page()
                else {
                    anyhow::bail!("expect an arist context page with a state")
                };

                match focus {
                    ArtistFocusState::Albums => handle_command_for_album_list_window(
                        command,
                        &view.search_filtered_items(albums),
                        &data,
                        view,
                        client_pub,
                        count_prefix,
                    ),
                    ArtistFocusState::RelatedArtists => Ok(handle_command_for_artist_list_window(
                        command,
                        &view.search_filtered_items(related_artists),
                        &data,
                        view,
                        count_prefix,
                    )),
                    ArtistFocusState::TopTracks => handle_command_for_track_table_window(
                        command,
                        client_pub,
                        None,
                        top_tracks,
                        &data,
                        view,
                        state,
                        count_prefix,
                    ),
                    ArtistFocusState::LikedSongs => {
                        let liked = data.user_data.liked_tracks_by_artist(artist);
                        handle_command_for_track_table_window(
                            command,
                            client_pub,
                            None,
                            &liked,
                            &data,
                            view,
                            state,
                            count_prefix,
                        )
                    }
                }
            }
            Context::Album { tracks, .. }
            | Context::Playlist { tracks, .. }
            | Context::Tracks { tracks, .. } => handle_command_for_track_table_window(
                command,
                client_pub,
                Some(context_id.clone()),
                tracks,
                &data,
                view,
                state,
                count_prefix,
            ),
            Context::Show { show, episodes, .. } => {
                let focus = match view.current_page() {
                    PageState::Context {
                        state: Some(ContextPageUIState::Show { focus, .. }),
                        ..
                    } => *focus,
                    _ => ShowFocusState::Episodes,
                };
                match focus {
                    ShowFocusState::Episodes => handle_command_for_episode_table_window(
                        command,
                        client_pub,
                        &show.id,
                        &view.search_filtered_items(episodes),
                        &data,
                        view,
                        state,
                        count_prefix,
                    ),
                    ShowFocusState::Details => {
                        if handle_command_for_episode_detail_window(command, view) {
                            Ok(true)
                        } else {
                            handle_command_for_episode_table_window(
                                command,
                                client_pub,
                                &show.id,
                                &view.search_filtered_items(episodes),
                                &data,
                                view,
                                state,
                                count_prefix,
                            )
                        }
                    }
                }
            }
        },
        None => Ok(false),
    }
}

/// Handle navigation/scroll commands when the episode detail window is focused, scrolling the
/// wrapped description body.
fn handle_command_for_episode_detail_window(
    command: Command,
    view: &mut ViewStateGuard<'_>,
) -> bool {
    let PageState::Context {
        state: Some(ContextPageUIState::Show {
            description_scroll, ..
        }),
        ..
    } = view.current_page_mut()
    else {
        return false;
    };

    let page = crate::config::get_config()
        .app_config
        .layout
        .detail_window_height as u16;
    match command {
        Command::SelectNextOrScrollDown => {
            *description_scroll = description_scroll.saturating_add(1);
        }
        Command::SelectPreviousOrScrollUp => {
            *description_scroll = description_scroll.saturating_sub(1);
        }
        Command::PageSelectNextOrScrollDown => {
            *description_scroll = description_scroll.saturating_add(page);
        }
        Command::PageSelectPreviousOrScrollUp => {
            *description_scroll = description_scroll.saturating_sub(page);
        }
        Command::SelectFirstOrScrollToTop => *description_scroll = 0,
        Command::SelectLastOrScrollToBottom => *description_scroll = u16::MAX,
        _ => return false,
    }
    true
}

/// Handle commands that may modify a playlist
fn handle_playlist_modify_command(
    id: usize,
    playlist_id: &PlaylistId<'static>,
    command: Command,
    client_pub: &flume::Sender<ClientRequest>,
    tracks: &[&Track],
    data: &DataReadGuard,
    view: &mut ViewStateGuard<'_>,
) -> Result<bool> {
    match command {
        Command::MovePlaylistItemUp => {
            if id > 0 {
                client_pub.send(ClientRequest::ReorderPlaylistItems {
                    playlist_id: playlist_id.clone_static(),
                    insert_index: id - 1,
                    range_start: id,
                    range_length: None,
                    snapshot_id: None,
                })?;
                view.current_page_mut().select(id - 1);
            }
            return Ok(true);
        }
        Command::MovePlaylistItemDown => {
            if id + 1 < tracks.len() {
                client_pub.send(ClientRequest::ReorderPlaylistItems {
                    playlist_id: playlist_id.clone_static(),
                    insert_index: id + 1,
                    range_start: id,
                    range_length: None,
                    snapshot_id: None,
                })?;
                view.current_page_mut().select(id + 1);
            }
            return Ok(true);
        }
        Command::ShowActionsOnSelectedItem => {
            let mut actions = command::construct_track_actions(tracks[id], data);
            actions.push(Action::DeleteFromPlaylist);
            view.popup = Some(PopupState::ActionList(
                Box::new(ActionListItem::Track(tracks[id].clone(), actions)),
                Cursor::default(),
            ));
            return Ok(true);
        }
        _ => {}
    }

    Ok(false)
}

#[allow(clippy::too_many_arguments)]
fn handle_command_for_track_table_window(
    command: Command,
    client_pub: &flume::Sender<ClientRequest>,
    context_id: Option<ContextId>,
    tracks: &[Track],
    data: &DataReadGuard,
    view: &mut ViewStateGuard<'_>,
    state: &SharedState,
    count_prefix: Option<usize>,
) -> Result<bool> {
    let id = view.current_page_mut().selected().unwrap_or_default();
    let filtered_tracks = view.search_filtered_items(tracks);
    if id >= filtered_tracks.len() {
        return Ok(false);
    }

    if let Some(ContextId::Playlist(ref playlist_id)) = context_id {
        let modifiable =
            data.user_data.modifiable_playlist_items(None).iter().any(
                |item| matches!(item, PlaylistFolderItem::Playlist(p) if p.id.eq(playlist_id)),
            );
        if modifiable
            && handle_playlist_modify_command(
                id,
                playlist_id,
                command,
                client_pub,
                &filtered_tracks,
                data,
                view,
            )?
        {
            return Ok(true);
        }
    }

    if handle_navigation_command(
        command,
        view.current_page_mut(),
        id,
        filtered_tracks.len(),
        count_prefix,
    ) {
        return Ok(true);
    }

    match command {
        Command::PlayRandom | Command::ChooseSelected => {
            let uri = if command == Command::PlayRandom {
                tracks[rand::rng().random_range(0..tracks.len())].id.uri()
            } else {
                filtered_tracks[id].id.uri()
            };

            // Update currently_playing_tracks_id based on the context
            match context_id {
                Some(ContextId::Tracks(ref tracks_id)) => {
                    state.player.write().currently_playing_tracks_id = Some(tracks_id.clone());
                }
                _ => {
                    state.player.write().currently_playing_tracks_id = None;
                }
            }

            let base_playback = match context_id {
                None | Some(ContextId::Tracks(_)) => {
                    Playback::URIs(tracks.iter().map(|t| t.id.clone().into()).collect(), None)
                }
                Some(ContextId::Show(_)) => unreachable!(
                    "show context should be handled by handle_command_for_episode_table_window"
                ),
                Some(context_id) => Playback::Context(context_id, None),
            };

            client_pub.send(ClientRequest::Player(PlayerRequest::StartPlayback(
                base_playback
                    .uri_offset(uri, config::get_config().app_config.tracks_playback_limit),
                None,
            )))?;
        }
        Command::ShowActionsOnSelectedItem => {
            let actions = command::construct_track_actions(filtered_tracks[id], data);
            view.popup = Some(PopupState::ActionList(
                Box::new(ActionListItem::Track(tracks[id].clone(), actions)),
                Cursor::default(),
            ));
        }
        Command::AddSelectedItemToQueue => {
            client_pub.send(ClientRequest::AddPlayableToQueue(
                filtered_tracks[id].id.clone().into(),
            ))?;
        }
        Command::JumpToHighlightTrackInContext => {
            view.popup = None;
            let selected_track = filtered_tracks[id];
            let location = tracks
                .iter()
                .enumerate()
                .find(|(_, track)| track.id == selected_track.id)
                .unwrap();

            // Move selection and change the offset so selection is at the top
            view.current_page_mut().select(location.0);
            match view.current_page_mut().focus_window_state_mut().unwrap() {
                MutableWindowState::Table(table) => *table.offset_mut() = location.0,
                _ => unreachable!("playlist context should be a table"),
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

pub fn handle_command_for_track_list_window(
    command: Command,
    client_pub: &flume::Sender<ClientRequest>,
    tracks: &[&Track],
    data: &DataReadGuard,
    view: &mut ViewStateGuard<'_>,
    state: &SharedState,
    count_prefix: Option<usize>,
) -> Result<bool> {
    let id = view.current_page_mut().selected().unwrap_or_default();
    if id >= tracks.len() {
        return Ok(false);
    }

    if handle_navigation_command(
        command,
        view.current_page_mut(),
        id,
        tracks.len(),
        count_prefix,
    ) {
        return Ok(true);
    }
    match command {
        Command::ChooseSelected => {
            // for a track list, `ChooseSelected` on a track
            // will start a `URIs` playback containing only that track.
            // This is different from the track table, which handles
            // `ChooseSelected` by starting a `URIs` playback
            // containing all the tracks in the table.

            // Track lists are used for search results, so clear the Tracks context
            state.player.write().currently_playing_tracks_id = None;

            client_pub.send(ClientRequest::Player(PlayerRequest::StartPlayback(
                Playback::URIs(vec![tracks[id].id.clone().into()], None),
                None,
            )))?;
        }
        Command::ShowActionsOnSelectedItem => {
            let actions = command::construct_track_actions(tracks[id], data);
            view.popup = Some(PopupState::ActionList(
                Box::new(ActionListItem::Track(tracks[id].clone(), actions)),
                Cursor::default(),
            ));
        }
        Command::AddSelectedItemToQueue => {
            client_pub.send(ClientRequest::AddPlayableToQueue(
                tracks[id].id.clone().into(),
            ))?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

pub fn handle_command_for_artist_list_window(
    command: Command,
    artists: &[&Artist],
    data: &DataReadGuard,
    view: &mut ViewStateGuard<'_>,
    count_prefix: Option<usize>,
) -> bool {
    let id = view.current_page_mut().selected().unwrap_or_default();
    if id >= artists.len() {
        return false;
    }

    if handle_navigation_command(
        command,
        view.current_page_mut(),
        id,
        artists.len(),
        count_prefix,
    ) {
        return true;
    }
    match command {
        Command::ChooseSelected => {
            let context_id = ContextId::Artist(artists[id].id.clone());
            view.new_page(PageState::Context {
                id: None,
                context_page_type: ContextPageType::Browsing(context_id),
                state: None,
            });
        }
        Command::ShowActionsOnSelectedItem => {
            let actions = construct_artist_actions(artists[id], data);
            view.popup = Some(PopupState::ActionList(
                Box::new(ActionListItem::Artist(artists[id].clone(), actions)),
                Cursor::default(),
            ));
        }
        _ => return false,
    }
    true
}

pub fn handle_command_for_album_list_window(
    command: Command,
    albums: &[&Album],
    data: &DataReadGuard,
    view: &mut ViewStateGuard<'_>,
    client_pub: &flume::Sender<ClientRequest>,
    count_prefix: Option<usize>,
) -> Result<bool> {
    let id = view.current_page_mut().selected().unwrap_or_default();
    if id >= albums.len() {
        return Ok(false);
    }

    if handle_navigation_command(
        command,
        view.current_page_mut(),
        id,
        albums.len(),
        count_prefix,
    ) {
        return Ok(true);
    }
    match command {
        Command::ChooseSelected => {
            let context_id = ContextId::Album(albums[id].id.clone());
            view.new_page(PageState::Context {
                id: None,
                context_page_type: ContextPageType::Browsing(context_id),
                state: None,
            });
        }
        Command::ShowActionsOnSelectedItem => {
            let actions = construct_album_actions(albums[id], data);
            view.popup = Some(PopupState::ActionList(
                Box::new(ActionListItem::Album(albums[id].clone(), actions)),
                Cursor::default(),
            ));
        }
        Command::AddSelectedItemToQueue => {
            client_pub.send(ClientRequest::AddAlbumToQueue(albums[id].id.clone()))?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

pub fn handle_command_for_playlist_list_window(
    command: Command,
    playlists: &[&PlaylistFolderItem],
    data: &DataReadGuard,
    view: &mut ViewStateGuard<'_>,
    count_prefix: Option<usize>,
) -> bool {
    let id = view.current_page_mut().selected().unwrap_or_default();
    if id >= playlists.len() {
        return false;
    }

    if handle_navigation_command(
        command,
        view.current_page_mut(),
        id,
        playlists.len(),
        count_prefix,
    ) {
        return true;
    }
    match command {
        Command::ChooseSelected => {
            let playlist = playlists[id];
            match playlist {
                PlaylistFolderItem::Folder(f) => {
                    // currently folders are only supported in the library page
                    match view.current_page_mut() {
                        PageState::Library { state } => {
                            state.playlist_list.select(Some(0));
                            state.focus = LibraryFocusState::Playlists;
                            state.playlist_folder_id = f.target_id;
                        }
                        _ => return false,
                    }
                }
                PlaylistFolderItem::Playlist(p) => {
                    let context_id = ContextId::Playlist(p.id.clone());
                    view.new_page(PageState::Context {
                        id: None,
                        context_page_type: ContextPageType::Browsing(context_id),
                        state: None,
                    });
                }
            }
        }
        Command::ShowActionsOnSelectedItem => {
            if let PlaylistFolderItem::Playlist(p) = playlists[id] {
                let actions = construct_playlist_actions(p, data);
                view.popup = Some(PopupState::ActionList(
                    Box::new(ActionListItem::Playlist(p.clone(), actions)),
                    Cursor::default(),
                ));
            }
        }
        _ => return false,
    }
    true
}

pub fn handle_command_for_show_list_window(
    command: Command,
    shows: &[&Show],
    data: &DataReadGuard,
    view: &mut ViewStateGuard<'_>,
    count_prefix: Option<usize>,
) -> bool {
    let id = view.current_page_mut().selected().unwrap_or_default();
    if id >= shows.len() {
        return false;
    }

    if handle_navigation_command(
        command,
        view.current_page_mut(),
        id,
        shows.len(),
        count_prefix,
    ) {
        return true;
    }
    match command {
        Command::ChooseSelected => {
            let context_id = ContextId::Show(shows[id].id.clone());
            view.new_page(PageState::Context {
                id: None,
                context_page_type: ContextPageType::Browsing(context_id),
                state: None,
            });
        }
        Command::ShowActionsOnSelectedItem => {
            let actions = construct_show_actions(shows[id], data);
            view.popup = Some(PopupState::ActionList(
                Box::new(ActionListItem::Show(shows[id].clone(), actions)),
                Cursor::default(),
            ));
        }
        _ => return false,
    }
    true
}

pub fn handle_command_for_episode_list_window(
    command: Command,
    client_pub: &flume::Sender<ClientRequest>,
    episodes: &[&Episode],
    data: &DataReadGuard,
    view: &mut ViewStateGuard<'_>,
    state: &SharedState,
    count_prefix: Option<usize>,
) -> Result<bool> {
    // Focus-independent: while the description panel is focused, unconsumed commands fall
    // through here and must act on the real cursor, not row 0.
    let id = view
        .current_page_mut()
        .selected_ignoring_focus()
        .unwrap_or_default();
    if id >= episodes.len() {
        return Ok(false);
    }

    if handle_navigation_command(
        command,
        view.current_page_mut(),
        id,
        episodes.len(),
        count_prefix,
    ) {
        return Ok(true);
    }
    match command {
        Command::ChooseSelected => {
            // Episodes don't have a Tracks context, so clear it
            state.player.write().currently_playing_tracks_id = None;

            client_pub.send(ClientRequest::Player(PlayerRequest::StartPlayback(
                Playback::URIs(vec![episodes[id].id.clone().into()], None),
                None,
            )))?;
        }
        Command::ShowActionsOnSelectedItem => {
            let actions = command::construct_episode_actions(episodes[id], data);
            view.popup = Some(PopupState::ActionList(
                Box::new(ActionListItem::Episode(episodes[id].clone(), actions)),
                Cursor::default(),
            ));
        }
        Command::AddSelectedItemToQueue => {
            client_pub.send(ClientRequest::AddPlayableToQueue(
                episodes[id].id.clone().into(),
            ))?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn handle_command_for_episode_table_window(
    command: Command,
    client_pub: &flume::Sender<ClientRequest>,
    show_id: &ShowId,
    episodes: &[&Episode],
    data: &DataReadGuard,
    view: &mut ViewStateGuard<'_>,
    state: &SharedState,
    count_prefix: Option<usize>,
) -> Result<bool> {
    // Read the selection from the episode table directly: when the detail window has focus,
    // `PageState::selected()` resolves to `None` (no focused window), but the episode selection
    // itself is unchanged.
    let id = match view.current_page() {
        PageState::Context {
            state: Some(ContextPageUIState::Show { episode_table, .. }),
            ..
        } => episode_table.selected().unwrap_or_default(),
        _ => return Ok(false),
    };
    if id >= episodes.len() {
        return Ok(false);
    }

    if handle_navigation_command(
        command,
        view.current_page_mut(),
        id,
        episodes.len(),
        count_prefix,
    ) {
        // reset the description scroll for the newly selected episode
        if let PageState::Context {
            state:
                Some(ContextPageUIState::Show {
                    description_scroll, ..
                }),
            ..
        } = view.current_page_mut()
        {
            *description_scroll = 0;
        }
        return Ok(true);
    }
    match command {
        Command::ChooseSelected => {
            let uri = episodes[id].id.uri();

            // Show context doesn't have a Tracks context, so clear it
            state.player.write().currently_playing_tracks_id = None;

            client_pub.send(ClientRequest::Player(PlayerRequest::StartPlayback(
                Playback::Context(
                    ContextId::Show(show_id.clone_static()),
                    Some(rspotify::model::Offset::Uri(uri)),
                ),
                None,
            )))?;
        }
        Command::ShowActionsOnSelectedItem => {
            let actions = command::construct_episode_actions(episodes[id], data);
            view.popup = Some(PopupState::ActionList(
                Box::new(ActionListItem::Episode(episodes[id].clone(), actions)),
                Cursor::default(),
            ));
        }
        Command::AddSelectedItemToQueue => {
            client_pub.send(ClientRequest::AddPlayableToQueue(
                episodes[id].id.clone().into(),
            ))?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// Handle the `n`-th action in an action list popup
pub fn handle_item_action(
    n: usize,
    client_pub: &flume::Sender<ClientRequest>,
    state: &SharedState,
    view: &mut ViewStateGuard<'_>,
) -> Result<bool> {
    let item = match view.popup {
        Some(PopupState::ActionList(ref item, ..)) => *item.clone(),
        _ => return Ok(false),
    };

    let data = state.data.read();

    match item {
        ActionListItem::Track(track, actions) => {
            handle_action_in_context(actions[n], track.into(), client_pub, &data, view)
        }
        ActionListItem::Album(album, actions) => {
            handle_action_in_context(actions[n], album.into(), client_pub, &data, view)
        }
        ActionListItem::Artist(artist, actions) => {
            handle_action_in_context(actions[n], artist.into(), client_pub, &data, view)
        }
        ActionListItem::Playlist(playlist, actions) => {
            handle_action_in_context(actions[n], playlist.into(), client_pub, &data, view)
        }
        ActionListItem::Show(show, actions) => {
            handle_action_in_context(actions[n], show.into(), client_pub, &data, view)
        }
        ActionListItem::Episode(episode, actions) => {
            handle_action_in_context(actions[n], episode.into(), client_pub, &data, view)
        }
    }
}
