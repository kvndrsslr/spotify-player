//! The TSP frontend's tests: a fake Tern over channels, plus document and input fixtures.
//!
//! Everything here is offline and deterministic: the session talks to an in-memory
//! [`Terminal`](tern_sdk::term::Terminal) and the pages are built from hand-made view-model
//! fixtures, so no terminal, network or Spotify account is involved.

use std::sync::{Arc, Once};
use std::time::Duration;

use spotify_player_core::{
    client::ClientRequest,
    config,
    state::{
        ContextId, ContextPageType, LibraryPageUIState, PageState, Playlist, PlaylistFolderItem,
        PlaylistId, PopupState, SearchPageUIState, SharedState, Show, ShowId, Track, TrackId,
        UserId,
    },
};
use tern_sdk::{term::Terminal, ui, wire::Op, Options, Session, Surface, SurfaceOptions};

use super::view::FrontendState;
use super::{blobs, keys, view, Msg, SessionState};
use crate::ui::UIState;

/// The `hello` reply and the DA1 answer a Tern pane sends back, in the order the handshake needs
/// them: the reply first, so TSP is detected.
fn hello_reply() -> Vec<u8> {
    let mut bytes = b"\x1b_tsp;r;{\"r\":\"hello\",\"v\":1,\"term\":\"tern\",\"ver\":\"test\",\"kinds\":[\"col\",\"row\",\"card\",\"text\",\"list\",\"item\",\"input\",\"section\",\"spacer\",\"rule\",\"spinner\",\"status\",\"seg\",\"progress\",\"image\",\"overlay\",\"table\",\"el\"],\"features\":[\"blobs\",\"styles\",\"flow\",\"settle\",\"program-palette\"],\"apc\":65536,\"credits\":2,\"cols\":100,\"dark\":true}\x1b\\".to_vec();
    bytes.extend_from_slice(b"\x1b[?62;52;c");
    bytes
}

/// A Tern peer: what the session writes is queued for the test, and what the test writes is read.
struct FakeTerminal {
    out: flume::Sender<Vec<u8>>,
    inp: flume::Receiver<Vec<u8>>,
}

impl Terminal for FakeTerminal {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.out
            .send(bytes.to_vec())
            .map_err(|err| std::io::Error::other(err.to_string()))
    }

    fn read(
        &mut self,
        buf: &mut [u8],
        timeout: Option<Duration>,
    ) -> std::io::Result<Option<usize>> {
        match self
            .inp
            .recv_timeout(timeout.unwrap_or(Duration::from_secs(5)))
        {
            Ok(bytes) => {
                let n = bytes.len().min(buf.len());
                buf[..n].copy_from_slice(&bytes[..n]);
                Ok(Some(n))
            }
            Err(flume::RecvTimeoutError::Timeout) => Ok(None),
            // the test side dropped its sender: the "terminal" closed
            Err(flume::RecvTimeoutError::Disconnected) => Ok(Some(0)),
        }
    }
}

/// A session talking to a [`FakeTerminal`], with the test's end of both channels: what the session
/// wrote, and the sender that types at it.
fn session() -> (
    Session<Msg>,
    flume::Receiver<Vec<u8>>,
    flume::Sender<Vec<u8>>,
) {
    let (out_tx, out_rx) = flume::unbounded();
    let (inp_tx, inp_rx) = flume::unbounded();

    inp_tx.send(hello_reply()).expect("queue the hello reply");

    let term = FakeTerminal {
        out: out_tx,
        inp: inp_rx,
    };
    let session = Session::with_terminal(term, Options::new().app("test"))
        .expect("the handshake should not fail")
        .expect("the fake terminal speaks TSP");

    (session, out_rx, inp_tx)
}

/// Everything the session wrote, as raw bytes plus the `(verb, body)` of every TSP message.
fn written(out: &flume::Receiver<Vec<u8>>) -> (Vec<u8>, Vec<(String, String)>) {
    let mut all = Vec::new();
    while let Ok(bytes) = out.try_recv() {
        all.extend_from_slice(&bytes);
    }

    let mut messages = Vec::new();
    let mut rest = all.as_slice();
    while let Some(start) = find(rest, b"\x1b_tsp;") {
        let Some(end) = find(&rest[start..], b"\x1b\\") else {
            break;
        };
        let message = String::from_utf8_lossy(&rest[start + 6..start + end]).into_owned();
        let (verb, body) = message.split_once(';').unwrap_or((&message, ""));
        // skip the parameter segments (`c=…`, `id=…`) up to the JSON body
        let body = match body.find(['{', '[']) {
            Some(at) => &body[at..],
            None => body,
        };
        messages.push((verb.to_owned(), body.to_owned()));
        rest = &rest[start + end + 2..];
    }

    (all, messages)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Initializes the process-global configuration once, for the fixtures that need it.
fn init_config() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir =
            std::env::temp_dir().join(format!("spotify-player-tsp-tests-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the test config folder");

        let configs = config::Configs::new(&dir, &dir).expect("load the test configuration");
        config::set_config(configs);
        crate::frontend_config::set(
            crate::frontend_config::FrontendConfigs::new(&dir).expect("load the test theme"),
        );
    });
}

