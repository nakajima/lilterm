use std::fmt;
use std::io;
use std::io::Write;

use crossterm::Command;
use ratatui::backend::Backend;
use ratatui::buffer::Buffer;
use ratatui::style::Style;
use ratatui::text::Line;
use unicode_width::UnicodeWidthChar;

use crate::terminal::Terminal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertHistoryMode {
    Standard,
    Zellij,
}

impl InsertHistoryMode {
    pub fn new(is_zellij: bool) -> Self {
        if is_zellij {
            Self::Zellij
        } else {
            Self::Standard
        }
    }
}

pub fn insert_history_lines<'a, B>(
    terminal: &mut Terminal<B>,
    lines: Vec<Line<'a>>,
) -> io::Result<()>
where
    B: Backend<Error = io::Error> + Write,
{
    insert_history_lines_with_mode(terminal, lines, InsertHistoryMode::Standard)
}

pub fn insert_history_lines_with_mode<'a, B>(
    terminal: &mut Terminal<B>,
    lines: Vec<Line<'a>>,
    mode: InsertHistoryMode,
) -> io::Result<()>
where
    B: Backend<Error = io::Error> + Write,
{
    if lines.is_empty() {
        return Ok(());
    }

    let width = terminal.viewport_area.width.max(1) as usize;
    let inserted_rows = physical_rows(&lines, width);
    if inserted_rows == 0 {
        return Ok(());
    }

    let last_cursor_pos = terminal.last_known_cursor_pos;
    let draw_lines = |buffer: &mut Buffer| render_lines_to_buffer(buffer, &lines, width);

    match mode {
        InsertHistoryMode::Standard => terminal.insert_before(inserted_rows, draw_lines)?,
        InsertHistoryMode::Zellij => {
            terminal.insert_before_without_scrolling_regions(inserted_rows, draw_lines)?;
            terminal.invalidate_viewport();
        }
    }

    terminal.set_cursor_position(last_cursor_pos)?;
    terminal.note_history_rows_inserted(inserted_rows);

    Ok(())
}

fn physical_rows(lines: &[Line<'_>], wrap_width: usize) -> u16 {
    let wrap_width = wrap_width.max(1);
    let rows = lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(wrap_width))
        .sum::<usize>();
    rows.min(u16::MAX as usize) as u16
}

fn render_lines_to_buffer(buffer: &mut Buffer, lines: &[Line<'_>], wrap_width: usize) {
    let wrap_width = wrap_width.max(1);
    let mut row = buffer.area.y;

    for line in lines {
        let line_rows = line.width().max(1).div_ceil(wrap_width).max(1) as u16;
        let end_row = row.saturating_add(line_rows);
        let mut x = buffer.area.x;
        let mut y = row;

        for span in &line.spans {
            let style = line.style.patch(span.style);
            for ch in span.content.chars() {
                if ch == '\n' || ch == '\r' {
                    y = y.saturating_add(1);
                    x = buffer.area.x;
                    if y >= end_row {
                        break;
                    }
                    continue;
                }

                let width = ch.width().unwrap_or(0);
                if width == 0 {
                    continue;
                }

                if x.saturating_sub(buffer.area.x) as usize + width > wrap_width {
                    y = y.saturating_add(1);
                    x = buffer.area.x;
                    if y >= end_row {
                        break;
                    }
                }

                if y < buffer.area.bottom() && x < buffer.area.right() {
                    buffer.set_string(x, y, ch.to_string(), style);
                }
                x = x.saturating_add(width as u16);
            }
        }

        if line.spans.is_empty() {
            let style = line.style.patch(Style::default());
            if row < buffer.area.bottom() {
                buffer.set_string(buffer.area.x, row, "", style);
            }
        }

        row = end_row;
        if row >= buffer.area.bottom() {
            break;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetScrollRegion(pub std::ops::Range<u16>);

impl Command for SetScrollRegion {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[{};{}r", self.0.start, self.0.end)
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "SetScrollRegion requires ANSI command execution",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResetScrollRegion;

impl Command for ResetScrollRegion {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[r")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "ResetScrollRegion requires ANSI command execution",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_backend::VT100Backend;
    use pretty_assertions::assert_eq;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    #[test]
    fn vt100_standard_mode_inserts_history_and_updates_viewport() {
        let width: u16 = 32;
        let height: u16 = 8;
        let backend = VT100Backend::new(width, height);
        let mut term = Terminal::with_options(backend).unwrap();
        term.set_viewport_area(Rect::new(0, 4, width, 2));

        insert_history_lines(&mut term, vec![Line::from("history row")]).unwrap();

        let rows: Vec<String> = term.backend().vt100().screen().rows(0, width).collect();
        assert!(rows.iter().any(|row| row.contains("history row")));
        assert_eq!(term.viewport_area, Rect::new(0, 5, width, 2));
        assert_eq!(term.visible_history_rows(), 1);
    }

    #[test]
    fn vt100_zellij_mode_inserts_history_and_updates_viewport() {
        let width: u16 = 32;
        let height: u16 = 8;
        let backend = VT100Backend::new(width, height);
        let mut term = Terminal::with_options(backend).unwrap();
        term.set_viewport_area(Rect::new(0, 4, width, 2));

        insert_history_lines_with_mode(
            &mut term,
            vec![Line::from("zellij history")],
            InsertHistoryMode::Zellij,
        )
        .unwrap();

        let rows: Vec<String> = term.backend().vt100().screen().rows(0, width).collect();
        assert!(rows.iter().any(|row| row.contains("zellij history")));
        assert_eq!(term.viewport_area, Rect::new(0, 5, width, 2));
        assert_eq!(term.visible_history_rows(), 1);
    }

    #[test]
    fn long_line_counts_physical_rows() {
        let width: u16 = 10;
        let height: u16 = 8;
        let backend = VT100Backend::new(width, height);
        let mut term = Terminal::with_options(backend).unwrap();
        term.set_viewport_area(Rect::new(0, 5, width, 1));

        insert_history_lines(&mut term, vec![Line::from("abcdefghijklmnop")]).unwrap();

        assert_eq!(term.visible_history_rows(), 2);
    }

    #[test]
    fn full_height_viewport_inserts_history_into_scrollback() {
        let width: u16 = 24;
        let height: u16 = 4;
        let backend = VT100Backend::new(width, height);
        let mut term = Terminal::with_options(backend).unwrap();
        term.set_viewport_area(Rect::new(0, 0, width, height));

        term.draw(|frame| {
            let area = frame.area();
            for y in 0..area.height {
                frame.buffer_mut().set_string(
                    area.x,
                    area.y + y,
                    format!("live row {y}"),
                    Style::default(),
                );
            }
        })
        .unwrap();

        insert_history_lines(&mut term, vec![Line::from("backscroll row")]).unwrap();

        let visible_rows: Vec<String> = term.backend().vt100().screen().rows(0, width).collect();
        assert!(visible_rows.iter().any(|row| row.contains("live row 0")));
        assert!(
            !visible_rows
                .iter()
                .any(|row| row.contains("backscroll row"))
        );

        term.backend_mut().vt100_mut().set_scrollback(1);
        let scrolled_rows: Vec<String> = term.backend().vt100().screen().rows(0, width).collect();
        assert!(
            scrolled_rows
                .iter()
                .any(|row| row.contains("backscroll row")),
            "expected inserted row in scrollback, rows: {scrolled_rows:?}"
        );
    }
}
