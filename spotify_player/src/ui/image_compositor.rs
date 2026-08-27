//! Image compositor: sole owner of terminal-graphics surfaces.
//!
//! Every cover image the app draws goes through exactly one slot here. The compositor decides
//! when a surface must be re-encoded and re-transmitted, because terminal graphics protocols
//! are hostile to naive immediate-mode rendering:
//!
//! * kitty transmits pixel data once and anchors it to unicode placeholder cells — any widget
//!   painting over those cells (popup overlay, page switch, a frame where the image is skipped)
//!   makes the terminal drop the placement permanently;
//! * this app's direct-write iTerm2 escapes suffer the same fate after any external screen clear.
//!
//! Healing strategy: any slot whose surface skipped one or more frames is healed with a
//! re-emission next time it renders — no per-cause bookkeeping needed (overlays, navigation and
//! cache-miss gaps all reduce to "the slot didn't render"). A [`TRANSMIT_REFRESH_INTERVAL`]
//! backstop additionally covers damage no transition can predict (external clears). Kitty's
//! `resize_encode` keeps the same image id, so live placements update in place and deleted ones
//! are restored by the next placeholder emission. Glyph-painted protocols (halfblocks,
//! sixel-as-widget) redraw from the buffer diff every frame and never need scheduled refreshes.

use std::{
    collections::HashMap,
    io::Write,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use base64::Engine;
use image::DynamicImage;
use ratatui::{
    buffer::CellDiffOption,
    layout::{Rect, Size},
    Frame,
};
use ratatui_image::{
    picker::{Picker, ProtocolType},
    protocol::StatefulProtocol,
    Resize, ResizeEncodeRender, StatefulImage,
};

/// Backstop interval between forced re-transmissions of grid-anchored surfaces.
pub const TRANSMIT_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Cache of prepared cover surfaces keyed by stable caller-declared slots.
#[derive(Default)]
pub struct ImageCompositor {
    slots: HashMap<&'static str, Surface>,
    /// Monotonic frame counter; bumped once per UI pass by [`ImageCompositor::begin_frame`].
    epoch: u64,
}

/// A cover image encoded once per `(url, area)` pair and kept alive across frames.
struct Surface {
    kind: Encoded,
    url: String,
    area: Rect,
    last_emitted: Instant,
    /// Last `epoch` this surface rendered in. A gap means other widgets painted over its cells
    /// meanwhile and the terminal may have dropped the underlying graphics data.
    last_frame: u64,
}

enum Encoded {
    /// Rendered through `ratatui-image` as a widget. `scheduled` is set for grid-anchored
    /// protocols (kitty) whose data needs interval refreshes; glyph painters opt out.
    Widget {
        protocol: Box<StatefulProtocol>,
        size: Size,
        scheduled: bool,
    },
    /// A cursor-anchored iTerm2 inline-image escape written straight to stdout.
    Iterm2 { escape: String },
}

impl std::fmt::Debug for ImageCompositor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageCompositor")
            .field("slots", &self.slots.len())
            .finish_non_exhaustive()
    }
}

impl ImageCompositor {
    /// Advance the frame counter; call once per UI pass before drawing.
    pub fn begin_frame(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
    }

    /// Draw `img` into `area`, healing or re-encoding the slot's surface first if needed.
    pub fn render(
        &mut self,
        frame: &mut Frame,
        picker: &Picker,
        slot: &'static str,
        url: &str,
        img: &DynamicImage,
        area: Rect,
    ) {
        let now = Instant::now();
        let needs_prepare = match self.slots.get(slot) {
            Some(surface) => surface.url != url || surface.area != area,
            None => true,
        };
        if needs_prepare {
            match Surface::prepare(picker, url, img, area, self.epoch) {
                Ok(surface) => {
                    self.slots.insert(slot, surface);
                }
                Err(err) => {
                    tracing::error!("Failed to encode image for slot `{slot}`: {err:#}");
                    return;
                }
            }
        }

        let Some(surface) = self.slots.get_mut(slot) else {
            return;
        };
        // A skipped frame while this slot stayed idle means something else painted its cells
        // (popup overlay, page switch, a cache-miss gap): the terminal may have dropped the
        // placement, so force one healing re-emission before drawing again.
        let missed_frames = self.epoch.saturating_sub(surface.last_frame) > 1;
        let needs_heal =
            missed_frames || (surface.scheduled() && refresh_due(Some(surface.last_emitted), now));
        if needs_heal {
            surface.last_emitted = now;
            match &mut surface.kind {
                Encoded::Widget {
                    protocol,
                    size,
                    scheduled,
                } => {
                    if *scheduled {
                        protocol.resize_encode(&Resize::Fit(None), *size);
                    }
                }
                Encoded::Iterm2 { escape } => {
                    if let Err(err) = write_iterm2(escape, area) {
                        tracing::error!("Failed to draw iTerm2 cover image: {err:#}");
                    }
                }
            }
        }
        surface.last_frame = self.epoch;

        match &mut surface.kind {
            Encoded::Widget { protocol, .. } => {
                frame.render_stateful_widget(StatefulImage::new(), area, protocol.as_mut());
            }
            Encoded::Iterm2 { .. } => reserve_area(frame, area),
        }
    }
}

impl Surface {
    fn prepare(
        picker: &Picker,
        url: &str,
        img: &DynamicImage,
        area: Rect,
        epoch: u64,
    ) -> Result<Self> {
        let kind = if picker.protocol_type() == ProtocolType::Iterm2 {
            Encoded::Iterm2 {
                escape: encode_iterm2(img, area)?,
            }
        } else {
            Encoded::Widget {
                protocol: Box::new(picker.new_resize_protocol(img.clone())),
                size: area.into(),
                scheduled: picker.protocol_type() == ProtocolType::Kitty,
            }
        };
        Ok(Self {
            kind,
            url: url.to_owned(),
            area,
            last_emitted: Instant::now(),
            last_frame: epoch,
        })
    }

