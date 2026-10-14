use super::{
    controller, single_line_input, ActionTarget, ClientRequest, CommandOrAction, Ctx, Key, KeyCode,
    KeySequence, PageState, PageType, PlaylistFolderItem, Result, SearchFocusState, SharedState,
};

pub fn handle_key_sequence_for_page(
    key_sequence: &KeySequence,
    client_pub: &flume::Sender<ClientRequest>,
    state: &SharedState,
    ui: &mut Ctx<'_>,
) -> Result<bool> {
    let page_type = ui.current_page().page_type();
    // handle search page separately as it needs access to the raw key sequence
    // as opposed to the matched command
    if page_type == PageType::Search {
        return handle_key_sequence_for_search_page(key_sequence, client_pub, state, ui);
    }

    match crate::config::get_config()
        .keymap_config
        .find_command_or_action_from_key_sequence(key_sequence)
    {
        Some(CommandOrAction::Command(command)) => {
            handle_command_for_page(command, page_type, client_pub, state, ui)
        }
        Some(CommandOrAction::Action(action, ActionTarget::SelectedItem)) => {
            handle_action_for_page(action, page_type, client_pub, state, ui)
        }
        _ => Ok(false),
    }
}

/// Handles `command` for a page of type `page_type`.
///
/// Shared with frontends that run a command without a key press (Tern's `activate`).
pub fn handle_command_for_page(
    command: crate::command::Command,
    page_type: PageType,
    client_pub: &flume::Sender<ClientRequest>,
    state: &SharedState,
    ui: &mut Ctx<'_>,
) -> Result<bool> {
    let count = ui.tui.count_prefix;
    match page_type {
        PageType::Search => anyhow::bail!("page search type should already be handled!"),
        PageType::Library => controller::handle_command_for_library_page(
            command,
            client_pub,
            ui.view_mut(),
            state,
            count,
        ),
        PageType::Context => controller::handle_command_for_context_page(
            command,
            client_pub,
            ui.view_mut(),
            state,
            count,
        ),
        PageType::Browse => controller::handle_command_for_browse_page(
            command,
            client_pub,
            ui.view_mut(),
            state,
            count,
        ),
        // lyrics page doesn't support any commands
        PageType::Lyrics => Ok(false),
        PageType::Queue => Ok(controller::handle_command_for_queue_page(
            command,
            ui.view_mut(),
            count,
        )),
        PageType::CommandHelp => Ok(controller::handle_command_for_command_help_page(
            command,
            ui.view_mut(),
            count,
        )),
        PageType::Logs => Ok(controller::handle_command_for_logs_page(
            command,
            ui.view_mut(),
            count,
        )),
    }
}

/// Handles `action` on the selected item of a page of type `page_type`.
pub fn handle_action_for_page(
    action: crate::command::Action,
    page_type: PageType,
    client_pub: &flume::Sender<ClientRequest>,
    state: &SharedState,
    ui: &mut Ctx<'_>,
) -> Result<bool> {
    match page_type {
        PageType::Search => anyhow::bail!("page search type should already be handled!"),
        PageType::Library => {
            controller::handle_action_for_library_page(action, client_pub, ui.view_mut(), state)
        }
        PageType::Context => controller::handle_action_for_focused_context_page(
            action,
            client_pub,
            ui.view_mut(),
            state,
        ),
        PageType::Browse => {
            controller::handle_action_for_browse_page(action, client_pub, ui.view_mut(), state)
        }
        _ => Ok(false),
    }
}