/// An application state with a little of everything the pages render.
fn fixture() -> SharedState {
    init_config();

    let state = Arc::new(spotify_player_core::state::State::new(
        false,
        Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new())),
        spotify_player_core::state::RedrawNotify::new(),
    ));

    {
        let mut data = state.data.write();
        data.user_data.playlists = vec![
            PlaylistFolderItem::Playlist(playlist("37i9dQZF1DXcBWIGoYBM5M", "Daily Mix")),
            PlaylistFolderItem::Playlist(playlist("37i9dQZF1DX0XUsuxWVO1uo", "Discover Weekly")),
        ];
        let track = Track {
            id: TrackId::from_id("1eX57c7iU9Q2b0t6s0kJ2vW").expect("valid track id"),
            name: String::from("The Title"),
            artists: Vec::new(),
            album: None,
            duration: Duration::from_secs(200),
            explicit: false,
            added_at: 0,
        };
        data.caches.context.insert(
            String::from("spotify:user:me:collection"),
            spotify_player_core::state::Context::Tracks {
                tracks: vec![track],
                desc: String::from("Saved tracks"),
            },
            *spotify_player_core::state::TTL_CACHE_DURATION,
        );
        data.caches.lyrics.insert(
            String::from("spotify:track:x"),
            Some(spotify_player_core::state::Lyrics {
                lines: vec![
                    (chrono::Duration::zero(), String::from("first line")),
                    (
                        chrono::Duration::milliseconds(5_000),
                        String::from("second line"),
                    ),
                ],
            }),
            *spotify_player_core::state::TTL_CACHE_DURATION,
        );
        data.user_data.saved_shows = vec![Show {
            id: ShowId::from_id("4aawyAB9vmqN3uQ7FjRGTy").expect("valid show id"),
            name: "The Show".to_owned(),
            publisher: "The Publisher".to_owned(),
            #[cfg(feature = "image")]
            image_url: None,
        }];
    }

    state
}

fn playlist(id: &'static str, name: &str) -> Playlist {
    Playlist {
        id: PlaylistId::from_id(id).expect("valid playlist id"),
        collaborative: false,
        name: name.to_owned(),
        owner: (
            String::from("me"),
            UserId::from_id("spotify").expect("valid user id"),
        ),
        desc: String::new(),
        current_folder_id: 0,
        snapshot_id: String::from("snapshot"),
    }
}

/// Runs `$body` with the state's locks held in a [`view::Model`] named `$model`.
macro_rules! with_model {
    ($state:expr, $model:ident, $body:block) => {{
        let view = $state.view.lock();
        let data = $state.data.read();
        let player = $state.player.read();
        let $model = view::Model {
            view: &view,
            data: &data,
            player: &player,
            logs: &$state.logs,
        };
        $body
    }};
}

/// Every node id of a document, and every id it asks to have selected.
fn ids_and_selections(view: ui::View<Msg>) -> (Vec<String>, Vec<String>) {
    fn walk(tree: &tern_sdk::reconcile::Tree, ids: &mut Vec<String>, selected: &mut Vec<String>) {
        ids.push(tree.id().to_owned());
        if let Some(value) = tree
            .props()
            .get("selected")
            .and_then(|value| value.as_str())
        {
            selected.push(value.to_owned());
        }
        for child in tree.children() {
            walk(child, ids, selected);
        }
    }

    let doc = tern_sdk::reconcile::Doc::from_view(view).expect("a valid document");
    let mut ids = Vec::new();
    let mut selected = Vec::new();
    for region in doc.regions().iter().flatten() {
        walk(region, &mut ids, &mut selected);
    }

    (ids, selected)
}

/// The key of the page node a [`PageState`] draws.
fn page_node_key(page: &PageState) -> &'static str {
    match page {
        PageState::Library { .. } => "page.library",
        PageState::Search { .. } => "page.search",
        PageState::Context { .. } => "page.context",
        PageState::Browse { .. } => "page.browse",
        PageState::Lyrics { .. } => "page.lyrics",
        PageState::Queue { .. } => "page.queue",
        PageState::CommandHelp { .. } => "page.help",
        PageState::Logs { .. } => "page.logs",
    }
}

#[test]
fn the_handshake_opens_a_screen_surface_and_sends_one_frame() {
    let (mut session, out, _inp) = session();
    let surface = session
        .open(
            SurfaceOptions::screen()
                .id("sp")
                .title("spotify-player")
                .role("spotify-player.session"),
        )
        .expect("open the surface");

    session
        .render(surface, ui::View::new().main(ui::text("hello")))
        .expect("render");

    let (_all, messages) = written(&out);
    let verbs = messages
        .iter()
        .map(|(verb, _)| verb.clone())
        .collect::<Vec<_>>();

    assert_eq!(
        verbs,
        ["q", "o", "f"],
        "the hello query, the open, then one frame"
    );
    assert!(messages[1].1.contains("\"mode\":\"screen\""));
    assert!(messages[2].1.contains("\"s\":1"));
    assert!(messages[2].1.contains("\"add\""));
}

