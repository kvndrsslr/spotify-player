use super::{
    config, utils::construct_and_render_block, Alignment, Borders, Constraint, Ctx, Frame, Gauge,
    Layout, Line, LineGauge, Modifier, Paragraph, ProgressBarLayout, Rect, SharedState, Span,
    Style, Text, Wrap,
};
use crate::playback::{current_playback_spans, PlaybackSpan, PlaybackToken};

/// Render a playback window showing information about the current playback, which includes
/// - track title, artists, album
/// - playback metadata (playing state, repeat state, shuffle state, volume, device, etc)
/// - cover image (if `image` feature is enabled)
/// - playback progress bar
pub fn render_playback_window(
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut Ctx<'_>,
    rect: Rect,
) -> Rect {
    let (rect, other_rect) = split_rect_for_playback_window(state, rect);
    let rect = construct_and_render_block("Playback", &ui.tui.theme, Borders::ALL, frame, rect);

    let player = state.player.read();
    if let Some(ref playback) = player.playback {
        if let Some(item) = &playback.item {
            // Carve off the visualization rows here, inside the active-playback
            // branch, so the full rect is used when there is nothing playing.
            #[cfg(feature = "streaming")]
            let (rect, vis_rect) = {
                let configs = config::get_config();
                if configs.app_config.enable_audio_visualization
                    && state.is_local_streaming_active()
                {
                    let chunks = Layout::vertical([
                        Constraint::Fill(0),
                        Constraint::Length(crate::visualization::VIS_HEIGHT),
                    ])
                    .split(rect);
                    (chunks[0], Some(chunks[1]))
                } else {
                    (rect, None)
                }
            };

            let (metadata_rect, progress_bar_rect) = {
                // Render the track's cover image if `image` feature is enabled
                #[cfg(feature = "image")]
                {
                    let configs = config::get_config();
                    // Split the allocated rectangle into `metadata_rect`, `cover_img_rect` and `progress_bar_rect`
                    let (metadata_rect, cover_img_rect, progress_bar_rect) =
                        match configs.app_config.progress_bar_position {
                            config::ProgressBarPosition::Bottom => {
                                let ver_chunks = split_rect_for_progress_bar(rect); // rect, progress_bar_rect
                                let hor_chunks =
                                    split_rect_for_cover_img(ver_chunks.0, &ui.tui.picker); // cover_img_rect, metadata_rect
                                (hor_chunks.1, hor_chunks.0, ver_chunks.1)
                            }
                            config::ProgressBarPosition::Right => {
                                let hor_chunks = split_rect_for_cover_img(rect, &ui.tui.picker); // cover_img_rect, rect
                                let ver_chunks = split_rect_for_progress_bar(hor_chunks.1); // metadata_rect, progress_bar_rect
                                (ver_chunks.0, hor_chunks.0, ver_chunks.1)
                            }
                        };

                    let url = match item {
                        rspotify::model::PlayableItem::Track(track) => {
                            crate::utils::get_track_album_image_url(track).map(String::from)
                        }
                        rspotify::model::PlayableItem::Episode(episode) => {
                            crate::utils::get_episode_show_image_url(episode).map(String::from)
                        }
                        rspotify::model::PlayableItem::Unknown(_) => None,
                    };
                    if let Some(url) = url {
                        let data = state.data.read();
                        if let Some(img) = data.caches.images.get(&url) {
                            let picker = ui.tui.picker.clone();
                            ui.tui.image_compositor.render(
                                frame,
                                &picker,
                                "playback",
                                &url,
                                img,
                                cover_img_rect,
                            );
                        } else {
                            state.queue_image_fetch(&url);
                        }
                    }
                    (metadata_rect, progress_bar_rect)
                }

                #[cfg(not(feature = "image"))]
                {
                    let chunks = split_rect_for_progress_bar(rect);
                    (chunks.0, chunks.1)
                }
            };

            let playback_desc = {
                // the spans read the data cache; take its guard here rather than nesting one inside
                // the frame's other locks
                let data = state.data.read();
                playback_text(current_playback_spans(&data, &player), &ui.tui.theme)
            };
            frame.render_widget(Paragraph::new(playback_desc), metadata_rect);

            let duration = match item {
                rspotify::model::PlayableItem::Track(track) => track.duration,
                rspotify::model::PlayableItem::Episode(episode) => episode.duration,
                rspotify::model::PlayableItem::Unknown(item) => {
                    log::warn!("Unknown playback item: {item:?}");
                    return other_rect;
                }
            };

            let progress = std::cmp::min(
                player.playback_progress().expect("non-empty playback"),
                duration,
            );
            render_playback_progress_bar(frame, ui, progress, duration, progress_bar_rect);
            #[cfg(feature = "streaming")]
            if let Some(vis_r) = vis_rect {
                super::streaming::render_audio_visualization(frame, state, vis_r);
            }
            return other_rect;
        }
    }
    if player.playback_last_updated_time.is_none() {
        // Still waiting for the first successful playback fetch — show animated loading indicator
        const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let frame_idx = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            / 100) as usize
            % SPINNER_FRAMES.len();
        let vertical_chunks = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .split(rect);
        frame.render_widget(
            Paragraph::new(format!("{} Loading...", SPINNER_FRAMES[frame_idx]))
                .style(ui.tui.theme.playback_metadata())
                .alignment(Alignment::Center),
            vertical_chunks[1],
        );
    } else {
        frame.render_widget(
            Paragraph::new(
                "No playback found. Please start a new playback.\n \
                 Make sure there is a running Spotify device and try to connect to one using the `SwitchDevice` command."
            )
            .wrap(Wrap { trim: true }),
            rect,
        );
    }

    other_rect
}

