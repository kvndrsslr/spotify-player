//! The view model → Tern document.
//!
//! Node ids come from the SDK's key paths (`<parent>.<key>`, starting at the region root), so a
//! node can name the id of the node it wants selected: every selectable child here carries an
//! explicit `key`, and [`select`]/[`activate`] mirror a pointer event back into the same cursor the
//! keyboard moves.

use std::collections::VecDeque;

use spotify_player_core::{
    config,
    playback::{current_playback_spans, PlaybackSpan},
    state::{
        Album, Artist, ArtistFocusState, BidiDisplay, BrowsePageUIState, Context, ContextId,
        ContextPageType, ContextPageUIState, Cursor, DataReadGuard, Episode, Id, LibraryFocusState,
        LibraryPageUIState, PageState, PlaylistCreateCurrentField, PlaylistFolderItem, PopupState,
        SearchFocusState, SearchPageUIState as SearchPage, SearchResults, SharedState, Show,
        ShowFocusState, TextInput, Track, ViewState,
    },
    utils::{filtered_items_from_query, format_duration},
};
use tern_sdk::{
    ui::{self, Column, Text, TextAlign, Tone, Wrap},
    wire::{Op, RevealAt, ScrollBy},
    Session, View,
};

use rspotify::model::PlayableItem;

use super::blobs::Blobs;

/// The most `scroll` ops one frame carries for a single scroller.
const MAX_SCROLL_OPS: usize = 64;

/// What this frontend asked Tern to report.
#[derive(Clone, Debug)]
pub enum Msg {
    /// A row was clicked: move that list's selection to it.
    Select { target: Target, index: usize },
    /// A row was double-clicked: select it and choose it, as `Enter` would.
    Activate { target: Target, index: usize },
    /// One of the `actions` names this frontend puts on a node (`"play-pause"`, `"seek=0.4"`, …).
    Act(String),
    /// An `el` control changed: a named choice among a few (`PlaylistCreate`'s field).
    Choice { name: String, value: String },
}

/// What a pointer event addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// A list of the current page.
    Page(PageList),
    /// The open popup's list.
    Popup,
}

/// A list of a page, which resolves to that page's own cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageList {
    // library
    Playlists,
    SavedAlbums,
    FollowedArtists,
    SavedShows,
    // browse
    Categories,
    CategoryPlaylists,
    // search
    SearchTracks,
    SearchAlbums,
    SearchArtists,
    SearchPlaylists,
    SearchShows,
    SearchEpisodes,
    // context
    ContextTracks,
    ArtistTopTracks,
    ArtistLikedSongs,
    ArtistAlbums,
    ArtistRelated,
    ShowEpisodes,
}

impl PageList {
    /// The node key of this list's pane.
    const fn key(self) -> &'static str {
        match self {
            Self::Playlists | Self::CategoryPlaylists | Self::SearchPlaylists => "playlists",
            Self::SavedAlbums | Self::SearchAlbums | Self::ArtistAlbums => "albums",
            Self::FollowedArtists | Self::SearchArtists | Self::ArtistRelated => "artists",
            Self::SavedShows | Self::SearchShows => "shows",
            Self::Categories => "categories",
            Self::SearchTracks | Self::ContextTracks | Self::ArtistTopTracks => "tracks",
            Self::SearchEpisodes | Self::ShowEpisodes => "episodes",
            Self::ArtistLikedSongs => "liked-songs",
        }
    }

    /// The library focus state this pane belongs to.
    const fn library_focus(self) -> LibraryFocusState {
        match self {
            Self::SavedAlbums => LibraryFocusState::SavedAlbums,
            Self::FollowedArtists => LibraryFocusState::FollowedArtists,
            Self::SavedShows => LibraryFocusState::SavedShows,
            _ => LibraryFocusState::Playlists,
        }
    }
}

/// The read-only view model a document is built from.
///
/// Holding the data directly (rather than the [`SharedState`]) is what lets a test build a page
/// document without an application state; only cover art needs more, through [`Covers`].
pub struct Model<'a> {
    pub view: &'a ViewState,
    pub data: &'a DataReadGuard<'a>,
    pub player: &'a spotify_player_core::state::PlayerState,
    pub logs: &'a parking_lot::Mutex<VecDeque<String>>,
}

/// The cover-art path: absent when there is no blob cache (tests) or no `image` feature.
// without the `image` feature nothing reads these, but the document builders keep passing them
#[cfg_attr(not(feature = "image"), allow(dead_code))]
pub struct Covers<'a> {
    blobs: Option<&'a mut Blobs>,
    state: Option<&'a SharedState>,
}

impl Covers<'_> {
    /// A cover source that draws nothing, for documents built outside a session.
    #[cfg(test)]
    #[must_use]
    pub fn none() -> Self {
        Self {
            blobs: None,
            state: None,
        }
    }

    /// The cover of `url`, as an `image` node keyed `key`; `None` when it is not available yet.
    ///
    /// Never sends the blob itself: this runs under the data cache's read guard, and encoding a
    /// cover takes that cache's write lock. It asks [`Blobs::want`]s instead, and the blob goes out
    /// with the next [`Blobs::resolve`] — so a cover first drawn here appears a frame later.
    #[cfg(feature = "image")]
    fn node(&mut self, key: &str, url: Option<String>) -> Option<ui::Node<Msg>> {
        let (Some(blobs), Some(state)) = (self.blobs.as_mut(), self.state) else {
            return None;
        };
        let url = url?;

        if let Some(id) = blobs.known(&url) {
            return Some(
                ui::image()
                    .key(key)
                    .blob(id)
                    .alt("Cover art")
                    .max(ui::Bound::wh(ui::Extent::Ch(20.0), ui::Extent::Lines(5.0)))
                    .into(),
            );
        }

        blobs.want(&url);
        state.queue_image_fetch(&url);
        None
    }

    /// Without the `image` feature there is no cover art to draw.
    // the signature matches the `image` variant, so the callers need no `cfg`
    #[cfg(not(feature = "image"))]
    #[allow(clippy::unused_self)]
    fn node(&mut self, _key: &str, _url: Option<String>) -> Option<ui::Node<Msg>> {
        None
    }
}

/// What the frontend remembers between frames, to turn the view model's scroll offsets, focus and
/// playing lyric line into frame ops.
#[derive(Default)]
pub struct FrontendState {
    /// The offset each scroller was last drawn at, by node id.
    scrollers: std::collections::HashMap<String, usize>,
    /// The field that owns the caret, if any.
    focus: Option<String>,
    /// The lyric line last revealed.
    lyric_line: Option<usize>,
}

/// Builds the document for the current view model, plus the ops the document itself cannot carry
/// (scrolling, the caret and reveals), and sends the cover blobs it asked for.
pub fn build(
    session: &mut Session<Msg>,
    blobs: &mut Blobs,
    state: &SharedState,
    frontend: &mut FrontendState,
) -> (View<Msg>, Vec<Op>) {
    let (view, ops) = document(blobs, state, frontend);

    // No guard is held here, so the covers the document asked for can be encoded and sent.
    blobs.resolve(session, state);

    (view, ops)
}