/// The document for the state's current page and popup: node ids, selection ids and ops.
fn document(state: &SharedState) -> (Vec<String>, Vec<String>, Vec<Op>) {
    with_model!(state, model, {
        let (view, ops) = view::view_for_test(&model, &mut FrontendState::default());
        let (ids, selected) = ids_and_selections(view);
        (ids, selected, ops)
    })
}

/// Every id the document refers to by name must name a node in it.
fn assert_ids_resolve(label: &str, ids: &[String], selected: &[String], ops: &[Op]) {
    for id in selected {
        assert!(ids.contains(id), "{label}: selection {id} is not a node");
    }

    for op in ops {
        let Some(id) = (match op {
            Op::Focus { id: Some(id) } | Op::Reveal { id, .. } | Op::Scroll { id, .. } => Some(id),
            _ => None,
        }) else {
            continue;
        };
        assert!(ids.contains(id), "{label}: {op:?} targets {id}, not a node");
    }
}

#[test]
fn every_page_builds_a_document_whose_ids_resolve() {
    let state = fixture();
    let context_id = ContextId::Tracks(spotify_player_core::state::TracksId::new(
        "spotify:user:me:collection",
        "tracks",
    ));

    let cases = vec![
        PageState::Library {
            state: LibraryPageUIState::new(),
        },
        PageState::Search {
            line_input: spotify_player_core::state::TextInput::default(),
            current_query: String::from("query"),
            state: SearchPageUIState::new(),
        },
        PageState::Context {
            id: Some(context_id.clone()),
            context_page_type: ContextPageType::Browsing(context_id),
            state: Some(spotify_player_core::state::ContextPageUIState::Tracks {
                track_table: spotify_player_core::state::Cursor::default(),
            }),
        },
        PageState::Browse {
            state: spotify_player_core::state::BrowsePageUIState::CategoryList {
                state: spotify_player_core::state::Cursor::default(),
            },
        },
        PageState::Lyrics {
            track_uri: String::from("spotify:track:x"),
            track: String::from("Track"),
            artists: String::from("Artist"),
        },
        PageState::Queue { scroll_offset: 0 },
        PageState::CommandHelp { scroll_offset: 0 },
        PageState::Logs { scroll_offset: 0 },
    ];

    for page in cases {
        let label = page_node_key(&page);
        let expected = format!("main.{label}");
        let expected_ids = match &page {
            PageState::Library { .. } => vec![
                format!("main.{label}.panes.playlists.list"),
                format!("main.{label}.panes.albums.list"),
                format!("main.{label}.panes.artists.list"),
                format!("main.{label}.panes.shows.list"),
            ],
            PageState::Search { .. } => vec![
                format!("main.{label}.query"),
                format!("main.{label}.results.playlists.list"),
            ],
            PageState::Context { .. } => vec![
                format!("main.{label}.description"),
                format!("main.{label}.body.list"),
            ],
            PageState::Browse { .. } => vec![format!("main.{label}.categories.list")],
            // without playback the page shows its message; the loaded body needs a whole
            // `CurrentPlaybackContext` fixture, so its ids are only checked by construction
            PageState::Lyrics { .. } => vec![format!("main.{label}.message")],
            // the fixture has no queue, so the page shows its loading spinner: the table
            // under `main.page.queue.rows` is built the same way when a queue exists
            PageState::Queue { .. } => vec![format!("main.{label}.loading")],
            PageState::CommandHelp { .. } => vec![format!("main.{label}.commands")],
            PageState::Logs { .. } => vec![format!("main.{label}.body")],
        };

        state.view.lock().history.push(page);
        let (ids, selected, ops) = document(&state);

        assert!(ids.contains(&expected), "{label}: no {expected} in {ids:?}");
        for id in expected_ids {
            assert!(ids.contains(&id), "{label}: no {id} in {ids:?}");
        }
        assert_ids_resolve(label, &ids, &selected, &ops);
    }
}

