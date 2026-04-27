//! One long live message backed by native scrollback.
//!
//! This intentionally does *not* use the newline-gated commit model from the
//! other examples. Instead, it keeps one assistant message live while it grows.
//!
//! `draw_layout_tail` renders the layout into an offscreen virtual buffer,
//! inserts newly overflowed inline rows into native scrollback, and keeps only
//! the live tail above a pinned prompt editor.
//!
//! Run with:
//!
//! ```sh
//! cargo run --example long_message
//! ```

use std::io;
use std::time::Duration;
use std::time::Instant;

use crossterm::event;
use crossterm::event::Event;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use lilterm::Region;
use lilterm::ScrollbackTailState;
use lilterm::init;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use ratatui::widgets::Wrap;
use ratatui_textarea::TextArea;
use ratatui_textarea::WrapMode;

const ACTIVE_POLL_INTERVAL: Duration = Duration::from_millis(16);
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const STREAM_INTERVAL: Duration = Duration::from_millis(12);
const CHARS_PER_TICK: usize = 3;
const PROMPT_HEIGHT: u16 = 3;

// =============================================================================
// Actual lilterm usage
// =============================================================================

fn main() -> io::Result<()> {
    let mut session = init()?;
    let mut app = LongMessageApp::new();
    let mut tail_state = ScrollbackTailState::new();

    loop {
        if event::poll(app.poll_interval())? {
            app.handle_event(event::read()?);
        }

        app.tick();

        if app.should_quit {
            break;
        }

        session.draw_layout_tail(
            &mut tail_state,
            [Region::inline_min(1), Region::pinned_bottom(PROMPT_HEIGHT)],
            |frame| {
                app.render_message(frame.area(0), frame.buffer_mut());
                app.render_prompt(frame.area(1), frame.buffer_mut());
            },
        )?;
    }

    session.finish()
}

// =============================================================================
// Reproduction app
// =============================================================================

struct LongMessageApp {
    prompt: TextArea<'static>,
    source: Vec<char>,
    message: String,
    next_char: usize,
    last_stream_tick: Instant,
    should_quit: bool,
}

impl LongMessageApp {
    fn new() -> Self {
        Self {
            prompt: new_prompt(),
            source: long_message().chars().collect(),
            message: String::new(),
            next_char: 0,
            last_stream_tick: Instant::now(),
            should_quit: false,
        }
    }

    fn poll_interval(&self) -> Duration {
        if self.is_streaming() {
            ACTIVE_POLL_INTERVAL
        } else {
            IDLE_POLL_INTERVAL
        }
    }

    fn is_streaming(&self) -> bool {
        self.next_char < self.source.len()
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.handle_key(key),
            Event::Paste(text) => {
                self.prompt.insert_str(text.replace('\r', "\n"));
            }
            Event::Resize(_, _) => {}
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            self.should_quit = true;
            return;
        }

        match key.code {
            KeyCode::Esc => self.should_quit = true,
            _ => {
                self.prompt.input(key);
            }
        }
    }

    fn tick(&mut self) {
        while self.is_streaming() && self.last_stream_tick.elapsed() >= STREAM_INTERVAL {
            for _ in 0..CHARS_PER_TICK {
                let Some(ch) = self.source.get(self.next_char).copied() else {
                    break;
                };
                self.message.push(ch);
                self.next_char += 1;
            }
            self.last_stream_tick += STREAM_INTERVAL;
        }
    }

    fn message_paragraph(&self) -> Paragraph<'_> {
        // This is scrollback content. Do not include viewport chrome such as
        // blocks or borders here, because overflow rows are intentionally
        // inserted into native terminal scrollback.
        Paragraph::new(self.message.as_str()).wrap(Wrap { trim: true })
    }

    fn render_message(&self, area: Rect, buffer: &mut Buffer) {
        if area.is_empty() {
            return;
        }

        self.message_paragraph().render(area, buffer);
    }

    fn render_prompt(&self, area: Rect, buffer: &mut Buffer) {
        (&self.prompt).render(area, buffer);
    }
}

fn new_prompt() -> TextArea<'static> {
    let mut prompt = TextArea::default();
    prompt.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(" prompt pinned to bottom; type here or Esc to quit "),
    );
    prompt.set_placeholder_text("The growing message above should scroll, but currently does not.");
    prompt.set_placeholder_style(Style::default().fg(Color::DarkGray));
    prompt.set_cursor_line_style(Style::default());
    prompt.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    prompt.set_wrap_mode(WrapMode::WordOrGlyph);
    prompt
}

fn long_message() -> String {
    let paragraph = "This is a deliberately long streaming assistant message. It stays as one live paragraph rather than committing completed lines into terminal scrollback. The reproduction depends on terminal width because ratatui wraps the paragraph differently as the width changes. The prompt editor should remain pinned to the bottom while the assistant message area above it behaves like a terminal-backed scroll region. As more text arrives, the newly wrapped rows should become visible and older rows should move into native scrollback. The layout API renders into a tall virtual buffer, measures the inline rows that were touched, inserts overflow rows into native scrollback, and keeps only the tail live above the prompt. ";

    paragraph.repeat(8)
}