/// The document for the current view model: what [`build`] draws before it sends any blob.
///
/// Takes one guard per `state` lock, in the shared order (`view` -> `data` -> `player`, see
/// `spotify_player_core::state::State`), and leaves no `state` lock alive when it returns: a cover's
/// bytes are encoded from the data cache (`State::image_blob`), which a build holding a guard on
/// that cache must not call — a nested acquisition blocks forever on the guard this very thread
/// holds. The covers it could not draw are left for [`build`] to send.
pub fn document(
    blobs: &mut Blobs,
    state: &SharedState,
    frontend: &mut FrontendState,
) -> (View<Msg>, Vec<Op>) {
    let view_state = state.view.lock();
    let data = state.data.read();
    let player = state.player.read();

    let model = Model {
        view: &view_state,
        data: &data,
        player: &player,
        logs: &state.logs,
    };

    let mut covers = Covers {
        blobs: Some(blobs),
        state: Some(state),
    };

    // The playback bar's lines read the data cache through the guard above, the same way the pages
    // do, and never take one of their own.
    let playback_spans = current_playback_spans(&data, &player);

    let mut ops = Vec::new();
    let mut view = View::new()
        .main(page_region(main(&model, &mut covers, frontend, &mut ops)))
        .dock(dock(&model, &mut covers, &playback_spans));

    if let Some(layer) = layer(&model, frontend, &mut ops) {
        view = view.layer(layer);
    }

    (view, ops)
}

/// The page body under a wrapper: the region root's id is the region name, so a page node that
/// *is* the root would have its key dropped and its subtree unnamed.
fn page_region(body: ui::Node<Msg>) -> ui::Node<Msg> {
    ui::col().child(body).into()
}

/// The page area: one body per [`PageState`] variant.
fn main(
    model: &Model<'_>,
    covers: &mut Covers<'_>,
    frontend: &mut FrontendState,
    ops: &mut Vec<Op>,
) -> ui::Node<Msg> {
    match model.view.current_page() {
        PageState::Library { state: page } => library(model, page),
        PageState::Search {
            line_input,
            current_query,
            state: page,
        } => search(model, line_input, current_query, page, frontend, ops),
        PageState::Context {
            id,
            context_page_type,
            state,
        } => context(
            model,
            id.as_ref(),
            context_page_type,
            state.as_ref(),
            covers,
            frontend,
            ops,
        ),
        PageState::Browse { state } => browse(model, state),
        PageState::Lyrics {
            track_uri,
            track,
            artists,
        } => lyrics(model, track_uri, track, artists, frontend, ops),
        PageState::Queue { scroll_offset } => queue(model, *scroll_offset, frontend, ops),
        PageState::CommandHelp { scroll_offset } => {
            command_help(model, *scroll_offset, frontend, ops)
        }
        PageState::Logs { scroll_offset } => logs(model, *scroll_offset, frontend, ops),
    }
}

/// A selectable row of a list.
#[derive(Clone, Debug)]
struct Row {
    /// Its identity, and the last part of its node id.
    key: String,
    label: String,
    detail: String,
    value: String,
    /// The row is the item being played.
    playing: bool,
}

impl Row {
    fn new(key: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            detail: String::new(),
            value: String::new(),
            playing: false,
        }
    }

    fn detail(mut self, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        if !detail.is_empty() {
            self.detail = detail;
        }
        self
    }

    fn value(mut self, value: impl Into<String>) -> Self {
        let value = value.into();
        if !value.is_empty() {
            self.value = value;
        }
        self
    }

    fn playing(mut self, playing: bool) -> Self {
        self.playing = playing;
        self
    }
}

/// A `list` of `rows` for `target`, selecting the row `selected` points at.
fn list(
    list_id: &str,
    target: Target,
    rows: &[Row],
    selected: Option<usize>,
    max_lines: Option<u32>,
) -> ui::Node<Msg> {
    let mut node = ui::list().key("list");

    if let Some(max_lines) = max_lines {
        node = node.max_lines(max_lines);
    }
    if let Some(row) = selected.and_then(|index| rows.get(index)) {
        node = node.selected(format!("{list_id}.{}", row.key));
    }

    for (index, row) in rows.iter().enumerate() {
        let mut item = ui::item(Text::Plain(row.label.clone())).key(row.key.clone());

        if !row.detail.is_empty() {
            item = item.detail(Text::Plain(row.detail.clone()));
        }
        if !row.value.is_empty() {
            item = item.value(Text::Plain(row.value.clone()));
        }
        if row.playing {
            item = item.tone(Tone::Success);
        }

        node = node.child(
            item.on_select(move |_| Msg::Select { target, index })
                .on_activate(move |_| Msg::Activate { target, index }),
        );
    }

    node.into()
}

/// A card wrapping a pane: a title, an accent ring when it holds the focus, and its list.
#[allow(clippy::too_many_arguments)]
fn pane(
    key: &str,
    head: &str,
    focused: bool,
    list_id: &str,
    target: Target,
    rows: &[Row],
    selected: Option<usize>,
    max_lines: Option<u32>,
    filter: Option<&str>,
) -> ui::Node<Msg> {
    let mut card = ui::card().key(key).head(Text::Plain(head.to_owned()));

    if focused {
        card = card.tone(Tone::Accent);
    }

    let mut node = list(list_id, target, rows, selected, max_lines);
    if let Some(filter) = filter {
        node = node.prop("filter", filter);
    }

    card.child(node).into()
}

/// The library page: its four panes, the focused one accented and selected.
fn library(model: &Model<'_>, page: &LibraryPageUIState) -> ui::Node<Msg> {
    let max_lines = Some(config::get_config().app_config.page_size_in_rows as u32);
    let filter = search_filter(model);
    let mut panes = ui::row().key("panes");

    for (list, head, rows) in library_panes(page, model.data) {
        let focused = page.focus == list.library_focus();
        let selected = library_cursor(page, list).selected();

        panes = panes.child(pane(
            list.key(),
            head,
            focused,
            &format!("main.page.library.panes.{}.list", list.key()),
            Target::Page(list),
            &rows,
            selected,
            max_lines,
            filter.as_deref(),
        ));
    }

    ui::col().key("page.library").child(panes).into()
}

