//! Clean `LiveRegion` API sketch with multiple live-updating messages.
//!
//! The important part is the main loop:
//!
//! ```ignore
//! ui.commit(app.tick());
//! ui.draw_regions(app.live_regions())?;
//! ```
//!
//! The app owns message/tool state. `lilterm` owns terminal mechanics: stable
//! lines go into native scrollback, live regions are laid out in the inline
//! viewport and diff-rendered.

use std::collections::VecDeque;
use std::io;
use std::time::Duration;
use std::time::Instant;

use crossterm::event;
use crossterm::event::Event;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use lilterm::LiveRegion;
use lilterm::init;
use ratatui::buffer::Buffer;
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
const STREAM_INTERVAL: Duration = Duration::from_millis(18);
const COMMIT_INTERVAL: Duration = Duration::from_millis(45);

fn main() -> io::Result<()> {
    smol::block_on(async_main())
}

async fn async_main() -> io::Result<()> {
    let mut ui = init()?;
    let mut app = AgentUi::new();
    let (event_tx, event_rx) = smol::channel::unbounded();
    smol::spawn(read_terminal_events(event_tx)).detach();

    loop {
        match next_wake(&event_rx, app.poll_interval()).await {
            Wake::Event(Ok(event)) => app.handle_event(event),
            Wake::Event(Err(err)) => return Err(err),
            Wake::Tick => {}
        }

        ui.commit(app.tick());

        if app.should_quit {
            break;
        }

        ui.draw_regions(app.live_regions())?;
    }

    ui.finish()
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

struct AgentUi {
    textarea: TextArea<'static>,
    streams: Vec<FakeLiveStream>,
    should_quit: bool,
}

impl AgentUi {
    fn new() -> Self {
        Self {
            textarea: new_prompt_textarea(),
            streams: Vec::new(),
            should_quit: false,
        }
    }

    fn poll_interval(&self) -> Duration {
        if self.streams.is_empty() {
            IDLE_POLL_INTERVAL
        } else {
            ACTIVE_POLL_INTERVAL
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
        if prompt.trim().is_empty() || !self.streams.is_empty() {
            return;
        }

        self.textarea = new_prompt_textarea();

        self.streams.push(FakeLiveStream::new(
            "agent",
            "agent response",
            "agent> ",
            Color::Green,
            fake_agent_response(&prompt),
        ));
        self.streams.push(FakeLiveStream::new(
            "tool",
            "cargo test",
            "tool>  ",
            Color::Yellow,
            fake_tool_response(),
        ));
    }

    fn tick(&mut self) -> Vec<Line<'static>> {
        let mut history = Vec::new();
        for stream in &mut self.streams {
            history.extend(stream.tick());
        }
        self.streams.retain(|stream| !stream.is_finished());
        history
    }

    fn live_regions(&self) -> Vec<LiveRegion<'_>> {
        let mut regions = Vec::new();

        for stream in &self.streams {
            let height_stream = stream;
            let render_stream = stream;
            regions.push(
                LiveRegion::new(stream.id.as_str())
                    .fixed_height(3)
                    .height(move |_| height_stream.live_height())
                    .render(move |area, buffer| render_stream.render(area, buffer)),
            );
        }

        let prompt_height_app = self;
        let prompt_render_app = self;
        regions.push(
            LiveRegion::new("prompt")
                .min_height(3)
                .max_height(8)
                .height(move |width| prompt_height_app.prompt_height(width))
                .render(move |area, buffer| prompt_render_app.render_prompt(area, buffer)),
        );

        regions
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

    fn render_prompt(&self, area: Rect, buffer: &mut Buffer) {
        (&self.textarea).render(area, buffer);
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
    textarea.set_placeholder_text("Ask something; this will start two live regions...");
    textarea.set_placeholder_style(Style::default().fg(Color::DarkGray));
    textarea.set_cursor_line_style(Style::default());
    textarea.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    textarea.set_wrap_mode(WrapMode::WordOrGlyph);
    textarea
}

struct FakeLiveStream {
    id: String,
    title: &'static str,
    prefix: &'static str,
    color: Color,
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

impl FakeLiveStream {
    fn new(
        id: &'static str,
        title: &'static str,
        prefix: &'static str,
        color: Color,
        text: String,
    ) -> Self {
        let now = Instant::now();
        Self {
            id: id.to_string(),
            title,
            prefix,
            color,
            chars: text.chars().collect(),
            next_char: 0,
            live_tail: String::new(),
            pending_lines: VecDeque::new(),
            header_emitted: false,
            pending_blank_after_finish: true,
            last_stream_tick: now,
            last_commit_tick: now,
            started_at: now,
        }
    }

    fn tick(&mut self) -> Vec<Line<'static>> {
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
                    let history_line = self.history_line(line);
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
                let history_line = self.history_line(line);
                self.pending_lines.push_back(history_line);
            }
            self.pending_lines.push_back(Line::from(""));
            self.pending_blank_after_finish = false;
        }

        if (self.last_commit_tick.elapsed() >= COMMIT_INTERVAL
            || self.next_char >= self.chars.len())
            && let Some(line) = self.pending_lines.pop_front()
        {
            self.last_commit_tick = Instant::now();
            return vec![line];
        }

        Vec::new()
    }

    fn live_height(&self) -> u16 {
        if self.is_finished() { 0 } else { 3 }
    }

    fn render(&self, area: Rect, buffer: &mut Buffer) {
        if area.is_empty() {
            return;
        }

        let spinner = match self.started_at.elapsed().as_millis() / 120 % 4 {
            0 => "|",
            1 => "/",
            2 => "-",
            _ => "\\",
        };
        let body = if self.live_tail.is_empty() {
            "streaming...".to_string()
        } else {
            self.live_tail.clone()
        };

        Paragraph::new(body)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(self.color))
                    .title(format!(" {} {spinner} ", self.title)),
            )
            .wrap(Wrap { trim: false })
            .render(area, buffer);
    }

    fn history_line(&mut self, line: String) -> Line<'static> {
        if !self.header_emitted {
            self.header_emitted = true;
            return prefixed_line(
                self.prefix,
                line,
                Style::default().fg(self.color).add_modifier(Modifier::BOLD),
            );
        }
        Line::from(vec![
            Span::styled(
                " ".repeat(self.prefix.width()),
                Style::default().fg(self.color),
            ),
            Span::raw(line),
        ])
    }

    fn is_finished(&self) -> bool {
        self.next_char >= self.chars.len()
            && !self.pending_blank_after_finish
            && self.live_tail.is_empty()
            && self.pending_lines.is_empty()
    }
}

fn fake_agent_response(prompt: &str) -> String {
    format!(
        "I am handling this request:\n\n  {prompt}\n\nWhile I stream, completed lines are committed to scrollback. Only this partial tail remains live."
    )
}

fn fake_tool_response() -> String {
    "running cargo test\ncompiling lilterm\nrunning unit tests\ntest result: ok\n".to_string()
}

fn prefixed_line(prefix: &str, text: String, prefix_style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(prefix.to_string(), prefix_style),
        Span::raw(text),
    ])
}