#[test]
fn a_context_whose_kind_does_not_match_its_page_state_says_so() {
    let state = fixture();
    let context_id = ContextId::Tracks(spotify_player_core::state::TracksId::new(
        "spotify:user:me:collection",
        "tracks",
    ));

    // the cache holds a track list, but the page state believes this is an artist page
    state.view.lock().history.push(PageState::Context {
        id: Some(context_id.clone()),
        context_page_type: ContextPageType::Browsing(context_id),
        state: Some(spotify_player_core::state::ContextPageUIState::Artist {
            top_track_table: spotify_player_core::state::Cursor::default(),
            album_table: spotify_player_core::state::Cursor::default(),
            related_artist_list: spotify_player_core::state::Cursor::default(),
            liked_track_table: spotify_player_core::state::Cursor::default(),
            focus: spotify_player_core::state::ArtistFocusState::TopTracks,
        }),
    });

    let (ids, selected, ops) = document(&state);
    assert!(
        ids.contains(&String::from("main.page.context.body.message")),
        "a mismatched context should say so; got {ids:?}"
    );
    assert_ids_resolve("mismatched context", &ids, &selected, &ops);

    state.view.lock().history.pop();
}

#[test]
fn every_popup_builds_a_layer_document_whose_ids_resolve() {
    let state = fixture();
    let popups = vec![
        (
            PopupState::Search {
                query: String::from("find"),
            },
            "layer.popup.body.field",
        ),
        (
            PopupState::PlaylistCreate {
                name: spotify_player_core::state::TextInput::default(),
                desc: spotify_player_core::state::TextInput::default(),
                current_field: spotify_player_core::state::PlaylistCreateCurrentField::Name,
            },
            "layer.popup.body.name",
        ),
        (
            PopupState::DeviceList(spotify_player_core::state::Cursor::default()),
            "layer.popup",
        ),
        (
            PopupState::ThemeList(Vec::new(), spotify_player_core::state::Cursor::default()),
            "layer.popup",
        ),
        (
            PopupState::UserPlaylistList(
                spotify_player_core::state::PlaylistPopupAction::Browse {
                    folder_id: 0,
                    search_query: String::new(),
                },
                spotify_player_core::state::Cursor::default(),
            ),
            "layer.popup",
        ),
        (
            PopupState::ConfirmAction {
                message: String::from("Delete this?"),
                action: spotify_player_core::state::ConfirmableAction::DeleteFromLibrary(
                    spotify_player_core::state::ItemId::Track(
                        TrackId::from_id("1eX57c7iU9Q2b0t6s0kJ2vW").expect("valid track id"),
                    ),
                ),
            },
            "layer.popup",
        ),
    ];

    for (popup, expected) in popups {
        let label = format!("{popup:?}");
        state.view.lock().popup = Some(popup);
        let (ids, selected, ops) = document(&state);

        assert!(
            ids.contains(&String::from(expected)),
            "{label}: no {expected} in {ids:?}"
        );
        assert_ids_resolve(&label, &ids, &selected, &ops);

        state.view.lock().popup = None;
    }
}

#[test]
fn a_reordered_list_moves_instead_of_readding() {
    fn list(order: [&str; 2]) -> ui::View<()> {
        let mut node = ui::list()
            .key("list")
            .selected(format!("main.list.{}", order[0]));

        for key in order {
            node = node.child(ui::item(key).key(key));
        }

        ui::View::new().main(node)
    }

    let before = tern_sdk::reconcile::Doc::from_view(list(["a", "b"])).expect("a valid document");
    let after = tern_sdk::reconcile::Doc::from_view(list(["b", "a"])).expect("a valid document");

    let ops = before.ops(&after, "sp");

    assert!(
        ops.iter().any(|op| matches!(op, Op::Move { .. })),
        "a reorder should move: {ops:?}"
    );
    assert!(
        !ops.iter().any(|op| matches!(op, Op::Del { .. })),
        "a reorder should not delete: {ops:?}"
    );
}

#[test]
fn a_key_reaches_the_command_executor() {
    let state = fixture();
    let (client_tx, client_rx) = flume::bounded::<ClientRequest>(1);
    let mut ui_state = UIState::new();

    // "j" moves the library's selected playlist down
    let mut view = state.view.lock();
    view.current_page_mut().select(0);
    drop(view);

    let key = tern_sdk::keys::Key {
        text: Some(String::from("j")),
        ..tern_sdk::keys::Key::named("j")
    };
    keys::handle(&key, &state, &mut ui_state, &client_tx).expect("handle the key");

    let selected = {
        let mut view = state.view.lock();
        view.current_page_mut().selected()
    };
    assert_eq!(selected, Some(1), "the library cursor should follow `j`");
    drop(client_rx);
}

/// A Tern event with no fields: enough for the handlers that only log it.
fn an_event() -> tern_sdk::wire::Event {
    tern_sdk::wire::Event::Unknown(serde_json::Map::new())
}

#[test]
fn a_tern_select_moves_the_cursor_where_the_tui_keeps_it() {
    let state = fixture();
    let (client_tx, _client_rx) = flume::unbounded::<ClientRequest>();
    let mut ui_state = UIState::new();

    super::handle_msg(
        Msg::Select {
            target: view::Target::Page(view::PageList::Playlists),
            index: 1,
        },
        &state,
        &mut ui_state,
        &client_tx,
        &an_event(),
    )
    .expect("handle the select");

    let page = state.view.lock().current_page().clone();
    match page {
        PageState::Library { state } => {
            assert_eq!(state.playlist_list.selected(), Some(1));
            assert_eq!(
                state.focus,
                spotify_player_core::state::LibraryFocusState::Playlists
            );
        }
        other => panic!("expected the library page, got {other:?}"),
    }
}