/// A library pane: its list, its title and its rows.
type Pane = (PageList, &'static str, Vec<Row>);

/// The panes of the library page.
fn library_panes(page: &LibraryPageUIState, data: &DataReadGuard<'_>) -> Vec<Pane> {
    let playlists = data
        .user_data
        .folder_playlists_items(page.playlist_folder_id)
        .into_iter()
        .map(|item| {
            let key = match item {
                PlaylistFolderItem::Playlist(playlist) => format!("i:p:{}", uri(&playlist.id)),
                PlaylistFolderItem::Folder(folder) => format!("i:f:{}", folder.target_id),
            };
            Row::new(key, item.to_bidi_string())
        })
        .collect();

    vec![
        (PageList::Playlists, "Playlists", playlists),
        (
            PageList::SavedAlbums,
            "Albums",
            album_rows(&data.user_data.saved_albums),
        ),
        (
            PageList::FollowedArtists,
            "Artists",
            artist_rows(&data.user_data.followed_artists),
        ),
        (
            PageList::SavedShows,
            "Shows",
            show_rows(&data.user_data.saved_shows),
        ),
    ]
}

/// The pane's cursor in the library page.
fn library_cursor(page: &LibraryPageUIState, list: PageList) -> Cursor {
    match list {
        PageList::SavedAlbums => page.saved_album_list,
        PageList::FollowedArtists => page.followed_artist_list,
        PageList::SavedShows => page.saved_show_list,
        _ => page.playlist_list,
    }
}

fn uri(id: &impl Id) -> String {
    id.uri()
}

fn album_rows<'a>(albums: impl IntoIterator<Item = &'a Album>) -> Vec<Row> {
    albums
        .into_iter()
        .map(|album| {
            Row::new(format!("i:a:{}", uri(&album.id)), album.to_bidi_string())
                .value(album.release_date.clone())
        })
        .collect()
}

fn artist_rows<'a>(artists: impl IntoIterator<Item = &'a Artist>) -> Vec<Row> {
    artists
        .into_iter()
        .map(|artist| Row::new(format!("i:r:{}", uri(&artist.id)), artist.to_bidi_string()))
        .collect()
}

fn show_rows<'a>(shows: impl IntoIterator<Item = &'a Show>) -> Vec<Row> {
    shows
        .into_iter()
        .map(|show| {
            Row::new(format!("i:s:{}", uri(&show.id)), show.to_bidi_string())
                .detail(show.publisher.clone())
        })
        .collect()
}

/// The item being played, as `(uri, liked)` for the row decoration.
fn playing_uri(model: &Model<'_>) -> Option<String> {
    match model.player.playback.as_ref()?.item.as_ref()? {
        PlayableItem::Track(track) => track.id.as_ref().map(uri),
        PlayableItem::Episode(episode) => Some(uri(&episode.id)),
        PlayableItem::Unknown(_) => None,
    }
}

/// The track rows a context/search table shows, as the TUI's track table draws them.
fn track_rows<'a>(
    model: &Model<'_>,
    tracks: impl IntoIterator<Item = &'a Track>,
    show_added: bool,
) -> Vec<Row> {
    let playing = playing_uri(model);
    let configs = config::get_config();

    tracks
        .into_iter()
        .map(|track| {
            let label = if model.data.user_data.is_liked_track(track) {
                format!("{} {}", configs.app_config.liked_icon, track.display_name())
            } else {
                track.display_name().into_owned()
            };

            let album = track.album_info();
            let detail = if album.is_empty() {
                track.artists_info()
            } else {
                format!("{} • {album}", track.artists_info())
            };

            let value = if show_added && track.added_at > 0 {
                format!("{}  {}", added_date(track.added_at), clock(&track.duration))
            } else {
                clock(&track.duration)
            };

            Row::new(format!("i:t:{}", uri(&track.id)), label)
                .detail(detail)
                .value(value)
                .playing(playing.as_deref() == Some(&track.id.uri()))
        })
        .collect()
}

/// A duration as `m:ss`.
fn clock(duration: &std::time::Duration) -> String {
    format!("{}:{:02}", duration.as_secs() / 60, duration.as_secs() % 60)
}

/// The date a track was added, as the TUI shows it.
fn added_date(added_at: u64) -> String {
    let time = chrono::DateTime::from_timestamp_nanos(added_at as i64 * 1_000_000_000);

    if chrono::Utc::now() > time + chrono::Duration::days(30) {
        time.format("%b %d, %Y").to_string()
    } else {
        chrono_humanize::HumanTime::from(time).to_string()
    }
}

/// The episode rows a show/search list shows.
fn episode_rows<'a>(
    model: &Model<'_>,
    episodes: impl IntoIterator<Item = &'a Episode>,
) -> Vec<Row> {
    let playing = playing_uri(model);

    episodes
        .into_iter()
        .map(|episode| {
            Row::new(format!("i:e:{}", uri(&episode.id)), episode.name.clone())
                .detail(
                    episode
                        .show
                        .as_ref()
                        .map_or_else(String::new, |show| show.publisher.clone()),
                )
                .value(format!(
                    "{}  {}",
                    episode.release_date,
                    clock(&episode.duration)
                ))
                .playing(playing.as_deref() == Some(&episode.id.uri()))
        })
        .collect()
}

/// The query of an active search popup: the page's lists are filtered by it and mark its matches.
fn search_filter(model: &Model<'_>) -> Option<String> {
    match model.view.popup.as_ref() {
        Some(PopupState::Search { query }) if !query.is_empty() => Some(query.clone()),
        _ => None,
    }
}

/// The search page: the query field and the six result panes.
fn search(
    model: &Model<'_>,
    line_input: &TextInput,
    current_query: &str,
    page: &SearchPage,
    frontend: &mut FrontendState,
    ops: &mut Vec<Op>,
) -> ui::Node<Msg> {
    let field_id = "main.page.search.query";
    focus_op(
        frontend,
        ops,
        (page.focus == SearchFocusState::Input).then_some(field_id),
    );

    let field = ui::input()
        .key("query")
        .text(line_input.text())
        .cursor(text_input_cursor(line_input));

    let results = model.data.caches.search.get(current_query);
    let filter = search_filter(model);
    let mut panes = ui::col().key("results");

    for (list, head, rows) in search_panes(model, results) {
        let focused = page.focus == search_focus(list);
        let selected = search_cursor(page, list).selected();

        panes = panes.child(pane(
            list.key(),
            head,
            focused,
            &format!("main.page.search.results.{}.list", list.key()),
            Target::Page(list),
            &rows,
            selected,
            Some(10),
            filter.as_deref(),
        ));
    }

    ui::col()
        .key("page.search")
        .child(field)
        .child(panes)
        .into()
}

/// The caret's position in a text input, in UTF-16 units (the protocol's offsets).
fn text_input_cursor(input: &TextInput) -> usize {
    let (before, _, _) = input.split_at_cursor();
    before.encode_utf16().count()
}

fn search_focus(list: PageList) -> SearchFocusState {
    match list {
        PageList::SearchAlbums => SearchFocusState::Albums,
        PageList::SearchArtists => SearchFocusState::Artists,
        PageList::SearchPlaylists => SearchFocusState::Playlists,
        PageList::SearchShows => SearchFocusState::Shows,
        PageList::SearchEpisodes => SearchFocusState::Episodes,
        _ => SearchFocusState::Tracks,
    }
}

