//! Inline agent UI using normal ratatui widgets plus `ratatui-textarea`.
//!
//! This example is intentionally closer to how an app would be structured:
//!
//! - Prompt input is a real `ratatui_textarea::TextArea` widget.
//! - The live viewport is rendered with ordinary ratatui widgets (`Paragraph`,
//!   `Block`, etc.).
//! - Completed stream lines are committed to native terminal scrollback with
//!   `lilterm::InlineViewport::insert_history_lines`.
//! - The event loop uses `smol` and async/await. Blocking crossterm reads are
//!   isolated with `smol::unblock`.

use std::collections::VecDeque;
use std::io;
use std::time::Duration;
use std::time::Instant;

use crossterm::event;
use crossterm::event::Event;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use lilterm::Frame;
use lilterm::init;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use ratatui::widgets::Wrap;
use ratatui_textarea::TextArea;
use ratatui_textarea::WrapMode;
use unicode_width::UnicodeWidthStr;

const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const ACTIVE_POLL_INTERVAL: Duration = Duration::from_millis(16);
const STREAM_INTERVAL: Duration = Duration::from_millis(16);
const COMMIT_INTERVAL: Duration = Duration::from_millis(45);
const LIVE_TAIL_HEIGHT: u16 = 3;

// =============================================================================
// Actual lilterm usage
// =============================================================================

fn main() -> io::Result<()> {
    smol::block_on(async_main())
}

async fn async_main() -> io::Result<()> {
    let mut session = init()?;
    let mut app = WidgetApp::new();
    let (event_tx, event_rx) = smol::channel::unbounded();
    smol::spawn(read_terminal_events(event_tx)).detach();

    loop {
        match next_wake(&event_rx, app.poll_interval()).await {
            Wake::Event(Ok(event)) => app.handle_event(event),
            Wake::Event(Err(err)) => return Err(err),
            Wake::Tick => {}
        }

        let tick = app.tick();
        if !tick.history_lines.is_empty() {
            // The completed/stable stream lines go to native scrollback.
            session.insert_history_lines(tick.history_lines);
        }

        if app.should_quit {
            break;
        }

        let size = session.terminal().size()?;
        let viewport_height = app.desired_height(size.width, size.height);
        // `draw` is safe to call whenever the loop wakes. lilterm diffs the
        // rendered buffer and only writes terminal updates when something
        // actually changed.
        session.draw(viewport_height, |frame| app.render(frame))?;
    }

    session.finish()
}

async fn read_terminal_events(tx: smol::channel::Sender<io::Result<Event>>) {
    loop {
        let event = smol::unblock(event::read).await;
        if tx.send(event).await.is_err() {
            break;
        }
    }
}

async fn next_wake(rx: &smol::channel::Receiver<io::Result<Event>>, interval: Duration) -> Wake {
    smol::future::race(
        async {
            match rx.recv().await {
                Ok(event) => Wake::Event(event),
                Err(_) => Wake::Tick,
            }
        },
        async {
            smol::Timer::after(interval).await;
            Wake::Tick
        },
    )
    .await
}

#[derive(Debug)]
enum Wake {
    Event(io::Result<Event>),
    Tick,
}

// =============================================================================
// App-specific widget state
// =============================================================================

struct WidgetApp {
    textarea: TextArea<'static>,
    stream: Option<FakeAgentStream>,
    should_quit: bool,
}

impl WidgetApp {
    fn new() -> Self {
        Self {
            textarea: new_prompt_textarea(),
            stream: None,
            should_quit: false,
        }
    }

    fn poll_interval(&self) -> Duration {
        if self.stream.is_some() {
            ACTIVE_POLL_INTERVAL
        } else {
            IDLE_POLL_INTERVAL
        }
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.handle_key(key),
            Event::Paste(text) => {
                self.textarea.insert_str(text.replace('\r', "\n"));
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
            KeyCode::Enter if key.modifiers.is_empty() => self.submit_prompt(),
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.textarea.insert_newline();
            }
            _ => {
                self.textarea.input(key);
            }
        }
    }

    fn submit_prompt(&mut self) {
        let prompt = self.textarea.lines().join("\n");
        if prompt.trim().is_empty() || self.stream.is_some() {
            return;
        }

        self.textarea = new_prompt_textarea();
        self.stream = Some(FakeAgentStream::new(prompt));
    }

    fn tick(&mut self) -> TickOutcome {
        let Some(stream) = self.stream.as_mut() else {
            return TickOutcome::default();
        };

        let outcome = stream.tick();
        if stream.is_done() && stream.live_tail.is_empty() && stream.pending_lines.is_empty() {
            self.stream = None;
        }
        outcome
    }

    fn desired_height(&self, terminal_width: u16, terminal_height: u16) -> u16 {
        let prompt_height = self
            .prompt_height(terminal_width)
            .min(terminal_height.max(1));
        let live_height = if self.stream.is_some() {
            LIVE_TAIL_HEIGHT.min(terminal_height.saturating_sub(prompt_height))
        } else {
            0
        };
        prompt_height
            .saturating_add(live_height)
            .clamp(1, terminal_height.max(1))
    }

    fn prompt_height(&self, width: u16) -> u16 {
        let inner_width = width.saturating_sub(2).max(1) as usize;
        let text_rows = self
            .textarea
            .lines()
            .iter()
            .map(|line| line.width().max(1).div_ceil(inner_width))
            .sum::<usize>()
            .max(1);
        (text_rows as u16).saturating_add(2).clamp(3, 8)
    }

    fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        if area.is_empty() {
            return;
        }

        let prompt_height = self.prompt_height(area.width).min(area.height);
        let live_height = if self.stream.is_some() {
            LIVE_TAIL_HEIGHT.min(area.height.saturating_sub(prompt_height))
        } else {
            0
        };
        let live_area = Rect::new(area.x, area.y, area.width, live_height);
        let prompt_area = Rect::new(
            area.x,
            area.bottom().saturating_sub(prompt_height),
            area.width,
            prompt_height,
        );

        if live_area.height > 0 {
            self.render_live_tail(frame, live_area);
        }

        // `ratatui-textarea` implements ratatui's `Widget` trait for `&TextArea`,
        // so this is normal widget rendering into lilterm's frame buffer.
        (&self.textarea).render(prompt_area, frame.buffer_mut());
    }

    fn render_live_tail(&self, frame: &mut Frame, area: Rect) {
        let Some(stream) = &self.stream else {
            return;
        };

        let spinner = match stream.started_at.elapsed().as_millis() / 120 % 4 {
            0 => "|",
            1 => "/",
            2 => "-",
            _ => "\\",
        };
        let body = if stream.live_tail.is_empty() {
            "streaming...".to_string()
        } else {
            stream.live_tail.clone()
        };

        Paragraph::new(body)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Blue))
                    .title(format!(" live tail {spinner} ")),
            )
            .wrap(Wrap { trim: false })
            .render(area, frame.buffer_mut());
    }
}

