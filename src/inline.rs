use std::io;
use std::io::Write;

use crossterm::queue;
use crossterm::terminal::BeginSynchronizedUpdate;
use crossterm::terminal::EndSynchronizedUpdate;
use ratatui::backend::Backend;
use ratatui::layout::Offset;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::text::Line;

use crate::history::InsertHistoryMode;
use crate::history::insert_history_lines_with_mode;
use crate::terminal::Frame;
use crate::terminal::Terminal;

#[derive(Debug, Clone)]
pub struct InlineViewport<B>
where
    B: Backend<Error = io::Error> + Write,
{
    terminal: Terminal<B>,
    pending_history_lines: Vec<Line<'static>>,
    history_mode: InsertHistoryMode,
}

impl<B> InlineViewport<B>
where
    B: Backend<Error = io::Error> + Write,
{
    pub fn new(terminal: Terminal<B>) -> Self {
        Self {
            terminal,
            pending_history_lines: Vec::new(),
            history_mode: InsertHistoryMode::Standard,
        }
    }

    pub fn with_history_mode(mut self, mode: InsertHistoryMode) -> Self {
        self.history_mode = mode;
        self
    }

    pub fn terminal(&self) -> &Terminal<B> {
        &self.terminal
    }

    pub fn terminal_mut(&mut self) -> &mut Terminal<B> {
        &mut self.terminal
    }

    pub fn into_terminal(self) -> Terminal<B> {
        self.terminal
    }

    pub fn history_mode(&self) -> InsertHistoryMode {
        self.history_mode
    }

    pub fn set_history_mode(&mut self, mode: InsertHistoryMode) {
        self.history_mode = mode;
    }

    pub fn insert_history_lines<I>(&mut self, lines: I)
    where
        I: IntoIterator<Item = Line<'static>>,
    {
        self.pending_history_lines.extend(lines);
    }

    pub fn clear_pending_history_lines(&mut self) {
        self.pending_history_lines.clear();
    }

    pub fn draw<F>(&mut self, height: u16, draw_fn: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame),
    {
        self.try_draw(height, |frame| {
            draw_fn(frame);
            io::Result::Ok(())
        })
    }

    pub fn try_draw<F, E>(&mut self, height: u16, draw_fn: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame) -> Result<(), E>,
        E: Into<io::Error>,
    {
        let mut pending_viewport_area = self.pending_viewport_area()?;

        self.with_synchronized_update(|this| {
            if let Some(new_area) = pending_viewport_area.take() {
                this.terminal.set_viewport_area(new_area);
                this.terminal.clear()?;
            }

            let mut needs_full_repaint =
                Self::update_inline_viewport(&mut this.terminal, height, this.history_mode)?;
            needs_full_repaint |= Self::flush_pending_history_lines(
                &mut this.terminal,
                &mut this.pending_history_lines,
                this.history_mode,
            )?;

            if needs_full_repaint {
                this.terminal.invalidate_viewport();
            }

            this.terminal.try_draw(draw_fn)
        })
    }

    fn with_synchronized_update<T>(
        &mut self,
        operation: impl FnOnce(&mut Self) -> io::Result<T>,
    ) -> io::Result<T> {
        queue!(self.terminal.backend_mut(), BeginSynchronizedUpdate)?;
        let result = operation(self);
        let end_result = (|| -> io::Result<()> {
            queue!(self.terminal.backend_mut(), EndSynchronizedUpdate)?;
            Write::flush(self.terminal.backend_mut())?;
            Ok(())
        })();

        match (result, end_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(err), _) => Err(err),
            (Ok(_), Err(err)) => Err(err),
        }
    }

    fn update_inline_viewport(
        terminal: &mut Terminal<B>,
        height: u16,
        mode: InsertHistoryMode,
    ) -> io::Result<bool> {
        let size = terminal.size()?;
        let mut needs_full_repaint = false;

        let mut area = terminal.viewport_area;
        area.height = height.min(size.height);
        area.width = size.width;

        if area.y > size.height {
            area.y = size.height.saturating_sub(area.height);
        }

        if area.bottom() > size.height {
            let scroll_by = area.bottom() - size.height;
            if scroll_by > 0 && area.top() > 0 {
                if matches!(mode, InsertHistoryMode::Zellij) {
                    Self::scroll_zellij_expanded_viewport(terminal, size, scroll_by)?;
                    needs_full_repaint = true;
                } else {
                    terminal
                        .backend_mut()
                        .scroll_region_up(0..area.top(), scroll_by)?;
                }
            }
            area.y = size.height.saturating_sub(area.height);
        }

        if area != terminal.viewport_area {
            terminal.clear()?;
            terminal.set_viewport_area(area);
        }

        Ok(needs_full_repaint)
    }

    fn scroll_zellij_expanded_viewport(
        terminal: &mut Terminal<B>,
        size: Size,
        scroll_by: u16,
    ) -> io::Result<()> {
        queue!(
            terminal.backend_mut(),
            crossterm::cursor::MoveTo(0, size.height.saturating_sub(1))
        )?;
        for _ in 0..scroll_by {
            queue!(terminal.backend_mut(), crossterm::style::Print("\n"))?;
        }
        Ok(())
    }

    fn flush_pending_history_lines(
        terminal: &mut Terminal<B>,
        pending_history_lines: &mut Vec<Line<'static>>,
        mode: InsertHistoryMode,
    ) -> io::Result<bool> {
        if pending_history_lines.is_empty() {
            return Ok(false);
        }

        let lines = pending_history_lines.clone();
        match insert_history_lines_with_mode(terminal, lines, mode) {
            Ok(()) => {
                pending_history_lines.clear();
                Ok(matches!(mode, InsertHistoryMode::Zellij))
            }
            Err(err) => Err(err),
        }
    }

    fn pending_viewport_area(&mut self) -> io::Result<Option<Rect>> {
        let terminal = &mut self.terminal;
        let screen_size = terminal.size()?;
        let last_known_screen_size = terminal.last_known_screen_size;
        if screen_size != last_known_screen_size
            && let Ok(cursor_pos) = terminal.get_cursor_position()
        {
            let last_known_cursor_pos = terminal.last_known_cursor_pos;
            if cursor_pos.y != last_known_cursor_pos.y {
                let offset = Offset {
                    x: 0,
                    y: cursor_pos.y as i32 - last_known_cursor_pos.y as i32,
                };
                return Ok(Some(terminal.viewport_area.offset(offset)));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_backend::VT100Backend;
    use ratatui::layout::Rect;
    use ratatui::style::Style;
    use ratatui::text::Line;

    #[test]
    fn draw_creates_inline_viewport_without_alt_screen() {
        let width: u16 = 20;
        let height: u16 = 8;
        let backend = VT100Backend::new(width, height);
        let terminal = Terminal::with_options(backend).unwrap();
        let mut viewport = InlineViewport::new(terminal);

        viewport
            .draw(3, |frame| {
                let area = frame.area();
                frame
                    .buffer_mut()
                    .set_string(area.x, area.y, "hello", Style::default());
            })
            .unwrap();

        assert_eq!(viewport.terminal().viewport_area, Rect::new(0, 0, width, 3));
        let rows: Vec<String> = viewport
            .terminal()
            .backend()
            .vt100()
            .screen()
            .rows(0, width)
            .collect();
        assert!(rows.iter().any(|row| row.contains("hello")));
    }

    #[test]
    fn pending_history_is_flushed_before_draw() {
        let width: u16 = 30;
        let height: u16 = 8;
        let backend = VT100Backend::new(width, height);
        let mut terminal = Terminal::with_options(backend).unwrap();
        terminal.set_viewport_area(Rect::new(0, 4, width, 2));
        let mut viewport = InlineViewport::new(terminal);
        viewport.insert_history_lines([Line::from("history before draw")]);

        viewport
            .draw(2, |frame| {
                let area = frame.area();
                frame
                    .buffer_mut()
                    .set_string(area.x, area.y, "live", Style::default());
            })
            .unwrap();

        let rows: Vec<String> = viewport
            .terminal()
            .backend()
            .vt100()
            .screen()
            .rows(0, width)
            .collect();
        assert!(rows.iter().any(|row| row.contains("history before draw")));
        assert!(rows.iter().any(|row| row.contains("live")));
        assert_eq!(viewport.terminal().visible_history_rows(), 1);
    }
}