fn search_cursor(page: &SearchPage, list: PageList) -> Cursor {
    match list {
        PageList::SearchAlbums => page.album_list,
        PageList::SearchArtists => page.artist_list,
        PageList::SearchPlaylists => page.playlist_list,
        PageList::SearchShows => page.show_list,
        PageList::SearchEpisodes => page.episode_list,
        _ => page.track_list,
    }
}

/// The six panes of the search page, from the cached search results.
fn search_panes(model: &Model<'_>, results: Option<&SearchResults>) -> Vec<Pane> {
    let empty = SearchResults {
        tracks: Vec::new(),
        artists: Vec::new(),
        albums: Vec::new(),
        playlists: Vec::new(),
        shows: Vec::new(),
        episodes: Vec::new(),
    };
    let results = results.unwrap_or(&empty);

    let tracks = track_rows(
        model,
        model.view.search_filtered_items(&results.tracks),
        false,
    );
    let episodes = episode_rows(model, model.view.search_filtered_items(&results.episodes));

    let playlists = model
        .view
        .search_filtered_items(&results.playlists)
        .into_iter()
        .map(|playlist| {
            Row::new(
                format!("i:p:{}", uri(&playlist.id)),
                playlist.to_bidi_string(),
            )
        })
        .collect();

    vec![
        (PageList::SearchTracks, "Tracks", tracks),
        (
            PageList::SearchAlbums,
            "Albums",
            album_rows(model.view.search_filtered_items(&results.albums)),
        ),
        (
            PageList::SearchArtists,
            "Artists",
            artist_rows(model.view.search_filtered_items(&results.artists)),
        ),
        (PageList::SearchPlaylists, "Playlists", playlists),
        (
            PageList::SearchShows,
            "Shows",
            show_rows(model.view.search_filtered_items(&results.shows)),
        ),
        (PageList::SearchEpisodes, "Episodes", episodes),
    ]
}

/// The context page: the header rows and one of the five bodies.
fn context(
    model: &Model<'_>,
    id: Option<&ContextId>,
    page_type: &ContextPageType,
    page: Option<&ContextPageUIState>,
    covers: &mut Covers<'_>,
    frontend: &mut FrontendState,
    ops: &mut Vec<Op>,
) -> ui::Node<Msg> {
    let Some(id) = id else {
        return message_page(
            "page.context",
            "Cannot determine the current page's context",
        );
    };

    let mut body = ui::col().key("page.context");
    let mut children: Vec<ui::Node<Msg>> = Vec::new();

    match model.data.caches.context.get(&id.uri()) {
        None => children.push(ui::spinner().key("loading").label("Loading").into()),
        Some(context) => {
            children.push(
                ui::text(Text::Plain(context_header(model, context)))
                    .key("description")
                    .into(),
            );
            if let Context::Playlist { playlist, .. } = context {
                if !playlist.desc.is_empty() {
                    children.push(
                        ui::text(Text::Plain(playlist.desc.clone()))
                            .key("playlist-desc")
                            .into(),
                    );
                }
            }
            children.push(context_body(model, context, page, covers, frontend, ops));
        }
    }

    let _ = page_type;
    for child in children {
        body = body.child(child);
    }

    body.into()
}

/// The context's header line, as the TUI's description row shows it.
fn context_header(model: &Model<'_>, context: &Context) -> String {
    let description = context.description();

    match context {
        Context::Playlist { playlist, .. } => format!(
            "{description} | {}",
            if model.data.user_data.is_followed_playlist(playlist) {
                "Followed"
            } else {
                "Not Followed"
            }
        ),
        _ => description,
    }
}

/// The body of a context page, per its page state.
fn context_body(
    model: &Model<'_>,
    context: &Context,
    page: Option<&ContextPageUIState>,
    covers: &mut Covers<'_>,
    frontend: &mut FrontendState,
    ops: &mut Vec<Op>,
) -> ui::Node<Msg> {
    let Some(page) = page else {
        return ui::spinner().key("loading").label("Loading").into();
    };

    let filter = search_filter(model);

    match (context, page) {
        (
            Context::Playlist { tracks, .. }
            | Context::Album { tracks, .. }
            | Context::Tracks { tracks, .. },
            ContextPageUIState::Playlist { track_table }
            | ContextPageUIState::Album { track_table }
            | ContextPageUIState::Tracks { track_table },
        ) => {
            let filtered = model.view.search_filtered_items(tracks);
            let rows = track_rows(
                model,
                filtered,
                tracks.iter().any(|track| track.added_at > 0),
            );

            let mut node = list(
                "main.page.context.body.list",
                Target::Page(PageList::ContextTracks),
                &rows,
                track_table.selected(),
                None,
            );
            if let Some(filter) = filter {
                node = node.prop("filter", filter);
            }

            ui::col().key("body").child(node).into()
        }
        (
            Context::Artist {
                artist,
                top_tracks,
                albums,
                related_artists,
            },
            ContextPageUIState::Artist {
                top_track_table,
                album_table,
                related_artist_list,
                liked_track_table,
                focus,
            },
        ) => artist_body(
            model,
            artist,
            top_tracks,
            albums,
            related_artists,
            top_track_table,
            liked_track_table,
            album_table,
            related_artist_list,
            *focus,
            filter.as_deref(),
        ),
        (
            Context::Show {
                show,
                episodes,
                total_episodes,
            },
            ContextPageUIState::Show {
                episode_table,
                focus,
                description_scroll,
            },
        ) => show_body(
            model,
            show,
            episodes,
            *total_episodes,
            episode_table,
            *focus,
            *description_scroll,
            covers,
            frontend,
            ops,
            filter.as_deref(),
        ),
        (context, _) => {
            let kind = context_kind(context);
            // the cached context of an id can change kind between fetches, leaving the page's
            // declared state for another kind
            tracing::warn!(
                "The context page holds a {kind} context with a page state of another kind; \
                 showing a message"
            );
            ui::col()
                .key("body")
                .child(
                    ui::text(Text::Plain(format!(
                        "This {kind} cannot be shown on this page. Go back and open it again."
                    )))
                    .key("message")
                    .wrap(Wrap::Word),
                )
                .into()
        }
    }
}

/// What kind of item a cached context holds, for messages about it.
fn context_kind(context: &Context) -> &'static str {
    match context {
        Context::Playlist { .. } => "playlist",
        Context::Album { .. } => "album",
        Context::Artist { .. } => "artist",
        Context::Tracks { .. } => "track list",
        Context::Show { .. } => "show",
    }
}

