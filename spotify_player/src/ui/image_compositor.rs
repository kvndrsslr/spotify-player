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
//!
//! Burst coalescing: at UI refresh rates around 32ms, dragging quickly through an episode list
//! produces a new `(url, area)` target nearly every frame; encoding and transmitting a full
//! image per step starves the terminal's graphics pipeline and reads as images flickering or
//! vanishing. Requests that arrive within [`URL_SETTLE_WINDOW`] of the previous change only
//! *record* the desired target — the previous surface keeps painting (cheap: no cell damage, no
//! transmission) until inputs go quiet, and exactly one encode/transmit carries the final
//! selection.

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
    StatefulImage,
};

/// Backstop interval between forced re-transmissions of grid-anchored surfaces.
pub const TRANSMIT_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Quiet period a stream of image-target changes must reach before exactly one encode +
/// transmit is issued for the latest target.
const URL_SETTLE_WINDOW: Duration = Duration::from_millis(150);

/// Cache of prepared cover surfaces keyed by stable caller-declared slots.
#[derive(Default)]
pub struct ImageCompositor {
    slots: HashMap<&'static str, Slot>,
    /// Monotonic frame counter; bumped once per UI pass by [`ImageCompositor::begin_frame`].
    epoch: u64,
}

/// Per-slot state: the currently encoded surface plus burst-coalescing bookkeeping.
struct Slot {
    surface: Surface,
    /// The last *observed* request, distinct from `surface.url` while a burst is deferred.
    requested_url: Option<String>,
    requested_area: Option<Rect>,
    last_change: Option<Instant>,
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