#[test]
fn a_tern_action_runs_the_tui_command() {
    let state = fixture();
    let (client_tx, client_rx) = flume::unbounded::<ClientRequest>();
    let mut ui_state = UIState::new();

    super::handle_msg(
        Msg::Act(String::from("play-pause")),
        &state,
        &mut ui_state,
        &client_tx,
        &an_event(),
    )
    .expect("handle the action");

    assert!(
        client_rx.try_recv().is_ok(),
        "play-pause should ask the client for something"
    );
}

#[test]
fn a_tern_activate_submits_the_playlist_create_popup() {
    let state = fixture();
    let (client_tx, client_rx) = flume::unbounded::<ClientRequest>();
    let mut ui_state = UIState::new();

    let mut name = spotify_player_core::state::TextInput::default();
    for c in "Night Drive".chars() {
        name.insert_char(c);
    }
    state.view.lock().popup = Some(PopupState::PlaylistCreate {
        name,
        desc: spotify_player_core::state::TextInput::default(),
        current_field: spotify_player_core::state::PlaylistCreateCurrentField::Name,
    });

    super::handle_msg(
        Msg::Activate {
            target: view::Target::Popup,
            index: 0,
        },
        &state,
        &mut ui_state,
        &client_tx,
        &an_event(),
    )
    .expect("handle the activate");

    assert!(
        matches!(
            client_rx.try_recv(),
            Ok(ClientRequest::CreatePlaylist { playlist_name, .. }) if playlist_name == "Night Drive"
        ),
        "the popup should ask for the playlist"
    );
    assert!(state.view.lock().popup.is_none(), "the popup should close");
}

#[test]
fn a_tern_activate_confirms_the_confirm_popup() {
    let state = fixture();
    let (client_tx, client_rx) = flume::unbounded::<ClientRequest>();
    let mut ui_state = UIState::new();

    state.view.lock().popup = Some(PopupState::ConfirmAction {
        message: String::from("Remove?"),
        action: spotify_player_core::state::ConfirmableAction::DeleteFromLibrary(
            spotify_player_core::state::ItemId::Track(
                TrackId::from_id("1eX57c7iU9Q2b0t6s0kJ2vW").expect("valid track id"),
            ),
        ),
    });

    super::handle_msg(
        Msg::Activate {
            target: view::Target::Popup,
            index: 0,
        },
        &state,
        &mut ui_state,
        &client_tx,
        &an_event(),
    )
    .expect("handle the activate");

    assert!(
        matches!(
            client_rx.try_recv(),
            Ok(ClientRequest::DeleteFromLibrary(_))
        ),
        "the confirmation should ask for the deletion"
    );
    assert!(state.view.lock().popup.is_none(), "the popup should close");
}

#[test]
fn a_tern_change_mirrors_into_the_view_model() {
    let state = fixture();
    let (client_tx, _client_rx) = flume::unbounded::<ClientRequest>();
    let mut ui_state = UIState::new();

    state.view.lock().popup = Some(PopupState::PlaylistCreate {
        name: spotify_player_core::state::TextInput::default(),
        desc: spotify_player_core::state::TextInput::default(),
        current_field: spotify_player_core::state::PlaylistCreateCurrentField::Name,
    });

    super::handle_msg(
        Msg::Choice {
            name: String::from("field"),
            value: String::from("desc"),
        },
        &state,
        &mut ui_state,
        &client_tx,
        &an_event(),
    )
    .expect("handle the change");

    assert!(
        matches!(
            state.view.lock().popup,
            Some(PopupState::PlaylistCreate {
                current_field: spotify_player_core::state::PlaylistCreateCurrentField::Desc,
                ..
            })
        ),
        "the radio group should move the field"
    );
}

#[test]
fn quitting_closes_the_surface_and_resets_the_input_modes() {
    let (mut session, out, _inp) = session();
    let surface = session
        .open(SurfaceOptions::screen().id("sp"))
        .expect("open the surface");

    session
        .render(surface, ui::View::new().main(ui::text("bye")))
        .expect("render");
    session
        .close_surface(surface, false)
        .expect("close the surface");
    session.close().expect("close the session");

    let (all, messages) = written(&out);
    let verbs = messages
        .iter()
        .map(|(verb, _)| verb.clone())
        .collect::<Vec<_>>();

    assert!(verbs.contains(&String::from("x")), "the close: {verbs:?}");
    assert!(
        find(&all, b"\x1b[<u").is_some(),
        "the kitty keyboard flag should be reset"
    );
    assert!(
        find(&all, b"\x1b[?2004l").is_some(),
        "bracketed paste should be disabled again"
    );
}