/// The artist context page: top tracks, liked songs, albums and related artists.
#[allow(clippy::too_many_arguments)]
fn artist_body(
    model: &Model<'_>,
    artist: &Artist,
    top_tracks: &[Track],
    albums: &[Album],
    related_artists: &[Artist],
    top_track_table: &Cursor,
    liked_track_table: &Cursor,
    album_table: &Cursor,
    related_artist_list: &Cursor,
    focus: ArtistFocusState,
    filter: Option<&str>,
) -> ui::Node<Msg> {
    let top = model.view.search_filtered_items(top_tracks);
    let liked = model.data.user_data.liked_tracks_by_artist(artist);

    let panes = [
        (
            PageList::ArtistTopTracks,
            "Top Tracks",
            focus == ArtistFocusState::TopTracks,
            track_rows(model, top, false),
            top_track_table.selected(),
        ),
        (
            PageList::ArtistLikedSongs,
            "Liked Songs",
            focus == ArtistFocusState::LikedSongs,
            track_rows(model, model.view.search_filtered_items(&liked), false),
            liked_track_table.selected(),
        ),
        (
            PageList::ArtistAlbums,
            "Albums",
            focus == ArtistFocusState::Albums,
            album_rows(model.view.search_filtered_items(albums)),
            album_table.selected(),
        ),
        (
            PageList::ArtistRelated,
            "Related Artists",
            focus == ArtistFocusState::RelatedArtists,
            artist_rows(model.view.search_filtered_items(related_artists)),
            related_artist_list.selected(),
        ),
    ];

    let mut col = ui::col().key("body");

    for (list, head, focused, rows, selected) in panes {
        col = col.child(pane(
            list.key(),
            head,
            focused,
            &format!("main.page.context.body.{}.list", list.key()),
            Target::Page(list),
            &rows,
            selected,
            None,
            filter,
        ));
    }

    col.into()
}

/// The show context page: the episode list, the selected episode's cover and its description.
#[allow(clippy::too_many_arguments)]
// without the `image` feature the cover source is passed through unused
#[cfg_attr(not(feature = "image"), allow(unused_variables))]
fn show_body(
    model: &Model<'_>,
    show: &Show,
    episodes: &[Episode],
    total_episodes: usize,
    episode_table: &Cursor,
    focus: ShowFocusState,
    description_scroll: u16,
    covers: &mut Covers<'_>,
    frontend: &mut FrontendState,
    ops: &mut Vec<Op>,
    filter: Option<&str>,
) -> ui::Node<Msg> {
    let selected = episode_table.selected();
    let filtered = model.view.search_filtered_items(episodes);
    let rows = episode_rows(model, filtered.iter().copied());

    let episodes_list = pane(
        "episodes",
        &format!("{total_episodes} Episodes"),
        focus == ShowFocusState::Episodes,
        "main.page.context.body.episodes.list",
        Target::Page(PageList::ShowEpisodes),
        &rows,
        selected,
        None,
        filter,
    );

    let episode = selected.and_then(|index| filtered.get(index)).copied();
    let mut details = ui::col()
        .key("details")
        .child(ui::text(Text::Plain(format!("{} • {}", show.name, show.publisher))).key("head"));

    if let Some(episode) = episode {
        #[cfg(feature = "image")]
        if let Some(node) = covers.node("cover", episode.image_url.clone()) {
            details = details.child(node);
        }

        details = details.child(
            ui::text(Text::Plain(episode.description.clone()))
                .key("body")
                .wrap(Wrap::Word),
        );
    }

    // the description panel scrolls from the page's own offset
    scroll_ops(
        frontend,
        ops,
        "main.page.context.body.details-card.details.body",
        usize::from(description_scroll),
    );

    let mut card = ui::card().key("details-card").child(details);
    if focus == ShowFocusState::Details {
        card = card.tone(Tone::Accent);
    }

    ui::col()
        .key("body")
        .child(episodes_list)
        .child(card)
        .into()
}

/// The browse page: the categories, or a category's playlists.
fn browse(model: &Model<'_>, page: &BrowsePageUIState) -> ui::Node<Msg> {
    let filter = search_filter(model);

    match page {
        BrowsePageUIState::CategoryList { state } => {
            let rows: Vec<Row> = model
                .view
                .search_filtered_items(&model.data.browse.categories)
                .into_iter()
                .map(|category| Row::new(format!("i:c:{}", category.id), category.name.clone()))
                .collect();

            ui::col()
                .key("page.browse")
                .child(pane(
                    "categories",
                    "Categories",
                    true,
                    "main.page.browse.categories.list",
                    Target::Page(PageList::Categories),
                    &rows,
                    state.selected(),
                    None,
                    filter.as_deref(),
                ))
                .into()
        }
        BrowsePageUIState::CategoryPlaylistList { category, state } => {
            let mut body = ui::col().key("page.browse");

            match model.data.browse.category_playlists.get(&category.id) {
                None => body = body.child(ui::spinner().key("loading").label("Loading")),
                Some(playlists) => {
                    let rows: Vec<Row> = model
                        .view
                        .search_filtered_items(playlists)
                        .into_iter()
                        .map(|playlist| {
                            Row::new(
                                format!("i:p:{}", uri(&playlist.id)),
                                playlist.to_bidi_string(),
                            )
                        })
                        .collect();

                    body = body.child(pane(
                        "playlists",
                        &format!("{} Playlists", category.name),
                        true,
                        "main.page.browse.playlists.list",
                        Target::Page(PageList::CategoryPlaylists),
                        &rows,
                        state.selected(),
                        None,
                        filter.as_deref(),
                    ));
                }
            }

            body.into()
        }
    }
}

/// The lyrics page: the track header and the lyric lines.
fn lyrics(
    model: &Model<'_>,
    track_uri: &str,
    track: &str,
    artists: &str,
    frontend: &mut FrontendState,
    ops: &mut Vec<Op>,
) -> ui::Node<Msg> {
    let progress = model.player.playback_progress();
    let lyrics = model.data.caches.lyrics.get(track_uri);

    let Some(progress) = progress else {
        return message_page("page.lyrics", "No playback available");
    };

    let lyrics = match lyrics {
        None => {
            return ui::col()
                .key("page.lyrics")
                .child(ui::spinner().key("loading").label("Loading"))
                .into()
        }
        Some(None) => return message_page("page.lyrics", "Lyrics not found"),
        Some(Some(lyrics)) => lyrics,
    };

    let playing = lyrics.lines.iter().rposition(|(time, _)| *time <= progress);

    if frontend.lyric_line != playing {
        frontend.lyric_line = playing;
        if let Some(line) = playing {
            ops.push(Op::Reveal {
                id: format!("main.page.lyrics.body.l{line}"),
                at: RevealAt::Nearest,
            });
        }
    }

    let mut body = ui::col().key("body");
    for (index, (_, line)) in lyrics.lines.iter().enumerate() {
        let style = match playing {
            Some(playing) if index == playing => "strong accent",
            Some(playing) if index < playing => "muted",
            _ => "",
        };

        body = body.child(
            ui::text(Text::Spans(vec![ui::Span {
                t: line.clone(),
                s: (!style.is_empty()).then(|| style.to_owned()),
                fx: None,
                href: None,
            }]))
            .key(format!("l{index}"))
            .wrap(Wrap::None),
        );
    }

    ui::col()
        .key("page.lyrics")
        .child(ui::text(Text::Plain(format!("{track} by {artists}"))).key("head"))
        .child(body)
        .into()
}

