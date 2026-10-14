use crate::{
    state::SharedState,
    visualization::{decay_for_elapsed, peak_decay_for_elapsed},
};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    widgets::{Bar, BarChart, BarGroup},
    Frame,
};

/// Maps a normalised amplitude [0, 1] to an RGB colour.
/// Quiet (0.0) → cool blue, medium → green, loud (1.0) → hot red.
fn bar_color(t: f32) -> Color {
    let (r, g, b) = if t < 0.5 {
        let s = t * 2.0;
        (
            (30.0 + 20.0 * s) as u8,
            (100.0 + 155.0 * s) as u8,
            (255.0 * (1.0 - s * 0.5)) as u8,
        )
    } else {
        let s = (t - 0.5) * 2.0;
        (
            (50.0 + 205.0 * s) as u8,
            (255.0 * (1.0 - s)) as u8,
            (128.0 * (1.0 - s)) as u8,
        )
    };
    Color::Rgb(r, g, b)
}

/// Render a frequency-band bar chart using live FFT data from the audio sink.
///
/// Bars are subsampled to the available rect width so they always fill the area
/// cleanly. Heights use a sqrt (perceptual) curve so quiet signals stay visible.
/// Each bar is coloured by its amplitude: cool blue (quiet) → green → hot red (loud).
pub fn render_audio_visualization(frame: &mut Frame, state: &SharedState, rect: Rect) {
    // display_decay interpolates bar heights smoothly between write() calls.
    // We normalise against peak_envelope (NOT the per-frame peak), so display_decay
    // no longer cancels out and bars genuinely fade between audio packets.
    //
    // vis_bands is only Some when enable_audio_visualization is true.
    let Some(vis_lock) = state.vis_bands.as_ref() else {
        return;
    };
    let guard = vis_lock.lock();
    if !guard.is_active {
        return;
    }
    let display_decay = decay_for_elapsed(guard.updated_at.elapsed());
    let peak_norm =
        (guard.peak_envelope * peak_decay_for_elapsed(guard.updated_at.elapsed())).max(1e-6);
    // Copy the fixed-size array by value — no heap allocation.
    let values = guard.values;
    drop(guard);
    let num_bars = (rect.width as usize).min(values.len()).max(1);
    // Multiply by 8 to use ratatui's eighth-block characters (▁▂▃▄▅▆▇█),
    // giving 8× the resolution of whole terminal rows.
    let max_val = u64::from(rect.height) * 8;

    let step = values.len() as f64 / num_bars as f64;
    let bars: Vec<Bar> = (0..num_bars)
        .map(|i| {
            let idx = ((i as f64 * step) as usize).min(values.len() - 1);
            // Normalise against the slow peak envelope, then apply inter-frame decay.
            // Sqrt (gamma 0.5) scaling boosts quiet signals without clipping louds.
            let norm = ((values[idx] * display_decay) / peak_norm)
                .clamp(0.0, 1.0)
                .powf(0.5);
            let val = (norm * max_val as f32) as u64;
            Bar::default()
                .value(val)
                .text_value("")
                .style(Style::default().fg(bar_color(norm)))
        })
        .collect();

    let chart = BarChart::default()
        .data(BarGroup::default().bars(&bars))
        .bar_width(1)
        .bar_gap(0)
        .max(max_val);

    frame.render_widget(chart, rect);
}