fn new_prompt_textarea() -> TextArea<'static> {
    let mut textarea = TextArea::default();
    textarea.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(" prompt: Enter submit, Ctrl-J newline, Esc quit "),
    );
    textarea.set_placeholder_text("Ask the agent to do something...");
    textarea.set_placeholder_style(Style::default().fg(Color::DarkGray));
    textarea.set_cursor_line_style(Style::default());
    textarea.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    textarea.set_wrap_mode(WrapMode::WordOrGlyph);
    textarea
}

// =============================================================================
// Fake backend support code
// =============================================================================

#[derive(Debug, Default)]
struct TickOutcome {
    history_lines: Vec<Line<'static>>,
}

#[derive(Debug)]
struct FakeAgentStream {
    chars: Vec<char>,
    next_char: usize,
    live_tail: String,
    pending_lines: VecDeque<Line<'static>>,
    header_emitted: bool,
    pending_blank_after_finish: bool,
    last_stream_tick: Instant,
    last_commit_tick: Instant,
    started_at: Instant,
}

impl FakeAgentStream {
    fn new(prompt: String) -> Self {
        let now = Instant::now();
        let mut pending_lines = VecDeque::new();
        for line in user_prompt_lines(&prompt) {
            pending_lines.push_back(line);
        }
        pending_lines.push_back(Line::from(""));

        Self {
            chars: fake_response(&prompt).chars().collect(),
            next_char: 0,
            live_tail: String::new(),
            pending_lines,
            header_emitted: false,
            pending_blank_after_finish: true,
            last_stream_tick: now,
            last_commit_tick: now,
            started_at: now,
        }
    }

    fn tick(&mut self) -> TickOutcome {
        let mut outcome = TickOutcome::default();

        while self.next_char < self.chars.len()
            && self.last_stream_tick.elapsed() >= STREAM_INTERVAL
        {
            for _ in 0..2 {
                let Some(ch) = self.chars.get(self.next_char).copied() else {
                    break;
                };
                self.next_char += 1;
                if ch == '\n' {
                    let line = std::mem::take(&mut self.live_tail);
                    let history_line = self.agent_history_line(line);
                    self.pending_lines.push_back(history_line);
                } else if ch != '\r' {
                    self.live_tail.push(ch);
                }
            }
            self.last_stream_tick += STREAM_INTERVAL;
        }

        if self.next_char >= self.chars.len() && self.pending_blank_after_finish {
            if !self.live_tail.is_empty() {
                let line = std::mem::take(&mut self.live_tail);
                let history_line = self.agent_history_line(line);
                self.pending_lines.push_back(history_line);
            }
            self.pending_lines.push_back(Line::from(""));
            self.pending_blank_after_finish = false;
        }

        if (self.last_commit_tick.elapsed() >= COMMIT_INTERVAL
            || self.next_char >= self.chars.len())
            && let Some(line) = self.pending_lines.pop_front()
        {
            outcome.history_lines.push(line);
            self.last_commit_tick = Instant::now();
        }

        outcome
    }

    fn agent_history_line(&mut self, line: String) -> Line<'static> {
        if !self.header_emitted {
            self.header_emitted = true;
            return prefixed_line(
                "agent> ",
                line,
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            );
        }
        Line::from(vec![
            Span::styled("       ".to_string(), Style::default().fg(Color::Green)),
            Span::raw(line),
        ])
    }

    fn is_done(&self) -> bool {
        self.next_char >= self.chars.len() && !self.pending_blank_after_finish
    }
}

fn fake_response(prompt: &str) -> String {
    format!(
        "I'll work on this prompt:\n\n  {prompt}\n\nThis example uses actual ratatui widget rendering for the live viewport and ratatui-textarea for the prompt. Completed stream lines are committed into native scrollback as they become stable, while only the partial tail stays live."
    )
}

fn user_prompt_lines(prompt: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (idx, line) in prompt.lines().enumerate() {
        if idx == 0 {
            lines.push(prefixed_line(
                "user> ",
                line.to_string(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            lines.push(Line::from(vec![
                Span::styled("      ".to_string(), Style::default().fg(Color::Cyan)),
                Span::raw(line.to_string()),
            ]));
        }
    }
    lines
}

fn prefixed_line(prefix: &str, text: String, prefix_style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(prefix.to_string(), prefix_style),
        Span::raw(text),
    ])
}