fn split_rect_for_progress_bar(rect: Rect) -> (Rect, Rect) {
    let chunks = Layout::vertical([Constraint::Fill(0), Constraint::Length(1)]).split(rect);
    (chunks[0], chunks[1])
}

#[cfg(feature = "image")]
fn split_rect_for_cover_img(rect: Rect, picker: &ratatui_image::picker::Picker) -> (Rect, Rect) {
    let configs = config::get_config();
    let hor_chunks = Layout::horizontal([
        Constraint::Length(cover_img_length(configs, picker)),
        Constraint::Fill(0), // metadata_rect
    ])
    .spacing(1)
    .split(rect);
    let ver_chunks = Layout::vertical([
        Constraint::Length(configs.app_config.cover_img_width as u16), // cover_img_rect
    ])
    .split(hor_chunks[0]);

    (ver_chunks[0], hor_chunks[1])
}

/// Determine the cover image box's width in columns.
#[cfg(feature = "image")]
fn cover_img_length(configs: &config::Configs, picker: &ratatui_image::picker::Picker) -> u16 {
    match configs.app_config.cover_img_length {
        // When `cover_img_length` is `0` (the default), derive it from the terminal's cell aspect ratio
        0 => {
            let font_size = picker.font_size();
            let rows = configs.app_config.cover_img_width as u16;
            rows * font_size.height / font_size.width
        }
        length => length as u16,
    }
}

/// Converts the framework-agnostic playback description into styled text.
fn playback_text(lines: Vec<Vec<PlaybackSpan>>, theme: &crate::theme::Theme) -> Text<'static> {
    let mut playback_text = Text::default();

    for spans in lines {
        let spans = spans
            .into_iter()
            .map(|span| match span.token {
                // literal runs are drawn unstyled
                PlaybackToken::Literal => Span::raw(span.text),
                token => Span::styled(span.text, playback_style(token, theme)),
            })
            .collect::<Vec<_>>();

        playback_text.lines.push(Line::from(spans));
    }

    playback_text
}

