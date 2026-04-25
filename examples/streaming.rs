//! Streaming inline TUI example.
//!
//! The file is split into two parts:
//!
//! 1. `Actual lilterm usage`: the small amount of code you copy into an app.
//!    It initializes a session, calls `insert_history_lines()` for completed
//!    stream chunks, and calls `draw()` for the live viewport.
//! 2. `Demo support code`: fake streaming, a tiny textarea, and rendering
//!    helpers used only to make this example runnable without an agent backend.

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
use ratatui::layout::Position;
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
use unicode_width::UnicodeWidthStr;

const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const ACTIVE_POLL_INTERVAL: Duration = Duration::from_millis(16);
const STREAM_INTERVAL: Duration = Duration::from_millis(18);
const STREAM_CHARS_PER_TICK: usize = 2;
const COMMIT_INTERVAL: Duration = Duration::from_millis(45);
const LIVE_TAIL_HEIGHT: u16 = 3;

// =============================================================================
// Actual lilterm usage
// =============================================================================
//
// In a real coding-agent UI, keep this shape and replace `App` with your own
// app state:
//
// - `init()` enters raw terminal mode and creates an inline viewport session.
// - `session.insert_history_lines(...)` commits completed stream chunks to
//   native terminal scrollback above the live viewport.
// - `session.draw(height, |frame| ...)` redraws only the live viewport.
// - `session.finish()` restores terminal modes before exit.