fn handle_key_sequence_for_search_page(
    key_sequence: &KeySequence,
    client_pub: &flume::Sender<ClientRequest>,
    state: &SharedState,
    ui: &mut Ctx<'_>,
) -> Result<bool> {
    let count = ui.tui.count_prefix;
    let (focus_state, current_query, line_input) = match ui.current_page_mut() {
        PageState::Search {
            state,
            line_input,
            current_query,
        } => (state.focus, current_query, line_input),
        _ => anyhow::bail!("expect a search page"),
    };

    // handle user's input
    if let SearchFocusState::Input = focus_state {
        if key_sequence.keys.len() == 1 {
            return match &key_sequence.keys[0] {
                Key::None(KeyCode::Enter) => {
                    if !line_input.is_empty() {
                        *current_query = line_input.text();
                        client_pub.send(ClientRequest::Search(line_input.text()))?;
                    }
                    Ok(true)
                }
                k => match single_line_input::handle_key(line_input, k) {
                    None => Ok(false),
                    _ => Ok(true),
                },
            };
        }
    }

    let Some(found_keymap) = crate::config::get_config()
        .keymap_config
        .find_command_or_action_from_key_sequence(key_sequence)
    else {
        return Ok(false);
    };

    let data = state.data.read();
    let search_results = data.caches.search.get(current_query);

    match focus_state {
        SearchFocusState::Input => anyhow::bail!("user's search input should be handled before"),
        SearchFocusState::Tracks => {
            let tracks = search_results
                .map(|s| s.tracks.iter().collect::<Vec<_>>())
                .unwrap_or_default();

            match found_keymap {
                CommandOrAction::Command(command) => {
                    controller::handle_command_for_track_list_window(
                        command,
                        client_pub,
                        &tracks,
                        &data,
                        ui.view_mut(),
                        state,
                        count,
                    )
                }
                CommandOrAction::Action(action, ActionTarget::SelectedItem) => {
                    controller::handle_action_for_selected_item(
                        action,
                        &tracks,
                        &data,
                        ui.view_mut(),
                        client_pub,
                    )
                }
                CommandOrAction::Action(..) => Ok(false),
            }
        }
        SearchFocusState::Artists => {
            let artists = search_results
                .map(|s| s.artists.iter().collect::<Vec<_>>())
                .unwrap_or_default();

            match found_keymap {
                CommandOrAction::Command(command) => {
                    Ok(controller::handle_command_for_artist_list_window(
                        command,
                        &artists,
                        &data,
                        ui.view_mut(),
                        count,
                    ))
                }
                CommandOrAction::Action(action, ActionTarget::SelectedItem) => {
                    controller::handle_action_for_selected_item(
                        action,
                        &artists,
                        &data,
                        ui.view_mut(),
                        client_pub,
                    )
                }
                CommandOrAction::Action(..) => Ok(false),
            }
        }
        SearchFocusState::Albums => {
            let albums = search_results
                .map(|s| s.albums.iter().collect::<Vec<_>>())
                .unwrap_or_default();

            match found_keymap {
                CommandOrAction::Command(command) => {
                    controller::handle_command_for_album_list_window(
                        command,
                        &albums,
                        &data,
                        ui.view_mut(),
                        client_pub,
                        count,
                    )
                }
                CommandOrAction::Action(action, ActionTarget::SelectedItem) => {
                    controller::handle_action_for_selected_item(
                        action,
                        &albums,
                        &data,
                        ui.view_mut(),
                        client_pub,
                    )
                }
                CommandOrAction::Action(..) => Ok(false),
            }
        }
        SearchFocusState::Playlists => {
            let playlists = search_results
                .map(|s| {
                    s.playlists
                        .iter()
                        .map(|p| PlaylistFolderItem::Playlist(p.clone()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let playlist_refs = playlists.iter().collect::<Vec<_>>();

            match found_keymap {
                CommandOrAction::Command(command) => {
                    Ok(controller::handle_command_for_playlist_list_window(
                        command,
                        &playlist_refs,
                        &data,
                        ui.view_mut(),
                        count,
                    ))
                }
                CommandOrAction::Action(action, ActionTarget::SelectedItem) => {
                    controller::handle_action_for_selected_item(
                        action,
                        &playlist_refs,
                        &data,
                        ui.view_mut(),
                        client_pub,
                    )
                }
                CommandOrAction::Action(..) => Ok(false),
            }
        }
        SearchFocusState::Shows => {
            let shows = search_results
                .map(|s| s.shows.iter().collect::<Vec<_>>())
                .unwrap_or_default();

            match found_keymap {
                CommandOrAction::Command(command) => {
                    Ok(controller::handle_command_for_show_list_window(
                        command,
                        &shows,
                        &data,
                        ui.view_mut(),
                        count,
                    ))
                }
                CommandOrAction::Action(action, ActionTarget::SelectedItem) => {
                    controller::handle_action_for_selected_item(
                        action,
                        &shows,
                        &data,
                        ui.view_mut(),
                        client_pub,
                    )
                }
                CommandOrAction::Action(..) => Ok(false),
            }
        }
        SearchFocusState::Episodes => {
            let episodes = match search_results {
                Some(s) => s.episodes.iter().collect(),
                None => Vec::new(),
            };

            match found_keymap {
                CommandOrAction::Command(command) => {
                    controller::handle_command_for_episode_list_window(
                        command,
                        client_pub,
                        &episodes,
                        &data,
                        ui.view_mut(),
                        state,
                        count,
                    )
                }
                CommandOrAction::Action(action, ActionTarget::SelectedItem) => {
                    controller::handle_action_for_selected_item(
                        action,
                        &episodes,
                        &data,
                        ui.view_mut(),
                        client_pub,
                    )
                }
                CommandOrAction::Action(..) => Ok(false),
            }
        }
    }
}