/// The theme style of a playback run.
fn playback_style(token: PlaybackToken, theme: &crate::theme::Theme) -> Style {
    match token {
        PlaybackToken::Literal => Style::default(),
        PlaybackToken::Status => theme.playback_status(),
        PlaybackToken::Liked => theme.like(),
        PlaybackToken::Track | PlaybackToken::TrackNumber => theme.playback_track(),
        PlaybackToken::Artists => theme.playback_artists(),
        PlaybackToken::Album => theme.playback_album(),
        PlaybackToken::Genres => theme.playback_genres(),
        PlaybackToken::Metadata => theme.playback_metadata(),
    }
}

fn render_playback_progress_bar(
    frame: &mut Frame,
    ui: &mut Ctx<'_>,
    progress: chrono::Duration,
    duration: chrono::Duration,
    rect: Rect,
) {
    // Negative numbers can sometimes appear from progress.num_seconds() so this stops
    // them coming through into the ratios
    let ratio = (progress.num_seconds() as f64 / duration.num_seconds() as f64).clamp(0.0, 1.0);

    match config::get_config().app_config.progress_bar_type {
        config::ProgressBarType::Line => frame.render_widget(
            LineGauge::default()
                .filled_style(ui.tui.theme.playback_progress_bar())
                .unfilled_style(ui.tui.theme.playback_progress_bar_unfilled())
                .ratio(ratio)
                .label(Span::styled(
                    format!(
                        "{}/{}",
                        crate::utils::format_duration(&progress),
                        crate::utils::format_duration(&duration),
                    ),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
            rect,
        ),
        config::ProgressBarType::Rectangle => frame.render_widget(
            Gauge::default()
                .gauge_style(ui.tui.theme.playback_progress_bar())
                .ratio(ratio)
                .label(Span::styled(
                    format!(
                        "{}/{}",
                        crate::utils::format_duration(&progress),
                        crate::utils::format_duration(&duration),
                    ),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
            rect,
        ),
    }

    // update the progress bar's position stored inside the UI state
    ui.tui.playback_progress_bar_rect = ProgressBarLayout {
        row: rect.y,
        x: rect.x,
        width: rect.width,
    };
}

/// Split the given area into two, the first one for the playback window
/// and the second one for the main application's layout (popup, page, etc).
#[allow(unused_variables)]
fn split_rect_for_playback_window(state: &SharedState, rect: Rect) -> (Rect, Rect) {
    let configs = config::get_config();
    let playback_width = configs.app_config.layout.playback_window_height;
    // the playback window's width should not be smaller than the cover image's width + 1
    #[cfg(feature = "image")]
    let playback_width = std::cmp::max(configs.app_config.cover_img_width + 1, playback_width);

    // When visualization is enabled *and* librespot is actively streaming,
    // reserve extra rows for the bar chart. When the local player is idle
    // (e.g. playback on an external Spotify Connect device) the rows are not
    // reserved so the space is available to the rest of the layout.
    #[cfg(feature = "streaming")]
    let playback_width = playback_width
        + if configs.app_config.enable_audio_visualization && state.is_local_streaming_active() {
            crate::visualization::VIS_HEIGHT as usize
        } else {
            0
        };

    // add lines for top/bottom borders depending on the progress bar's position
    let num_lines = match configs.app_config.progress_bar_position {
        config::ProgressBarPosition::Bottom => 2,
        config::ProgressBarPosition::Right => 1,
    };
    let playback_width = (playback_width + num_lines) as u16;

    match configs.app_config.layout.playback_window_position {
        config::Position::Top => {
            let chunks =
                Layout::vertical([Constraint::Length(playback_width), Constraint::Fill(0)])
                    .split(rect);

            (chunks[0], chunks[1])
        }
        config::Position::Bottom => {
            let chunks =
                Layout::vertical([Constraint::Fill(0), Constraint::Length(playback_width)])
                    .split(rect);

            (chunks[1], chunks[0])
        }
    }
}
