use std::{collections::VecDeque, sync::Arc};

use parking_lot::Mutex;
use tracing::Subscriber;
use tracing_subscriber::Layer;

use crate::state::RedrawNotify;

pub struct BufferLayer {
    buffer: Arc<Mutex<VecDeque<String>>>,
    max_lines: usize,
    redraw: RedrawNotify,
}

impl BufferLayer {
    pub fn new(
        buffer: Arc<Mutex<VecDeque<String>>>,
        max_lines: usize,
        redraw: RedrawNotify,
    ) -> Self {
        Self {
            buffer,
            max_lines,
            redraw,
        }
    }
}

impl<S: Subscriber> Layer<S> for BufferLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);

        let level = event.metadata().level();
        let target = event.metadata().target();
        let line = format!(
            "{} {:>5} {}: {}",
            chrono::Local::now().format("%H:%M:%S"),
            level,
            target,
            visitor.message
        );

        let mut buf = self.buffer.lock();
        buf.push_back(line);
        while buf.len() > self.max_lines {
            buf.pop_front();
        }
        drop(buf);

        // The log view renders straight from this buffer, so a new line is new on-screen state.
        // Suppressed while this thread is inside a frame (a render path that logs must not
        // schedule the next frame) -- see `RedrawNotify::frame`.
        self.redraw.raise();
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn core::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }
}