/// The command help page: every binding the keymap contributes, with its description.
fn command_help(
    model: &Model<'_>,
    scroll_offset: usize,
    frontend: &mut FrontendState,
    ops: &mut Vec<Op>,
) -> ui::Node<Msg> {
    let keymaps = model
        .view
        .search_filtered_items(&config::get_config().keymap_config.keymaps);

    let mut map: std::collections::BTreeMap<spotify_player_core::command::Command, String> =
        std::collections::BTreeMap::new();
    for keymap in keymaps {
        if !keymap.include_in_help_screen() {
            continue;
        }
        let entry = map.entry(keymap.command).or_default();
        if !entry.is_empty() {
            entry.push_str(", ");
        }
        entry.push_str(&keymap.key_sequence.to_string());
    }

    let rows = map
        .iter()
        .map(|(command, keys)| {
            ui::TableRow::new(format!("c:{command:?}"))
                .cell("keys", keys.clone())
                .cell("description", command.desc())
        })
        .collect::<Vec<_>>();

    let table = ui::table()
        .key("commands")
        .col(Column::new("keys", "Shortcuts").priority(1.0))
        .col(Column::new("description", "Description").grow(1.0))
        .rows(rows);

    scroll_ops(frontend, ops, "main.page.help.commands", scroll_offset);

    ui::col().key("page.help").child(table).into()
}

/// The queue page: the current queue, as a table (its rows are not selectable in the app).
fn queue(
    model: &Model<'_>,
    scroll_offset: usize,
    frontend: &mut FrontendState,
    ops: &mut Vec<Op>,
) -> ui::Node<Msg> {
    let Some(queue) = model.player.queue.as_ref() else {
        return ui::col()
            .key("page.queue")
            .child(ui::spinner().key("loading").label("Loading"))
            .into();
    };

    let rows = queue
        .queue
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let (title, artists, duration) = match item {
                PlayableItem::Track(track) => (
                    track.name.clone(),
                    track
                        .artists
                        .iter()
                        .map(|artist| artist.name.clone())
                        .collect::<Vec<_>>()
                        .join(", "),
                    format_duration(&track.duration),
                ),
                PlayableItem::Episode(episode) => (
                    episode.name.clone(),
                    episode.show.publisher.clone(),
                    format_duration(&episode.duration),
                ),
                PlayableItem::Unknown(_) => (String::new(), String::new(), String::new()),
            };

            ui::TableRow::new(format!("q:{index}"))
                .cell("#", (index + 1).to_string())
                .cell("title", title)
                .cell("artists", artists)
                .cell("duration", duration)
        })
        .collect::<Vec<_>>();

    let table = ui::table()
        .key("rows")
        .col(Column::new("#", "#").align(TextAlign::End).priority(3.0))
        .col(Column::new("title", "Title").grow(1.0))
        .col(Column::new("artists", "Artists").grow(1.0).priority(1.0))
        .col(
            Column::new("duration", "Duration")
                .align(TextAlign::End)
                .priority(2.0),
        )
        .rows(rows);

    scroll_ops(frontend, ops, "main.page.queue.rows", scroll_offset);

    ui::col().key("page.queue").child(table).into()
}

/// The logs page: the application's recent log lines.
fn logs(
    model: &Model<'_>,
    scroll_offset: usize,
    frontend: &mut FrontendState,
    ops: &mut Vec<Op>,
) -> ui::Node<Msg> {
    let lines = model.logs.lock().iter().cloned().collect::<Vec<_>>();

    scroll_ops(frontend, ops, "main.page.logs.body", scroll_offset);

    ui::col()
        .key("page.logs")
        .child(
            ui::text(Text::Plain(lines.join("\n")))
                .key("body")
                .wrap(Wrap::None),
        )
        .into()
}

/// A page that only shows a message (a missing context, lyrics not found).
fn message_page(key: &str, message: &str) -> ui::Node<Msg> {
    ui::col()
        .key(key)
        .child(ui::text(Text::Plain(message.to_owned())).key("message"))
        .into()
}

/// Turns a page's scroll offset into `scroll` ops, once per change.
fn scroll_ops(frontend: &mut FrontendState, ops: &mut Vec<Op>, id: &str, offset: usize) {
    let previous = frontend
        .scrollers
        .insert(id.to_owned(), offset)
        .unwrap_or(offset);
    if previous == offset {
        return;
    }

    let (by, count) = if offset > previous {
        (ScrollBy::LineDown, offset - previous)
    } else {
        (ScrollBy::LineUp, previous - offset)
    };

    for _ in 0..count.min(MAX_SCROLL_OPS) {
        ops.push(Op::Scroll {
            id: id.to_owned(),
            by,
        });
    }
}

/// Records a caret move as a [`Op::Focus`], once per change.
fn focus_op(frontend: &mut FrontendState, ops: &mut Vec<Op>, id: Option<&str>) {
    let id = id.map(str::to_owned);
    if frontend.focus == id {
        return;
    }

    frontend.focus.clone_from(&id);
    ops.push(Op::Focus { id });
}