// ---------------------------------------------------------------------------------------------
// The frame-level harness: the real session loop over the fake terminal.
//
// Everything below drives `super::turn` — the body of the frontend's session loop — so what it
// asserts is what Tern is told to draw, not just what the view model holds.
// ---------------------------------------------------------------------------------------------

/// The surface the frontend opens for its session.
fn open_surface(session: &mut Session<Msg>) -> Surface {
    session
        .open(
            SurfaceOptions::screen()
                .id("sp")
                .title("spotify-player")
                .role("spotify-player.session"),
        )
        .expect("open the surface")
}

/// The state a session keeps between turns.
fn turn_state() -> SessionState {
    SessionState {
        ui_state: UIState::new(),
        blobs: blobs::Blobs::default(),
        frontend: FrontendState::default(),
    }
}

/// One turn of the real session loop. Panics when it fails, so a test reads as the loop does.
fn step(
    state: &SharedState,
    session: &mut Session<Msg>,
    surface: Surface,
    session_state: &mut SessionState,
    client_tx: &flume::Sender<ClientRequest>,
) -> bool {
    super::turn(session, surface, session_state, state, client_tx).expect("a session loop turn")
}

/// The frames in a set of written messages, as JSON.
fn frames(messages: &[(String, String)]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .filter(|(verb, _)| verb == "f")
        .map(|(_, body)| serde_json::from_str(body).expect("a frame body"))
        .collect()
}

/// The `ops` of a frame.
fn ops(frame: &serde_json::Value) -> Vec<serde_json::Value> {
    frame["ops"].as_array().expect("a frame's ops").clone()
}

/// A Tern event, as the terminal sends it.
fn event(body: &str) -> Vec<u8> {
    format!("\x1b_tsp;e;{body}\x1b\\").into_bytes()
}

/// A Tern `ack` for frame `s`.
fn ack(s: u64) -> Vec<u8> {
    event(&format!("{{\"ev\":\"ack\",\"sf\":\"sp\",\"s\":{s}}}"))
}

#[test]
fn a_key_changes_the_rendered_frame() {
    let state = fixture();
    let (client_tx, _client_rx) = flume::unbounded::<ClientRequest>();
    let (mut session, out, inp) = session();
    let surface = open_surface(&mut session);
    let mut session_state = turn_state();

    assert!(step(
        &state,
        &mut session,
        surface,
        &mut session_state,
        &client_tx
    ));
    let first = frames(&written(&out).1);
    assert_eq!(first.len(), 1, "the first turn draws the library page");

    // `j` moves the library's selection one row down. A turn draws before it reads, so the turn
    // that takes the key draws nothing and the one after it draws the moved selection.
    inp.send(b"j".to_vec()).expect("type a key");
    for _ in 0..2 {
        assert!(step(
            &state,
            &mut session,
            surface,
            &mut session_state,
            &client_tx
        ));
    }
    let second = frames(&written(&out).1);

    assert_eq!(second.len(), 1, "the key draws one frame: {second:?}");
    assert_eq!(second[0]["s"], 2, "the frame is the second one: {second:?}");
    assert_ne!(
        second[0], first[0],
        "a keypress must change what Tern is told to draw"
    );
    assert!(
        ops(&second[0]).iter().any(|op| op[0] == "set"
            && op[1] == "main.page.library.panes.playlists.list"
            && op[2].get("selected").is_some()),
        "the frame moves the list's selection: {second:?}"
    );
}

#[test]
fn a_tern_select_changes_the_rendered_frame() {
    let state = fixture();
    let (client_tx, _client_rx) = flume::unbounded::<ClientRequest>();
    let (mut session, out, inp) = session();
    let surface = open_surface(&mut session);
    let mut session_state = turn_state();

    assert!(step(
        &state,
        &mut session,
        surface,
        &mut session_state,
        &client_tx
    ));
    let _ = written(&out);

    // Tern reports the click on the second playlist row; the next turn has to draw the selection
    let clicked =
        "main.page.library.panes.playlists.list.i:p:spotify:playlist:37i9dQZF1DX0XUsuxWVO1uo";
    inp.send(event(&format!(
        "{{\"ev\":\"select\",\"sf\":\"sp\",\"id\":\"main.page.library.panes.playlists.list\",\"item\":\"{clicked}\"}}"
    )))
    .expect("click a row");
    // the turn that takes the event draws the old document; the one after it draws the selection
    for _ in 0..2 {
        assert!(step(
            &state,
            &mut session,
            surface,
            &mut session_state,
            &client_tx
        ));
    }
    let frame = frames(&written(&out).1);

    assert_eq!(frame.len(), 1, "the select draws one frame: {frame:?}");
    assert!(
        frame[0].to_string().contains(clicked),
        "the frame draws the clicked row as selected: {frame:?}"
    );
}