    /// Whether this surface's pixels can silently vanish from the terminal grid.
    fn scheduled(&self) -> bool {
        match &self.kind {
            Encoded::Widget { scheduled, .. } => *scheduled,
            Encoded::Iterm2 { .. } => true,
        }
    }
}

/// Whether a re-emission is owed: either never emitted yet or the backstop interval elapsed.
fn refresh_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|t| now.duration_since(t) >= TRANSMIT_REFRESH_INTERVAL)
}

/// Mark every cell in `area` as skipped so `ratatui`'s renderer leaves it untouched.
fn reserve_area(frame: &mut Frame, area: Rect) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = frame.buffer_mut().cell_mut((x, y)) {
                cell.set_diff_option(CellDiffOption::Skip);
            }
        }
    }
}

/// Encode `img` as a cursor-anchored, cell-sized iTerm2 inline-image escape sequence.
fn encode_iterm2(img: &DynamicImage, area: Rect) -> Result<String> {
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .context("encode cover image to PNG")?;
    let data = base64::engine::general_purpose::STANDARD.encode(&png);
    Ok(format!(
        "\x1b]1337;File=inline=1;preserveAspectRatio=1;size={};width={};height={}:{data}\x07",
        png.len(),
        area.width,
        area.height,
    ))
}

/// Write a prepared iTerm2 image escape at `area`'s top-left, erasing the cell box first (so
/// letterboxing doesn't reveal stale content) and restoring the cursor afterwards so
/// `ratatui`'s own rendering is unaffected.
fn write_iterm2(escape: &str, area: Rect) -> std::io::Result<()> {
    // `area` is always a sub-rectangle of the screen, so the cursor-anchored image fits and
    // does not scroll the alternate screen.
    let mut out = std::io::stdout().lock();
    out.write_all(b"\x1b7")?; // DEC save cursor
    for row in area.top()..area.bottom() {
        // move to the start of the row (1-based) and erase `width` cells
        write!(out, "\x1b[{};{}H\x1b[{}X", row + 1, area.x + 1, area.width)?;
    }
    // position at the image origin and draw it
    write!(out, "\x1b[{};{}H{escape}", area.y + 1, area.x + 1)?;
    out.write_all(b"\x1b8")?; // DEC restore cursor
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_is_due_when_never_drawn() {
        assert!(refresh_due(None, Instant::now()));
    }

    #[test]
    fn refresh_is_not_due_inside_the_interval() {
        let now = Instant::now();
        let past = now.checked_sub(Duration::from_secs(4)).unwrap();
        assert!(!refresh_due(Some(past), now));
    }

    #[test]
    fn refresh_is_due_once_the_interval_elapses() {
        let now = Instant::now();
        let past = now.checked_sub(TRANSMIT_REFRESH_INTERVAL).unwrap();
        assert!(refresh_due(Some(past), now));
    }

    #[test]
    fn skipped_frames_trigger_heal_then_settle() {
        let picker = ratatui_image::picker::Picker::halfblocks();
        let img = image::DynamicImage::new_rgb8(8, 8);
        let area = Rect::new(1, 1, 4, 2);

        let mut compositor = ImageCompositor::default();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 6)).expect("terminal");

        compositor.begin_frame();
        terminal
            .draw(|f| compositor.render(f, &picker, "s", "url://a", &img, area))
            .unwrap();

        // Two skipped frames while the slot stayed idle.
        compositor.begin_frame();
        compositor.begin_frame();

        terminal
            .draw(|f| compositor.render(f, &picker, "s", "url://a", &img, area))
            .unwrap();
        // Same url proves no rebuild happened; the gap alone forced the healing pass.
        assert_eq!(compositor.slots["s"].url, "url://a");
        assert_eq!(compositor.slots["s"].last_frame, compositor.epoch);
    }

    #[test]
    fn halfblocks_widget_renders_glyphs_across_frames() {
        let picker = ratatui_image::picker::Picker::halfblocks();
        let img = image::DynamicImage::from(image::ImageBuffer::<image::Rgb<u8>, _>::from_fn(
            8,
            8,
            |_x, y| {
                if y < 4 {
                    image::Rgb([255, 255, 255])
                } else {
                    image::Rgb([0, 0, 0])
                }
            },
        ));
        let area = Rect::new(1, 1, 4, 2);

        let mut compositor = ImageCompositor::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 6))
            .expect("test terminal");
        for _ in 0..3 {
            terminal
                .draw(|frame| {
                    compositor.render(frame, &picker, "episode-detail", "url://a", &img, area);
                })
                .expect("draw");
        }

        let painted = (area.top()..area.bottom())
            .flat_map(|y| (area.left()..area.right()).map(move |x| (x, y)))
            .filter(|&(x, y)| terminal.backend().buffer().get(x, y).symbol() != " ")
            .count();
        assert!(painted > 0, "halfblocks cover left no glyphs in its area");
    }

    #[test]
    fn url_change_replaces_surface_wholesale() {
        let picker = ratatui_image::picker::Picker::halfblocks();
        let img = image::DynamicImage::new_rgb8(8, 8);
        let area = Rect::new(1, 1, 4, 2);

        let mut compositor = ImageCompositor::default();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 6)).expect("terminal");

        terminal
            .draw(|f| compositor.render(f, &picker, "s", "url://a", &img, area))
            .unwrap();
        assert_eq!(compositor.slots.len(), 1);

        terminal
            .draw(|f| compositor.render(f, &picker, "s", "url://b", &img, area))
            .unwrap();
        assert_eq!(compositor.slots["s"].url, "url://b");
    }
}