/// The playback bar: cover art, the playback description, the progress and the status strip.
///
/// `playback_spans` is what [`current_playback_spans`] returned for this frame, computed before the
/// build took its guards: the bar cannot read the data cache itself.
fn dock(
    model: &Model<'_>,
    covers: &mut Covers<'_>,
    playback_spans: &[Vec<PlaybackSpan>],
) -> ui::Node<Msg> {
    let player = model.player;

    let Some(item) = player
        .playback
        .as_ref()
        .and_then(|playback| playback.item.as_ref())
    else {
        return ui::col()
            .key("dock")
            .child(
                ui::card().key("bar").child(
                    ui::text("No playback found. Please start a new playback.").key("empty"),
                ),
            )
            .into();
    };

    let Some(playback) = player.buffered_playback.as_ref() else {
        return ui::col()
            .key("dock")
            .child(
                ui::card()
                    .key("bar")
                    .child(ui::spinner().key("loading").label("Loading")),
            )
            .into();
    };

    let mut body = ui::row().key("body");

    let url = match item {
        PlayableItem::Track(track) => {
            spotify_player_core::utils::get_track_album_image_url(track).map(String::from)
        }
        PlayableItem::Episode(episode) => {
            spotify_player_core::utils::get_episode_show_image_url(episode).map(String::from)
        }
        PlayableItem::Unknown(_) => None,
    };
    if let Some(node) = covers.node("cover", url) {
        body = body.child(node);
    }

    let mut info = ui::col().key("info");
    for (index, line) in playback_spans.iter().enumerate() {
        info = info.child(
            ui::text(Text::Spans(
                line.iter()
                    .map(|span| span_text(span.token, span.text.clone()))
                    .collect(),
            ))
            .key(format!("line{index}"))
            .wrap(Wrap::None),
        );
    }

    let duration = match item {
        PlayableItem::Track(track) => track.duration,
        PlayableItem::Episode(episode) => episode.duration,
        PlayableItem::Unknown(_) => chrono::TimeDelta::zero(),
    };
    let progress = player.playback_progress().unwrap_or_default().min(duration);
    let ratio = if duration.num_milliseconds() > 0 {
        (progress.num_milliseconds() as f64 / duration.num_milliseconds() as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let mut progress_node = ui::progress().key("bar").value(ratio).label(format!(
        "{}/{}",
        format_duration(&progress),
        format_duration(&duration)
    ));

    // a click cannot land at a position, so the bar offers the positions the app can seek to
    for step in 0..10 {
        let action = format!("seek={}", f64::from(step) / 10.0);
        progress_node = progress_node.on_menu(action.clone(), Msg::Act(action));
    }

    info = info.child(ui::row().key("progress-row").child(progress_node));

    let _ = playback;

    ui::col()
        .key("dock")
        .child(ui::card().key("bar").child(body.child(info)))
        .child(status(model))
        .into()
}

/// The status strip: playback state, liked, shuffle, repeat, volume and the device.
fn status(model: &Model<'_>) -> ui::Node<Msg> {
    let configs = config::get_config();
    let player = model.player;

    let Some(playback) = player.buffered_playback.as_ref() else {
        return ui::status().key("status").into();
    };

    let playing = if playback.is_playing {
        configs.app_config.play_icon.clone()
    } else {
        configs.app_config.pause_icon.clone()
    };
    let liked = player.currently_playing().is_some_and(|item| match item {
        PlayableItem::Track(track) => {
            spotify_player_core::state::Track::try_from_full_track(track.clone())
                .is_some_and(|track| model.data.user_data.is_liked_track(&track))
        }
        PlayableItem::Episode(_) | PlayableItem::Unknown(_) => false,
    });

    let mut strip = ui::status()
        .key("status")
        .child(
            ui::seg(playing)
                .key("state")
                .tone(Tone::Success)
                .on_click(Msg::Act("play-pause".to_owned())),
        )
        .child(
            ui::seg(configs.app_config.liked_icon.clone())
                .key("liked")
                .tone(if liked { Tone::Error } else { Tone::Muted })
                .on_click(Msg::Act("like".to_owned())),
        )
        .child(
            ui::seg("shuffle")
                .key("shuffle")
                .tone(if playback.shuffle_state {
                    Tone::Accent
                } else {
                    Tone::Muted
                })
                .on_click(Msg::Act("shuffle".to_owned())),
        );

    let repeat = <&'static str>::from(playback.repeat_state);
    strip = strip.child(
        ui::seg(format!("repeat: {repeat}"))
            .key("repeat")
            .tone(
                if playback.repeat_state == rspotify::model::RepeatState::Off {
                    Tone::Muted
                } else {
                    Tone::Accent
                },
            )
            .on_click(Msg::Act("repeat".to_owned())),
    );

    strip = strip.child(
        ui::seg(format!("{}%", playback.volume.unwrap_or_default()))
            .key("volume")
            .side(ui::Side::Right)
            .on_click(Msg::Act("mute".to_owned())),
    );

    if !playback.device_name.is_empty() {
        strip = strip.child(
            ui::seg(playback.device_name.clone())
                .key("device")
                .side(ui::Side::Right)
                .priority(2.0)
                .on_click(Msg::Act("devices".to_owned())),
        );
    }

    strip.into()
}

/// The open popup, as an overlay: its fields, or its list of rows.
fn layer(
    model: &Model<'_>,
    frontend: &mut FrontendState,
    ops: &mut Vec<Op>,
) -> Option<ui::Node<Msg>> {
    let popup = model.view.popup.as_ref()?;
    let (title, rows) = popup_rows(popup, model);
    let mut body = ui::col().key("body");

    // the search and playlist-create popups own text fields, which take the caret
    match popup {
        PopupState::Search { query } => {
            body = body.child(
                ui::input()
                    .key("field")
                    .text(query.clone())
                    .placeholder("Search"),
            );
            focus_op(frontend, ops, Some("layer.popup.body.field"));
        }
        PopupState::PlaylistCreate {
            name,
            desc,
            current_field,
        } => {
            let fields = [
                (PlaylistCreateCurrentField::Name, "name", name.text()),
                (PlaylistCreateCurrentField::Desc, "desc", desc.text()),
            ];

            for (field, key, text) in fields {
                if *current_field == field {
                    focus_op(frontend, ops, Some(&format!("layer.popup.body.{key}")));
                }
                let cursor = text.encode_utf16().count();
                body = body.child(ui::input().key(key).text(text).cursor(cursor));
            }

            // a radio group picks the field the keys type into (the TUI's Tab, by pointer)
            let mut group = ui::row().key("field-choice").gap(ui::Gap::Sm);
            for (field, key) in [
                (PlaylistCreateCurrentField::Name, "name"),
                (PlaylistCreateCurrentField::Desc, "desc"),
            ] {
                group = group.child(
                    ui::html::radio::<Msg>("field", key)
                        .checked(*current_field == field)
                        .on_change(|change| Msg::Choice {
                            name: String::from("field"),
                            value: change.value.as_str().unwrap_or_default().to_owned(),
                        }),
                );
            }
            body = body.child(group);
        }
        _ => {
            let is_list = popup.list_cursor().is_some();

            for (index, label) in rows.iter().enumerate() {
                let node = if is_list {
                    ui::Node::from(
                        ui::item(Text::Plain(label.clone()))
                            .key(format!("i:{index}"))
                            .on_select(move |_| Msg::Select {
                                target: Target::Popup,
                                index,
                            })
                            .on_activate(move |_| Msg::Activate {
                                target: Target::Popup,
                                index,
                            }),
                    )
                } else {
                    ui::Node::from(ui::text(Text::Plain(label.clone())).key(format!("i:{index}")))
                };

                body = body.child(node);
            }
        }
    }

    let mut overlay = ui::overlay()
        .key("popup")
        .head(Text::Plain(title))
        .modal(true)
        .child(body);

    if let Some(index) = popup.list_selected() {
        overlay = overlay
            .key("popup")
            .prop("selected", format!("layer.popup.body.i:{index}"));
    }

    Some(ui::col().key("layer").child(overlay).into())
}

/// The open popup's title and its rows (its list's items, or its lines of text).
fn popup_rows(popup: &PopupState, model: &Model<'_>) -> (String, Vec<String>) {
    let data = model.data;
    let player = model.player;

    match popup {
        PopupState::Search { query } => (String::from("Search"), vec![format!("/{query}")]),
        PopupState::DeviceList(..) => {
            let current = player
                .playback
                .as_ref()
                .and_then(|playback| playback.device.id.clone())
                .unwrap_or_default();

            (
                String::from("Devices"),
                player
                    .devices
                    .iter()
                    .map(|device| {
                        let name = if device.is_integrated {
                            format!("{} (integrated)", device.name)
                        } else {
                            device.name.clone()
                        };
                        let marker = if device.id == current { " *" } else { "" };
                        format!("{name}{marker} | {}", device.id)
                    })
                    .collect(),
            )
        }
        PopupState::ThemeList(themes, ..) => (String::from("Themes"), themes.clone()),
        PopupState::UserPlaylistList(action, ..) => {
            let (items, query) = match action {
                spotify_player_core::state::PlaylistPopupAction::Browse {
                    folder_id,
                    search_query,
                } => (
                    data.user_data.folder_playlists_items(*folder_id),
                    search_query,
                ),
                spotify_player_core::state::PlaylistPopupAction::AddTrack {
                    folder_id,
                    search_query,
                    ..
                }
                | spotify_player_core::state::PlaylistPopupAction::AddEpisode {
                    folder_id,
                    search_query,
                    ..
                } => (
                    data.user_data.modifiable_playlist_items(Some(*folder_id)),
                    search_query,
                ),
            };

            (
                String::from("User Playlists"),
                filtered_items_from_query(query, &items)
                    .into_iter()
                    .copied()
                    .map(BidiDisplay::to_bidi_string)
                    .collect(),
            )
        }
        PopupState::UserFollowedArtistList(..) => (
            String::from("User Followed Artists"),
            data.user_data
                .followed_artists
                .iter()
                .map(|artist| artist.name.clone())
                .collect(),
        ),
        PopupState::UserSavedAlbumList(..) => (
            String::from("User Saved Albums"),
            data.user_data
                .saved_albums
                .iter()
                .map(BidiDisplay::to_bidi_string)
                .collect(),
        ),
        PopupState::ArtistList(_, artists, _) => (
            String::from("Artists"),
            artists.iter().map(|artist| artist.name.clone()).collect(),
        ),
        PopupState::ActionList(item, ..) => {
            (format!("Actions on {}", item.name()), item.actions_desc())
        }
        PopupState::PlaylistCreate { .. } => (String::from("New Playlist"), Vec::new()),
        PopupState::ConfirmAction { message, .. } => {
            (String::from("Confirm"), vec![format!("{message} (y/n)")])
        }
    }
}

/// The span a playback run is drawn with, as one of Tern's span tokens.
fn span_text(token: spotify_player_core::playback::PlaybackToken, text: String) -> ui::Span {
    use spotify_player_core::playback::PlaybackToken as Token;

    let style = match token {
        Token::Literal => "",
        Token::Status => "success",
        Token::Liked => "error",
        Token::Track | Token::TrackNumber => "strong",
        Token::Artists => "accent",
        Token::Album => "warning",
        Token::Genres | Token::Metadata => "muted",
    };

    ui::Span {
        t: text,
        s: (!style.is_empty()).then(|| style.to_owned()),
        fx: None,
        href: None,
    }
}

/// Moves the selection a pointer event addressed, and focuses what it belongs to.
pub fn select(ui: &mut crate::ui::Ctx<'_>, target: Target, index: usize) {
    match target {
        Target::Page(list) => match ui.current_page_mut() {
            PageState::Library { state } => {
                let cursor = match list {
                    PageList::SavedAlbums => &mut state.saved_album_list,
                    PageList::FollowedArtists => &mut state.followed_artist_list,
                    PageList::SavedShows => &mut state.saved_show_list,
                    _ => &mut state.playlist_list,
                };
                cursor.select(Some(index));
                state.focus = list.library_focus();
            }
            PageState::Browse { state } => match state {
                BrowsePageUIState::CategoryList { state }
                | BrowsePageUIState::CategoryPlaylistList { state, .. } => {
                    state.select(Some(index));
                }
            },
            PageState::Search { state, .. } => {
                let cursor = match list {
                    PageList::SearchAlbums => &mut state.album_list,
                    PageList::SearchArtists => &mut state.artist_list,
                    PageList::SearchPlaylists => &mut state.playlist_list,
                    PageList::SearchShows => &mut state.show_list,
                    PageList::SearchEpisodes => &mut state.episode_list,
                    _ => &mut state.track_list,
                };
                cursor.select(Some(index));
                state.focus = search_focus(list);
            }
            PageState::Context {
                state: Some(state), ..
            } => match state {
                ContextPageUIState::Playlist { track_table }
                | ContextPageUIState::Album { track_table }
                | ContextPageUIState::Tracks { track_table } => track_table.select(Some(index)),
                ContextPageUIState::Artist {
                    top_track_table,
                    album_table,
                    related_artist_list,
                    liked_track_table,
                    focus,
                } => match list {
                    PageList::ArtistLikedSongs => {
                        liked_track_table.select(Some(index));
                        *focus = ArtistFocusState::LikedSongs;
                    }
                    PageList::ArtistAlbums => {
                        album_table.select(Some(index));
                        *focus = ArtistFocusState::Albums;
                    }
                    PageList::ArtistRelated => {
                        related_artist_list.select(Some(index));
                        *focus = ArtistFocusState::RelatedArtists;
                    }
                    _ => {
                        top_track_table.select(Some(index));
                        *focus = ArtistFocusState::TopTracks;
                    }
                },
                ContextPageUIState::Show {
                    episode_table,
                    focus,
                    ..
                } => {
                    episode_table.select(Some(index));
                    *focus = ShowFocusState::Episodes;
                }
            },
            _ => {}
        },
        Target::Popup => {
            if let Some(popup) = ui.popup.as_mut() {
                popup.list_select(Some(index));
            }
        }
    }
}

/// Runs what a double click addressed: select the row, then choose it, exactly as `Enter` does.
pub fn activate(
    ui: &mut crate::ui::Ctx<'_>,
    target: Target,
    index: usize,
    client_tx: &flume::Sender<spotify_player_core::client::ClientRequest>,
    state: &SharedState,
) -> anyhow::Result<()> {
    select(ui, target, index);
    crate::event::dispatch::handle_command(
        spotify_player_core::command::Command::ChooseSelected,
        client_tx,
        state,
        ui,
    )?;

    Ok(())
}

/// Mirrors an `el` choice back into the view model.
pub fn choice(ui: &mut crate::ui::Ctx<'_>, name: &str, value: &str) {
    if let Some(PopupState::PlaylistCreate { current_field, .. }) = ui.popup.as_mut() {
        if name == "field" {
            *current_field = match value {
                "desc" => PlaylistCreateCurrentField::Desc,
                _ => PlaylistCreateCurrentField::Name,
            };
        }
    }
}

/// The document a test builds without a session: the page, the dock and any open popup.
#[cfg(test)]
pub fn view_for_test(model: &Model<'_>, frontend: &mut FrontendState) -> (View<Msg>, Vec<Op>) {
    let mut covers = Covers::none();
    let mut ops = Vec::new();
    let mut view = View::new()
        .main(page_region(main(model, &mut covers, frontend, &mut ops)))
        // no playback state in these fixtures, so the dock has no lines to show
        .dock(dock(model, &mut covers, &[]));

    if let Some(layer) = layer(model, frontend, &mut ops) {
        view = view.layer(layer);
    }

    (view, ops)
}