#[test]
fn frames_keep_flowing_while_tern_acks() {
    let state = fixture();
    let (client_tx, _client_rx) = flume::unbounded::<ClientRequest>();
    let (mut session, out, inp) = session();
    let surface = open_surface(&mut session);
    let mut session_state = turn_state();

    // A change draws on the turn *after* the one that takes the key, so a change is two turns.
    // `tab` moves the library's focus, which changes the document whatever the selection is. The
    // hello reply's `credits` is 2: two frames may be unacknowledged.
    let draw = |inp: &flume::Sender<Vec<u8>>,
                session: &mut Session<Msg>,
                session_state: &mut SessionState| {
        inp.send(b"\t".to_vec()).expect("type a key");
        for _ in 0..2 {
            assert!(step(&state, session, surface, session_state, &client_tx));
        }
        frames(&written(&out).1)
    };

    // the page itself, which uses the first of the two credits
    assert!(step(
        &state,
        &mut session,
        surface,
        &mut session_state,
        &client_tx
    ));
    assert_eq!(frames(&written(&out).1).len(), 1, "the first frame");

    assert_eq!(
        draw(&inp, &mut session, &mut session_state).len(),
        1,
        "the second frame uses the second credit"
    );
    assert!(
        draw(&inp, &mut session, &mut session_state).is_empty(),
        "a third frame waits for an ack"
    );

    // Tern drew the first frame: the next turn sends the change that was waiting
    inp.send(ack(1)).expect("ack the first frame");
    assert!(step(
        &state,
        &mut session,
        surface,
        &mut session_state,
        &client_tx
    ));
    let after = frames(&written(&out).1);
    assert_eq!(
        after.len(),
        1,
        "an ack releases the waiting frame: {after:?}"
    );
    assert_eq!(
        after[0]["s"], 3,
        "the released frame is the next sequence: {after:?}"
    );
}

/// The url of the cover the fixtures' playing item and episode carry.
#[cfg(feature = "image")]
const COVER_URL: &str = "https://i.scdn.co/image/cover-url";

/// The cover image itself, cached, so core can encode it into a blob.
#[cfg(feature = "image")]
fn cache_cover_image(state: &SharedState) {
    let image =
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2, 2, image::Rgb([1, 2, 3])));
    state.data.write().caches.images.insert(
        COVER_URL.to_owned(),
        image,
        *spotify_player_core::state::TTL_CACHE_DURATION,
    );
}

/// Puts a playing track whose album has cover art in the fixture: what the dock draws.
#[cfg(feature = "image")]
fn play_track_with_cover(state: &SharedState) {
    use rspotify::model::{AlbumId, Device, FullTrack, Image, PlayableItem, SimplifiedAlbum, Type};

    let track = FullTrack {
        album: SimplifiedAlbum {
            album_group: None,
            album_type: Some("album".to_owned()),
            artists: Vec::new(),
            available_markets: Vec::new(),
            external_urls: std::collections::HashMap::new(),
            href: None,
            id: Some(AlbumId::from_id("4aawyAB9vmqN3uQ7FjRGTy").expect("a valid album id")),
            images: vec![Image {
                url: COVER_URL.to_owned(),
                height: None,
                width: None,
            }],
            name: "The Album".to_owned(),
            release_date: None,
            release_date_precision: None,
            restrictions: None,
        },
        artists: vec![rspotify::model::SimplifiedArtist {
            external_urls: std::collections::HashMap::new(),
            href: None,
            id: None,
            name: "The Artist".to_owned(),
        }],
        available_markets: Vec::new(),
        disc_number: 1,
        duration: chrono::Duration::seconds(200),
        explicit: false,
        external_ids: std::collections::HashMap::new(),
        external_urls: std::collections::HashMap::new(),
        href: None,
        id: Some(TrackId::from_id("1eX57c7iU9Q2b0t6s0kJ2vW").expect("a valid track id")),
        is_local: false,
        is_playable: None,
        linked_from: None,
        name: "The Title".to_owned(),
        popularity: 0,
        preview_url: None,
        restrictions: None,
        track_number: 1,
        r#type: Type::Track,
    };

    let mut player = state.player.write();
    player.playback = Some(rspotify::model::CurrentPlaybackContext {
        device: Device {
            id: Some("device".to_owned()),
            is_active: true,
            is_private_session: false,
            is_restricted: false,
            name: "The Device".to_owned(),
            _type: rspotify::model::DeviceType::Computer,
            volume_percent: None,
        },
        repeat_state: rspotify::model::RepeatState::Off,
        shuffle_state: false,
        context: None,
        timestamp: chrono::Utc::now(),
        progress: Some(chrono::Duration::zero()),
        is_playing: false,
        item: Some(PlayableItem::Track(track)),
        currently_playing_type: rspotify::model::CurrentlyPlayingType::Track,
        actions: rspotify::model::Actions {
            disallows: Vec::new(),
        },
    });
    player.buffered_playback = Some(spotify_player_core::state::PlaybackMetadata {
        device_name: "The Device".to_owned(),
        device_id: None,
        volume: None,
        is_playing: false,
        repeat_state: rspotify::model::RepeatState::Off,
        shuffle_state: false,
        mute_state: None,
    });
}