fn main() -> io::Result<()> {
    let mut session = init()?;
    let mut app = App::default();

    loop {
        let poll_interval = if app.is_streaming() {
            ACTIVE_POLL_INTERVAL
        } else {
            IDLE_POLL_INTERVAL
        };

        if event::poll(poll_interval)? {
            match event::read()? {
                Event::Key(key) => app.handle_key(key),
                Event::Paste(text) => {
                    app.input.insert_str(&text.replace('\r', "\n"));
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }

        let tick = app.tick();
        if !tick.history_lines.is_empty() {
            // This is the key agent-UI operation: once stream text has a stable
            // line to show, commit it into native scrollback instead of keeping
            // an ever-growing live widget in the viewport.
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

// =============================================================================
// Demo support code
// =============================================================================
//
// Everything below is support code for the standalone example. It is not part of
// lilterm's API surface. Swap it out for your own event source, composer, stream
// transport, and renderables.

#[derive(Debug, Default)]
struct App {
    input: PromptTextArea,
    stream: Option<FakeStream>,
    should_quit: bool,
    flash: Option<String>,
}

impl App {
    fn is_streaming(&self) -> bool {
        self.stream.is_some()
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            self.should_quit = true;
            return;
        }

        match self.input.handle_key(key) {
            TextAreaAction::Submit(prompt) => {
                if prompt.trim().is_empty() {
                    return;
                }
                if self.stream.is_some() {
                    self.flash = Some("still streaming; prompt kept in the textarea".to_string());
                    self.input.replace_text(prompt);
                    return;
                }
                self.flash = None;
                self.stream = Some(FakeStream::new(prompt));
            }
            TextAreaAction::Quit => self.should_quit = true,
            TextAreaAction::None => {}
        }
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
        let terminal_height = terminal_height.max(1);
        let input_height = self
            .input
            .desired_height(terminal_width)
            .min(terminal_height);
        let live_height = if self.stream.is_some() {
            LIVE_TAIL_HEIGHT.min(terminal_height.saturating_sub(input_height))
        } else {
            0
        };

        input_height
            .saturating_add(live_height)
            .clamp(1, terminal_height)
    }

    fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        if area.is_empty() {
            return;
        }

        let input_height = self.input.desired_height(area.width).min(area.height);
        let live_height = if self.stream.is_some() {
            LIVE_TAIL_HEIGHT.min(area.height.saturating_sub(input_height))
        } else {
            0
        };
        let input_area = Rect::new(
            area.x,
            area.bottom().saturating_sub(input_height),
            area.width,
            input_height,
        );
        let live_area = Rect::new(area.x, area.y, area.width, live_height);

        if live_area.height > 0 {
            self.render_live_tail(frame, live_area);
        }
        self.input.render(frame, input_area, self.flash.as_deref());
    }

    fn render_live_tail(&self, frame: &mut Frame, area: Rect) {
        let Some(stream) = &self.stream else {
            return;
        };

        let elapsed = stream.started_at.elapsed().as_millis() / 120;
        let spinner = match elapsed % 4 {
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
        let title = format!(" live tail {spinner} ");
        let paragraph = Paragraph::new(body)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Blue))
                    .title(title),
            )
            .style(Style::default().fg(Color::White))
            .wrap(Wrap { trim: false });
        paragraph.render(area, frame.buffer_mut());
    }
}

#[derive(Debug, Default)]
struct TickOutcome {
    history_lines: Vec<Line<'static>>,
}

// Fake agent backend. It demonstrates Codex's important streaming shape:
// newline-gated commits into scrollback plus a small live tail for partial text.
#[derive(Debug)]
struct FakeStream {
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

impl FakeStream {
    fn new(prompt: String) -> Self {
        let response = fake_response(&prompt);
        let now = Instant::now();
        let mut pending_lines = VecDeque::new();
        for line in user_prompt_lines(&prompt) {
            pending_lines.push_back(line);
        }
        pending_lines.push_back(Line::from(""));

        Self {
            chars: response.chars().collect(),
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
            for _ in 0..STREAM_CHARS_PER_TICK {
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

// Minimal textarea used to keep the example dependency-free. A real app can
// replace this with its own composer or a dedicated textarea widget.
#[derive(Debug)]
struct PromptTextArea {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
}

impl Default for PromptTextArea {
    fn default() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
        }
    }
}

impl PromptTextArea {
    fn handle_key(&mut self, key: KeyEvent) -> TextAreaAction {
        match key.code {
            KeyCode::Esc => return TextAreaAction::Quit,
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert_newline()
            }
            KeyCode::Enter => {
                let prompt = self.text();
                self.clear();
                return TextAreaAction::Submit(prompt);
            }
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.move_left(),
            KeyCode::Right => self.move_right(),
            KeyCode::Up => self.move_up(),
            KeyCode::Down => self.move_down(),
            KeyCode::Home => self.cursor_col = 0,
            KeyCode::End => self.cursor_col = self.current_line_len(),
            KeyCode::Tab => self.insert_str("    "),
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::SUPER) =>
            {
                self.insert_char(ch);
            }
            _ => {}
        }

        TextAreaAction::None
    }

    fn render(&self, frame: &mut Frame, area: Rect, flash: Option<&str>) {
        if area.is_empty() {
            return;
        }

        let title = flash.unwrap_or(" prompt: Enter submit, Ctrl-J newline, Esc quit ");
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(title);
        let inner = block.inner(area);
        block.render(area, frame.buffer_mut());

        if inner.is_empty() {
            return;
        }

        let start = self.visible_start(inner.height as usize);
        let visible_lines = self
            .lines
            .iter()
            .skip(start)
            .take(inner.height as usize)
            .map(|line| Line::from(line.as_str()))
            .collect::<Vec<_>>();
        Paragraph::new(visible_lines).render(inner, frame.buffer_mut());

        if self.cursor_row >= start {
            let visible_row = self.cursor_row - start;
            if visible_row < inner.height as usize {
                let line = &self.lines[self.cursor_row];
                let before_cursor = &line[..byte_index_for_char(line, self.cursor_col)];
                let cursor_x = inner
                    .x
                    .saturating_add(before_cursor.width() as u16)
                    .min(inner.right().saturating_sub(1));
                let cursor_y = inner.y.saturating_add(visible_row as u16);
                frame.set_cursor_position(Position {
                    x: cursor_x,
                    y: cursor_y,
                });
            }
        }
    }

    fn desired_height(&self, width: u16) -> u16 {
        let inner_width = width.saturating_sub(2).max(1) as usize;
        let content_rows = self
            .lines
            .iter()
            .map(|line| line.width().max(1).div_ceil(inner_width))
            .sum::<usize>()
            .max(1);
        (content_rows as u16).saturating_add(2).clamp(3, 8)
    }

    fn text(&self) -> String {
        self.lines.join("\n")
    }

    fn replace_text(&mut self, text: String) {
        self.lines = text.split('\n').map(ToString::to_string).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.cursor_row = self.lines.len() - 1;
        self.cursor_col = self.current_line_len();
    }

    fn clear(&mut self) {
        self.lines.clear();
        self.lines.push(String::new());
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    fn insert_char(&mut self, ch: char) {
        let cursor = self.cursor_col;
        let line = self.current_line_mut();
        let idx = byte_index_for_char(line, cursor);
        line.insert(idx, ch);
        self.cursor_col += 1;
    }

    fn insert_str(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' {
                self.insert_newline();
            } else if ch != '\r' {
                self.insert_char(ch);
            }
        }
    }

    fn insert_newline(&mut self) {
        let cursor = self.cursor_col;
        let row = self.cursor_row;
        let line = self.current_line_mut();
        let idx = byte_index_for_char(line, cursor);
        let rest = line.split_off(idx);
        self.lines.insert(row + 1, rest);
        self.cursor_row += 1;
        self.cursor_col = 0;
    }

    fn backspace(&mut self) {
        if self.cursor_col > 0 {
            let new_col = self.cursor_col - 1;
            let line = self.current_line_mut();
            let start = byte_index_for_char(line, new_col);
            let end = byte_index_for_char(line, new_col + 1);
            line.replace_range(start..end, "");
            self.cursor_col = new_col;
        } else if self.cursor_row > 0 {
            let current = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            self.cursor_col = self.current_line_len();
            self.lines[self.cursor_row].push_str(&current);
        }
    }

    fn delete(&mut self) {
        if self.cursor_col < self.current_line_len() {
            let cursor = self.cursor_col;
            let line = self.current_line_mut();
            let start = byte_index_for_char(line, cursor);
            let end = byte_index_for_char(line, cursor + 1);
            line.replace_range(start..end, "");
        } else if self.cursor_row + 1 < self.lines.len() {
            let next = self.lines.remove(self.cursor_row + 1);
            self.lines[self.cursor_row].push_str(&next);
        }
    }

    fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.current_line_len();
        }
    }

    fn move_right(&mut self) {
        if self.cursor_col < self.current_line_len() {
            self.cursor_col += 1;
        } else if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = 0;
        }
    }

    fn move_up(&mut self) {
        if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.cursor_col.min(self.current_line_len());
        }
    }

    fn move_down(&mut self) {
        if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = self.cursor_col.min(self.current_line_len());
        }
    }

    fn visible_start(&self, visible_height: usize) -> usize {
        if visible_height == 0 {
            return self.cursor_row;
        }
        self.cursor_row.saturating_sub(visible_height - 1)
    }

    fn current_line_len(&self) -> usize {
        self.lines[self.cursor_row].chars().count()
    }

    fn current_line_mut(&mut self) -> &mut String {
        &mut self.lines[self.cursor_row]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TextAreaAction {
    Submit(String),
    Quit,
    None,
}

fn byte_index_for_char(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len())
}

fn fake_response(prompt: &str) -> String {
    format!(
        "I'll work on this prompt:\n\n  {prompt}\n\nPlan:\n  1. Keep the TUI inline, not in the alternate screen.\n  2. Commit completed stream lines into native scrollback as they arrive.\n  3. Keep only the current partial line live in the viewport.\n\nResult:\nThis is simulated streaming output. The live tail updates in place without growing the viewport on every token. Completed lines are inserted into backscroll using the same shape as Codex: newline-gated commits, paced by a commit tick."
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