    /// Draw the cover identified by `img`/`url` into `area`.
    ///
    /// Honors healing (skipped frames / interval backstop) and coalesces rapid target changes
    /// into a single re-encode after [`URL_SETTLE_WINDOW`] of silence.
    pub fn render(
        &mut self,
        frame: &mut Frame,
        picker: &Picker,
        slot_id: &'static str,
        url: &str,
        img: &DynamicImage,
        area: Rect,
    ) {
        let now = Instant::now();

        let slot = match self.slots.get_mut(slot_id) {
            Some(slot) => {
                // Only genuine target changes reset the settle clock; identical repeats are
                // free observations that let a deferred burst settle.
                if slot.requested_url.as_deref() != Some(url) || slot.requested_area != Some(area) {
                    slot.requested_url = Some(url.to_owned());
                    slot.requested_area = Some(area);
                    slot.last_change = Some(now);
                }

                let changed = slot.surface.url != url || slot.surface.area != area;
                let settled = slot
                    .last_change
                    .is_none_or(|t| now.duration_since(t) >= URL_SETTLE_WINDOW);
                if changed && settled {
                    match Surface::prepare(picker, url, img, area, self.epoch) {
                        Ok(surface) => slot.surface = surface,
                        Err(err) => {
                            tracing::error!("Failed to encode image for slot `{slot_id}`: {err:#}");
                            return;
                        }
                    }
                }
                slot
            }
            None => {
                // First sighting: encode immediately so the very first paint is correct.
                match Surface::prepare(picker, url, img, area, self.epoch) {
                    Ok(surface) => {
                        let slot = Slot {
                            surface,
                            requested_url: Some(url.to_owned()),
                            requested_area: Some(area),
                            last_change: Some(now),
                        };
                        self.slots.insert(slot_id, slot);
                        self.slots.get_mut(slot_id).expect("just inserted")
                    }
                    Err(err) => {
                        tracing::error!("Failed to encode image for slot `{slot_id}`: {err:#}");
                        return;
                    }
                }
            }
        };

        let surface = &mut slot.surface;

        // A skipped frame while this slot stayed idle means something else painted its cells
        // (popup overlay, page switch, a cache-miss gap) — or the terminal drifted for any
        // other reason (scrolled placement, dropped image data). A same-id re-encode can be
        // invisible in both cases: re-emitted placeholder rows are byte-identical, so
        // ratatui's diff emits nothing while the terminal-side pixels are gone. Healing
        // therefore rebuilds the surface with a FRESH image id: every placeholder row's
        // symbol changes, ratatui re-emits the whole block, and the terminal places the new
        // image at the current absolute cells.
        let missed_frames = self.epoch.saturating_sub(surface.last_frame) > 1;
        let needs_heal =
            missed_frames || (surface.scheduled() && refresh_due(Some(surface.last_emitted), now));
        if needs_heal {
            match Surface::prepare(picker, url, img, area, self.epoch) {
                Ok(fresh) => {
                    // Direct-write iTerm2 bypasses the buffer entirely: push the fresh
                    // escape now. Widget surfaces re-transmit through the render below.
                    if let Encoded::Iterm2 { escape } = &fresh.kind {
                        if let Err(err) = write_iterm2(escape, area) {
                            tracing::error!("Failed to draw iTerm2 cover image: {err:#}");
                        }
                    }
                    slot.surface = fresh;
                }
                Err(err) => {
                    tracing::error!("Failed to re-encode image for slot `{slot_id}`: {err:#}");
                }
            }
        }
        slot.surface.last_frame = self.epoch;
        slot.surface.last_emitted = now;

        let surface = &mut slot.surface;
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
    use parking_lot::Mutex;
    use std::sync::LazyLock;

    static AREA: LazyLock<Rect> = LazyLock::new(|| Rect::new(1, 1, 4, 2));

    fn banded_img() -> DynamicImage {
        image::DynamicImage::from(image::ImageBuffer::<image::Rgb<u8>, _>::from_fn(
            8,
            8,
            |_x, y| {
                if y < 4 {
                    image::Rgb([255, 255, 255])
                } else {
                    image::Rgb([0, 0, 0])
                }
            },
        ))
    }

    fn flat_img() -> DynamicImage {
        image::DynamicImage::new_rgb8(8, 8)
    }

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
        let picker = Picker::halfblocks();
        let img = flat_img();

        let mut compositor = ImageCompositor::default();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 6)).expect("terminal");

        compositor.begin_frame();
        terminal
            .draw(|f| compositor.render(f, &picker, "s", "url://a", &img, *AREA))
            .unwrap();

        // Two skipped frames while the slot stayed idle.
        compositor.begin_frame();
        compositor.begin_frame();

        terminal
            .draw(|f| compositor.render(f, &picker, "s", "url://a", &img, *AREA))
            .unwrap();
        // Same url proves no rebuild happened; the gap alone forced the healing pass.
        assert_eq!(compositor.slots["s"].surface.url, "url://a");
        assert_eq!(compositor.slots["s"].surface.last_frame, compositor.epoch);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn halfblocks_widget_renders_glyphs_across_frames() {
        let picker = Picker::halfblocks();
        let img = banded_img();

        let mut compositor = ImageCompositor::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 6))
            .expect("test terminal");
        for _ in 0..3 {
            terminal
                .draw(|frame| {
                    compositor.render(frame, &picker, "episode-detail", "url://a", &img, *AREA);
                })
                .expect("draw");
        }

        let painted = (AREA.top()..AREA.bottom())
            .flat_map(|y| (AREA.left()..AREA.right()).map(move |x| (x, y)))
            .filter(|&(x, y)| terminal.backend().buffer().get(x, y).symbol() != " ")
            .count();
        assert!(painted > 0, "halfblocks cover left no glyphs in its area");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn url_burst_defers_and_settles_on_latest_target() {
        let picker = Picker::halfblocks();
        let img = flat_img();

        let mut compositor = ImageCompositor::default();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 6)).expect("terminal");

        let first_target = "url://a";
        let last_target = "url://c";

        compositor.begin_frame();
        terminal
            .draw(|f| compositor.render(f, &picker, "s", first_target, &img, *AREA))
            .unwrap();
        assert_eq!(compositor.slots["s"].surface.url, first_target);

        // Rapidly cycling targets inside the settle window must not re-encode anything.
        for url in ["url://b", "url://c", "url://b", first_target] {
            std::thread::sleep(URL_SETTLE_WINDOW / 4);
            compositor.begin_frame();
            terminal
                .draw(|f| compositor.render(f, &picker, "s", url, &img, *AREA))
                .unwrap();
        }
        assert_eq!(
            compositor.slots["s"].surface.url, first_target,
            "bursting changes must not trigger re-encodes"
        );

        // Once inputs settle, exactly the latest target gets encoded: the first call records
        // the change and defers within its settle window, the next adopts it.
        terminal
            .draw(|f| compositor.render(f, &picker, "s", last_target, &img, *AREA))
            .unwrap();
        assert_eq!(
            compositor.slots["s"].surface.url, first_target,
            "freshly observed target must defer like any burst step"
        );

        std::thread::sleep(URL_SETTLE_WINDOW + Duration::from_millis(20));
        compositor.begin_frame();
        terminal
            .draw(|f| compositor.render(f, &picker, "s", last_target, &img, *AREA))
            .unwrap();
        assert_eq!(
            compositor.slots["s"].surface.url, last_target,
            "settled state must adopt the latest selection"
        );
    }
}