/// Whether the document draws a cover blob named `blob`.
#[cfg(feature = "image")]
fn draws_cover(view: tern_sdk::View<Msg>, blob: &str) -> bool {
    fn walk(tree: &tern_sdk::reconcile::Tree, blob: &str) -> bool {
        if tree.props().get("blob").and_then(|value| value.as_str()) == Some(blob) {
            return true;
        }
        tree.children().iter().any(|child| walk(child, blob))
    }

    let doc = tern_sdk::reconcile::Doc::from_view(view).expect("a valid document");
    doc.regions()
        .iter()
        .flatten()
        .any(|region| walk(region, blob))
}

/// The cover art of a build must not be encoded while the build holds a guard on the data cache:
/// `State::image_blob` reads the image through the guard the *caller* holds, and an encode under
/// one's own guard used to be a write lock that could never be taken.
///
/// A regression shows up as the build never returning, so the build runs on its own thread and the
/// test fails on the timeout instead of hanging the suite.
#[cfg(feature = "image")]
fn a_cover_is_encoded_off_the_build_thread(set_up: fn(&SharedState)) {
    let state = fixture();
    cache_cover_image(&state);
    set_up(&state);

    let (mut session, out, _inp) = session();
    let surface = open_surface(&mut session);

    let (tx, rx) = flume::bounded(1);
    let builder = std::thread::spawn({
        let state = state.clone();
        move || {
            let mut blobs = blobs::Blobs::default();
            let (view, _ops) = view::document(&mut blobs, &state, &mut FrontendState::default());
            // the cover's blob was never sent, so the first document cannot draw it
            assert!(
                blobs.known(COVER_URL).is_none(),
                "the first build has no blob id for the cover"
            );
            assert!(
                !draws_cover(view, "unknown"),
                "the first build draws no cover blob"
            );
            let _ = tx.send(blobs);
        }
    });

    let mut blobs = rx.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|_| {
        panic!("the document build never returned: it encoded a cover under the data guard it holds")
    });
    builder.join().expect("the build thread");

    // sending the blob is the session's job, and it runs with no guard held
    blobs.resolve(&mut session, &state);
    let (_, messages) = written(&out);
    let blob = blobs.known(COVER_URL).expect("the blob was sent");
    assert!(
        messages
            .iter()
            .any(|(verb, body)| verb == "b" && body.contains(&blob)),
        "the cover blob goes out by its id: {messages:?}"
    );

    // and the frame after it names that blob
    let (view, _ops) = view::document(&mut blobs, &state, &mut FrontendState::default());
    session.render(surface, view).expect("render");
    let frame = frames(&written(&out).1);
    assert_eq!(frame.len(), 1, "one frame: {frame:?}");
    assert!(
        frame[0].to_string().contains(&blob),
        "the frame draws the cover blob: {frame:?}"
    );
}

#[cfg(feature = "image")]
#[test]
fn a_playing_track_with_cover_art_is_drawn_without_blocking() {
    a_cover_is_encoded_off_the_build_thread(play_track_with_cover);
}

/// A build must survive the state writes that go on around it: the client task, the player-event
/// watcher and the frontend's own loop write `data`, `player` and `view` while a document is being
/// built. A build that re-entered one of those locks, or took them out of the shared order
/// (`view` -> `data` -> `player`), would park behind a writer — so the builds run on their own
/// thread and the test fails on the timeout instead of hanging the suite.
#[test]
fn a_build_survives_concurrent_state_writes() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    let state = fixture();
    let stop = Arc::new(AtomicBool::new(false));

    // writes one lock at a time, in the shared order, so the hammer itself can never close a cycle
    let writer = std::thread::spawn({
        let state = state.clone();
        let stop = stop.clone();
        move || {
            while !stop.load(Ordering::Relaxed) {
                {
                    let _guard = state.view.lock();
                }
                {
                    let _guard = state.data.write();
                }
                {
                    let _guard = state.player.write();
                }
                std::thread::yield_now();
            }
        }
    });

    let (tx, rx) = flume::bounded(1);
    let builder = std::thread::spawn({
        let state = state.clone();
        move || {
            let mut blobs = blobs::Blobs::default();
            let mut frontend = FrontendState::default();
            for _ in 0..25 {
                let (view, _ops) = view::document(&mut blobs, &state, &mut frontend);
                assert!(
                    !ids_and_selections(view).0.is_empty(),
                    "a document with nodes"
                );
            }
            let _ = tx.send(());
        }
    });

    let done = rx.recv_timeout(Duration::from_secs(15));
    stop.store(true, Ordering::Relaxed);
    writer.join().expect("the writer thread");
    done.expect("a document build parked behind a concurrent state write");
    builder.join().expect("the build thread");
}
