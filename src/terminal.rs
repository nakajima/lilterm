// This is derived from ratatui::Terminal, which is licensed under the following terms:
//
// The MIT License (MIT)
// Copyright (c) 2016-2022 Florian Dehau
// Copyright (c) 2023-2025 The Ratatui Developers
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

use std::fmt;
use std::io;
use std::io::Write;

use crossterm::cursor::Hide;
use crossterm::cursor::MoveTo;
use crossterm::cursor::Show;
use crossterm::queue;
use crossterm::style::Colors;
use crossterm::style::Print;
use crossterm::style::SetAttribute;
use crossterm::style::SetBackgroundColor;
use crossterm::style::SetColors;
use crossterm::style::SetForegroundColor;
use crossterm::terminal::Clear;
use ratatui::backend::Backend;
use ratatui::buffer::Buffer;
use ratatui::buffer::Cell;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::widgets::WidgetRef;
use unicode_width::UnicodeWidthStr;

fn color_to_crossterm(color: Color) -> crossterm::style::Color {
    match color {
        Color::Reset => crossterm::style::Color::Reset,
        Color::Black => crossterm::style::Color::Black,
        Color::Red => crossterm::style::Color::DarkRed,
        Color::Green => crossterm::style::Color::DarkGreen,
        Color::Yellow => crossterm::style::Color::DarkYellow,
        Color::Blue => crossterm::style::Color::DarkBlue,
        Color::Magenta => crossterm::style::Color::DarkMagenta,
        Color::Cyan => crossterm::style::Color::DarkCyan,
        Color::Gray => crossterm::style::Color::Grey,
        Color::DarkGray => crossterm::style::Color::DarkGrey,
        Color::LightRed => crossterm::style::Color::Red,
        Color::LightGreen => crossterm::style::Color::Green,
        Color::LightYellow => crossterm::style::Color::Yellow,
        Color::LightBlue => crossterm::style::Color::Blue,
        Color::LightMagenta => crossterm::style::Color::Magenta,
        Color::LightCyan => crossterm::style::Color::Cyan,
        Color::White => crossterm::style::Color::White,
        Color::Rgb(r, g, b) => crossterm::style::Color::Rgb { r, g, b },
        Color::Indexed(index) => crossterm::style::Color::AnsiValue(index),
    }
}

fn display_width(s: &str) -> usize {
    if !s.contains('\x1b') {
        return s.width();
    }

    let mut visible = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.clone().next() == Some(']') {
            chars.next();
            for c in chars.by_ref() {
                if c == '\x07' {
                    break;
                }
            }
            continue;
        }
        visible.push(ch);
    }
    visible.width()
}

#[derive(Debug, Hash)]
pub struct Frame<'a> {
    cursor_position: Option<Position>,
    viewport_area: Rect,
    buffer: &'a mut Buffer,
}

impl Frame<'_> {
    pub const fn area(&self) -> Rect {
        self.viewport_area
    }

    #[allow(clippy::needless_pass_by_value)]
    pub fn render_widget_ref<W: WidgetRef>(&mut self, widget: W, area: Rect) {
        widget.render_ref(area, self.buffer);
    }

    pub fn set_cursor_position<P: Into<Position>>(&mut self, position: P) {
        self.cursor_position = Some(position.into());
    }

    pub fn buffer_mut(&mut self) -> &mut Buffer {
        self.buffer
    }
}

#[derive(Debug, Default, Clone, Eq, PartialEq, Hash)]
pub struct Terminal<B>
where
    B: Backend<Error = io::Error> + Write,
{
    backend: B,
    buffers: [Buffer; 2],
    current: usize,
    pub hidden_cursor: bool,
    pub viewport_area: Rect,
    pub last_known_screen_size: Size,
    pub last_known_cursor_pos: Position,
    visible_history_rows: u16,
}

impl<B> Drop for Terminal<B>
where
    B: Backend<Error = io::Error> + Write,
{
    fn drop(&mut self) {
        if self.hidden_cursor {
            let _ = self.show_cursor();
        }
    }
}

impl<B> Terminal<B>
where
    B: Backend<Error = io::Error> + Write,
{
    pub fn with_options(mut backend: B) -> io::Result<Self> {
        let screen_size = backend.size()?;
        let cursor_pos = backend
            .get_cursor_position()
            .unwrap_or(Position { x: 0, y: 0 });
        Ok(Self {
            backend,
            buffers: [Buffer::empty(Rect::ZERO), Buffer::empty(Rect::ZERO)],
            current: 0,
            hidden_cursor: false,
            viewport_area: Rect::new(0, cursor_pos.y, 0, 0),
            last_known_screen_size: screen_size,
            last_known_cursor_pos: cursor_pos,
            visible_history_rows: 0,
        })
    }

    pub fn get_frame(&mut self) -> Frame<'_> {
        Frame {
            cursor_position: None,
            viewport_area: self.viewport_area,
            buffer: self.current_buffer_mut(),
        }
    }

    fn current_buffer(&self) -> &Buffer {
        &self.buffers[self.current]
    }

    fn current_buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffers[self.current]
    }

    fn previous_buffer(&self) -> &Buffer {
        &self.buffers[1 - self.current]
    }

    fn previous_buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffers[1 - self.current]
    }

    pub const fn backend(&self) -> &B {
        &self.backend
    }

    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    pub fn flush(&mut self) -> io::Result<bool> {
        let updates = diff_buffers(self.previous_buffer(), self.current_buffer());
        if updates.is_empty() {
            return Ok(false);
        }
        if let Some(DrawCommand::Put { x, y, .. }) =
            updates.iter().rfind(|command| command.is_put())
        {
            self.last_known_cursor_pos = Position { x: *x, y: *y };
        }
        draw(&mut self.backend, updates.into_iter())?;
        Ok(true)
    }

    pub fn resize(&mut self, screen_size: Size) -> io::Result<()> {
        crate::trace::log_args(format_args!(
            "terminal.resize old_screen={:?} new_screen={:?} viewport={:?}",
            self.last_known_screen_size, screen_size, self.viewport_area
        ));
        self.last_known_screen_size = screen_size;
        Ok(())
    }

    pub fn set_viewport_area(&mut self, area: Rect) {
        let old_area = self.viewport_area;
        self.current_buffer_mut().resize(area);
        self.previous_buffer_mut().resize(area);
        self.viewport_area = area;
        self.visible_history_rows = self.visible_history_rows.min(area.top());
        crate::trace::log_args(format_args!(
            "terminal.set_viewport_area old={old_area:?} new={:?} visible_history_rows={}",
            self.viewport_area, self.visible_history_rows
        ));
    }

    pub(crate) fn set_viewport_area_preserving_overlap(&mut self, area: Rect) {
        let old_area = self.viewport_area;
        remap_buffer_to_area(self.current_buffer_mut(), area);
        remap_buffer_to_area(self.previous_buffer_mut(), area);
        self.viewport_area = area;
        self.visible_history_rows = self.visible_history_rows.min(area.top());
        crate::trace::log_args(format_args!(
            "terminal.set_viewport_area_preserving_overlap old={old_area:?} new={:?} visible_history_rows={}",
            self.viewport_area, self.visible_history_rows
        ));
    }

    pub fn autoresize(&mut self) -> io::Result<()> {
        let screen_size = self.size()?;
        if screen_size != self.last_known_screen_size {
            self.resize(screen_size)?;
        }
        Ok(())
    }

    pub fn draw<F>(&mut self, render_callback: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame),
    {
        self.try_draw(|frame| {
            render_callback(frame);
            io::Result::Ok(())
        })
    }

    pub fn try_draw<F, E>(&mut self, render_callback: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame) -> Result<(), E>,
        E: Into<io::Error>,
    {
        self.autoresize()?;

        let mut frame = self.get_frame();
        render_callback(&mut frame).map_err(Into::into)?;
        let cursor_position = frame.cursor_position;

        let wrote_updates = self.flush()?;

        let mut wrote_cursor = false;
        match cursor_position {
            None => {
                if !self.hidden_cursor {
                    self.hide_cursor()?;
                    wrote_cursor = true;
                }
            }
            Some(position) => {
                if self.hidden_cursor {
                    self.show_cursor()?;
                    wrote_cursor = true;
                }
                if position != self.last_known_cursor_pos {
                    self.set_cursor_position(position)?;
                    wrote_cursor = true;
                }
            }
        }

        self.swap_buffers();
        if wrote_updates || wrote_cursor {
            Backend::flush(&mut self.backend)?;
        }

        Ok(())
    }

    pub fn hide_cursor(&mut self) -> io::Result<()> {
        queue!(self.backend, Hide)?;
        self.hidden_cursor = true;
        Ok(())
    }

    pub fn show_cursor(&mut self) -> io::Result<()> {
        queue!(self.backend, Show)?;
        self.hidden_cursor = false;
        Ok(())
    }

    pub fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.backend.get_cursor_position()
    }

    pub fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let position = position.into();
        queue!(self.backend, MoveTo(position.x, position.y))?;
        self.last_known_cursor_pos = position;
        Ok(())
    }

    pub fn clear(&mut self) -> io::Result<()> {
        if self.viewport_area.is_empty() {
            return Ok(());
        }
        crate::trace::log_args(format_args!(
            "terminal.clear viewport={:?}",
            self.viewport_area
        ));
        self.set_cursor_position(self.viewport_area.as_position())?;
        queue!(
            self.backend,
            Clear(crossterm::terminal::ClearType::FromCursorDown)
        )?;
        self.previous_buffer_mut().reset();
        Ok(())
    }

    pub fn invalidate_viewport(&mut self) {
        crate::trace::log_changed_args(
            "terminal.invalidate_viewport",
            format_args!("viewport={:?}", self.viewport_area),
        );
        mark_buffer_invalid(self.previous_buffer_mut());
    }

    pub fn visible_history_rows(&self) -> u16 {
        self.visible_history_rows
    }

    pub(crate) fn note_history_rows_inserted(&mut self, inserted_rows: u16) {
        let old_visible_history_rows = self.visible_history_rows;
        self.visible_history_rows = self
            .visible_history_rows
            .saturating_add(inserted_rows)
            .min(self.viewport_area.top());
        crate::trace::log_args(format_args!(
            "terminal.note_history_rows_inserted inserted={inserted_rows} old_visible={old_visible_history_rows} new_visible={} viewport={:?}",
            self.visible_history_rows, self.viewport_area
        ));
    }

    pub fn swap_buffers(&mut self) {
        self.previous_buffer_mut().reset();
        self.current = 1 - self.current;
    }

    pub fn size(&self) -> io::Result<Size> {
        self.backend.size()
    }

    pub fn leave_viewport(&mut self) -> io::Result<()> {
        if self.viewport_area.is_empty() {
            return Ok(());
        }

        let size = self.size()?;
        crate::trace::log_args(format_args!(
            "terminal.leave_viewport viewport={:?} screen={size:?}",
            self.viewport_area
        ));
        if size.height == 0 {
            return Ok(());
        }

        let bottom = self.viewport_area.bottom().min(size.height);
        if bottom < size.height {
            self.set_cursor_position(Position { x: 0, y: bottom })?;
        } else {
            queue!(
                self.backend,
                MoveTo(0, size.height.saturating_sub(1)),
                Print("\n")
            )?;
            self.last_known_cursor_pos = Position {
                x: 0,
                y: size.height.saturating_sub(1),
            };
        }

        Write::flush(&mut self.backend)?;
        Ok(())
    }

    pub(crate) fn scroll_region_up_queued(
        &mut self,
        region: std::ops::Range<u16>,
        amount: u16,
    ) -> io::Result<()> {
        if amount == 0 || region.start >= region.end {
            return Ok(());
        }
        crate::trace::log_args(format_args!(
            "terminal.scroll_region_up region={region:?} amount={amount} viewport={:?}",
            self.viewport_area
        ));
        queue!(
            self.backend,
            ScrollUpInRegion {
                first_row: region.start,
                last_row: region.end.saturating_sub(1),
                lines_to_scroll: amount,
            }
        )?;
        Ok(())
    }

    pub(crate) fn scroll_region_down_queued(
        &mut self,
        region: std::ops::Range<u16>,
        amount: u16,
    ) -> io::Result<()> {
        if amount == 0 || region.start >= region.end {
            return Ok(());
        }
        crate::trace::log_args(format_args!(
            "terminal.scroll_region_down region={region:?} amount={amount} viewport={:?}",
            self.viewport_area
        ));
        queue!(
            self.backend,
            ScrollDownInRegion {
                first_row: region.start,
                last_row: region.end.saturating_sub(1),
                lines_to_scroll: amount,
            }
        )?;
        Ok(())
    }

    pub fn insert_before<F>(&mut self, height: u16, draw_fn: F) -> io::Result<()>
    where
        F: FnOnce(&mut Buffer),
    {
        crate::trace::log_args(format_args!(
            "terminal.insert_before height={height} viewport={:?} screen={:?}",
            self.viewport_area, self.last_known_screen_size
        ));
        self.insert_before_scrolling_regions(height, draw_fn)
    }

    fn insert_before_scrolling_regions<F>(&mut self, mut height: u16, draw_fn: F) -> io::Result<()>
    where
        F: FnOnce(&mut Buffer),
    {
        if height == 0 || self.viewport_area.width == 0 {
            return Ok(());
        }
        crate::trace::log_args(format_args!(
            "terminal.insert_before_scrolling_regions start height={height} viewport={:?} screen={:?}",
            self.viewport_area, self.last_known_screen_size
        ));

        let area = Rect {
            x: 0,
            y: 0,
            width: self.viewport_area.width,
            height,
        };
        let mut buffer = Buffer::empty(area);
        draw_fn(&mut buffer);
        let mut buffer = buffer.content.as_slice();

        if self.viewport_area.height == self.last_known_screen_size.height {
            crate::trace::log_args(format_args!(
                "terminal.insert_before_scrolling_regions full_height height={height} viewport={:?}",
                self.viewport_area
            ));
            while !buffer.is_empty() {
                buffer = self.draw_lines(0, 1, buffer)?;
                self.scroll_region_up_queued(0..1, 1)?;
            }

            let width = self.viewport_area.width as usize;
            let top_line = self.buffers[1 - self.current].content[0..width].to_vec();
            self.draw_lines(0, 1, &top_line)?;
            return Ok(());
        }

        {
            let viewport_top = self.viewport_area.top();
            let viewport_bottom = self.viewport_area.bottom();
            let screen_bottom = self.last_known_screen_size.height;
            if viewport_bottom < screen_bottom {
                let to_draw = height.min(screen_bottom - viewport_bottom);
                crate::trace::log_args(format_args!(
                    "terminal.insert_before_scrolling_regions shift_down to_draw={to_draw} viewport_top={viewport_top} viewport_bottom={viewport_bottom} screen_bottom={screen_bottom}"
                ));
                self.scroll_region_down_queued(viewport_top..viewport_bottom + to_draw, to_draw)?;
                buffer = self.draw_lines(viewport_top, to_draw, buffer)?;
                self.set_viewport_area(Rect {
                    y: viewport_top + to_draw,
                    ..self.viewport_area
                });
                height -= to_draw;
            }
        }

        let viewport_top = self.viewport_area.top();
        while height > 0 {
            let to_draw = height.min(viewport_top);
            crate::trace::log_args(format_args!(
                "terminal.insert_before_scrolling_regions scroll_above to_draw={to_draw} viewport_top={viewport_top} remaining_height={height}"
            ));
            self.scroll_region_up_queued(0..viewport_top, to_draw)?;
            buffer = self.draw_lines(viewport_top - to_draw, to_draw, buffer)?;
            height -= to_draw;
        }

        Ok(())
    }

    fn draw_lines<'a>(
        &mut self,
        y_offset: u16,
        lines_to_draw: u16,
        cells: &'a [Cell],
    ) -> io::Result<&'a [Cell]> {
        let width: usize = self.viewport_area.width.into();
        let count = width * lines_to_draw as usize;
        let (to_draw, remainder) = cells.split_at(count.min(cells.len()));
        if lines_to_draw > 0 {
            let iter = to_draw
                .iter()
                .enumerate()
                .map(|(i, cell)| ((i % width) as u16, y_offset + (i / width) as u16, cell));
            self.backend.draw(iter)?;
        }
        Ok(remainder)
    }
}

fn mark_buffer_invalid(buffer: &mut Buffer) {
    for cell in &mut buffer.content {
        cell.set_symbol(" ");
        cell.fg = Color::Indexed(255);
        cell.bg = Color::Indexed(254);
        cell.modifier = Modifier::RAPID_BLINK;
    }
}

fn remap_buffer_to_area(buffer: &mut Buffer, area: Rect) {
    if buffer.area == area {
        return;
    }

    let old = std::mem::replace(buffer, Buffer::empty(area));
    let overlap = old.area.intersection(area);
    for y in overlap.top()..overlap.bottom() {
        for x in overlap.left()..overlap.right() {
            let Some(source) = old.cell((x, y)) else {
                continue;
            };
            let Some(target) = buffer.cell_mut((x, y)) else {
                continue;
            };
            *target = source.clone();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScrollUpInRegion {
    first_row: u16,
    last_row: u16,
    lines_to_scroll: u16,
}

impl crossterm::Command for ScrollUpInRegion {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        if self.lines_to_scroll == 0 {
            return Ok(());
        }
        write!(
            f,
            "\x1b[{};{}r\x1b[{}S\x1b[r",
            self.first_row.saturating_add(1),
            self.last_row.saturating_add(1),
            self.lines_to_scroll
        )
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "ScrollUpInRegion requires ANSI command execution",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScrollDownInRegion {
    first_row: u16,
    last_row: u16,
    lines_to_scroll: u16,
}

impl crossterm::Command for ScrollDownInRegion {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        if self.lines_to_scroll == 0 {
            return Ok(());
        }
        write!(
            f,
            "\x1b[{};{}r\x1b[{}T\x1b[r",
            self.first_row.saturating_add(1),
            self.last_row.saturating_add(1),
            self.lines_to_scroll
        )
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "ScrollDownInRegion requires ANSI command execution",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[derive(Debug)]
enum DrawCommand {
    Put { x: u16, y: u16, cell: Cell },
    ClearToEnd { x: u16, y: u16, bg: Color },
}

impl DrawCommand {
    fn is_put(&self) -> bool {
        matches!(self, Self::Put { .. })
    }
}

fn is_invalidated_cell(cell: &Cell) -> bool {
    cell.fg == Color::Indexed(255)
        && cell.bg == Color::Indexed(254)
        && cell.modifier.contains(Modifier::RAPID_BLINK)
}

fn diff_buffers(a: &Buffer, b: &Buffer) -> Vec<DrawCommand> {
    let previous_buffer = &a.content;
    let next_buffer = &b.content;

    let mut updates = vec![];
    let mut last_nonblank_columns = vec![0; a.area.height as usize];
    for y in 0..a.area.height {
        let row_start = y as usize * a.area.width as usize;
        let row_end = row_start + a.area.width as usize;
        let row = &next_buffer[row_start..row_end];
        let bg = row.last().map(|cell| cell.bg).unwrap_or(Color::Reset);

        let mut last_nonblank_column = 0usize;
        let mut column = 0usize;
        while column < row.len() {
            let cell = &row[column];
            let width = display_width(cell.symbol());
            if cell.symbol() != " " || cell.bg != bg || cell.modifier != Modifier::empty() {
                last_nonblank_column = column + width.saturating_sub(1);
            }
            column += width.max(1);
        }

        let clear_start = last_nonblank_column + 1;
        if clear_start < row.len()
            && previous_buffer[row_start + clear_start..row_end]
                .iter()
                .any(|cell| {
                    is_invalidated_cell(cell)
                        || cell.symbol() != " "
                        || cell.bg != bg
                        || cell.modifier != Modifier::empty()
                })
        {
            let (x, y) = a.pos_of(row_start + clear_start);
            updates.push(DrawCommand::ClearToEnd { x, y, bg });
        }

        last_nonblank_columns[y as usize] = last_nonblank_column as u16;
    }

    let mut invalidated: usize = 0;
    let mut to_skip: usize = 0;
    for (i, (current, previous)) in next_buffer.iter().zip(previous_buffer.iter()).enumerate() {
        if !current.skip
            && (is_invalidated_cell(previous) || current != previous || invalidated > 0)
            && to_skip == 0
        {
            let (x, y) = a.pos_of(i);
            let row = i / a.area.width as usize;
            if x <= last_nonblank_columns[row] {
                updates.push(DrawCommand::Put {
                    x,
                    y,
                    cell: next_buffer[i].clone(),
                });
            }
        }

        to_skip = display_width(current.symbol()).saturating_sub(1);

        let affected_width = std::cmp::max(
            display_width(current.symbol()),
            display_width(previous.symbol()),
        );
        invalidated = std::cmp::max(affected_width, invalidated).saturating_sub(1);
    }
    updates
}

fn draw<I>(writer: &mut impl Write, commands: I) -> io::Result<()>
where
    I: Iterator<Item = DrawCommand>,
{
    let mut fg = Color::Reset;
    let mut bg = Color::Reset;
    let mut modifier = Modifier::empty();
    let mut last_pos: Option<Position> = None;
    for command in commands {
        let (x, y) = match command {
            DrawCommand::Put { x, y, .. } => (x, y),
            DrawCommand::ClearToEnd { x, y, .. } => (x, y),
        };
        if !matches!(last_pos, Some(p) if x == p.x + 1 && y == p.y) {
            queue!(writer, MoveTo(x, y))?;
        }
        last_pos = Some(Position { x, y });
        match command {
            DrawCommand::Put { cell, .. } => {
                if cell.modifier != modifier {
                    let diff = ModifierDiff {
                        from: modifier,
                        to: cell.modifier,
                    };
                    diff.queue(writer)?;
                    modifier = cell.modifier;
                }
                if cell.fg != fg || cell.bg != bg {
                    queue!(
                        writer,
                        SetColors(Colors::new(
                            color_to_crossterm(cell.fg),
                            color_to_crossterm(cell.bg)
                        ))
                    )?;
                    fg = cell.fg;
                    bg = cell.bg;
                }

                queue!(writer, Print(cell.symbol()))?;
            }
            DrawCommand::ClearToEnd { bg: clear_bg, .. } => {
                queue!(writer, SetAttribute(crossterm::style::Attribute::Reset))?;
                modifier = Modifier::empty();
                queue!(writer, SetBackgroundColor(color_to_crossterm(clear_bg)))?;
                bg = clear_bg;
                queue!(writer, Clear(crossterm::terminal::ClearType::UntilNewLine))?;
            }
        }
    }

    queue!(
        writer,
        SetForegroundColor(crossterm::style::Color::Reset),
        SetBackgroundColor(crossterm::style::Color::Reset),
        SetAttribute(crossterm::style::Attribute::Reset),
    )?;

    Ok(())
}

struct ModifierDiff {
    from: Modifier,
    to: Modifier,
}

impl ModifierDiff {
    fn queue<W: io::Write>(self, w: &mut W) -> io::Result<()> {
        use crossterm::style::Attribute as CAttribute;
        let removed = self.from - self.to;
        if removed.contains(Modifier::REVERSED) {
            queue!(w, SetAttribute(CAttribute::NoReverse))?;
        }
        if removed.contains(Modifier::BOLD) {
            queue!(w, SetAttribute(CAttribute::NormalIntensity))?;
            if self.to.contains(Modifier::DIM) {
                queue!(w, SetAttribute(CAttribute::Dim))?;
            }
        }
        if removed.contains(Modifier::ITALIC) {
            queue!(w, SetAttribute(CAttribute::NoItalic))?;
        }
        if removed.contains(Modifier::UNDERLINED) {
            queue!(w, SetAttribute(CAttribute::NoUnderline))?;
        }
        if removed.contains(Modifier::DIM) {
            queue!(w, SetAttribute(CAttribute::NormalIntensity))?;
        }
        if removed.contains(Modifier::CROSSED_OUT) {
            queue!(w, SetAttribute(CAttribute::NotCrossedOut))?;
        }
        if removed.contains(Modifier::SLOW_BLINK) || removed.contains(Modifier::RAPID_BLINK) {
            queue!(w, SetAttribute(CAttribute::NoBlink))?;
        }

        let added = self.to - self.from;
        if added.contains(Modifier::REVERSED) {
            queue!(w, SetAttribute(CAttribute::Reverse))?;
        }
        if added.contains(Modifier::BOLD) {
            queue!(w, SetAttribute(CAttribute::Bold))?;
        }
        if added.contains(Modifier::ITALIC) {
            queue!(w, SetAttribute(CAttribute::Italic))?;
        }
        if added.contains(Modifier::UNDERLINED) {
            queue!(w, SetAttribute(CAttribute::Underlined))?;
        }
        if added.contains(Modifier::DIM) {
            queue!(w, SetAttribute(CAttribute::Dim))?;
        }
        if added.contains(Modifier::CROSSED_OUT) {
            queue!(w, SetAttribute(CAttribute::CrossedOut))?;
        }
        if added.contains(Modifier::SLOW_BLINK) {
            queue!(w, SetAttribute(CAttribute::SlowBlink))?;
        }
        if added.contains(Modifier::RAPID_BLINK) {
            queue!(w, SetAttribute(CAttribute::RapidBlink))?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    #[test]
    fn diff_buffers_does_not_emit_clear_to_end_for_full_width_row() {
        let area = Rect::new(0, 0, 3, 2);
        let previous = Buffer::empty(area);
        let mut next = Buffer::empty(area);

        next.cell_mut((2, 0)).unwrap().set_symbol("X");

        let commands = diff_buffers(&previous, &next);

        let clear_count = commands
            .iter()
            .filter(|command| matches!(command, DrawCommand::ClearToEnd { y, .. } if *y == 0))
            .count();
        assert_eq!(0, clear_count);
        assert!(
            commands
                .iter()
                .any(|command| matches!(command, DrawCommand::Put { x: 2, y: 0, .. }))
        );
    }

    #[test]
    fn diff_buffers_does_not_clear_identical_trailing_blank_cells() {
        let area = Rect::new(0, 0, 10, 1);
        let mut previous = Buffer::empty(area);
        previous.set_string(0, 0, "hello", Style::default());
        let next = previous.clone();

        let commands = diff_buffers(&previous, &next);

        assert!(commands.is_empty());
    }

    #[test]
    fn diff_buffers_clears_stale_trailing_cells() {
        let area = Rect::new(0, 0, 10, 1);
        let mut previous = Buffer::empty(area);
        let mut next = Buffer::empty(area);
        previous.set_string(0, 0, "hello", Style::default());
        next.set_string(0, 0, "hi", Style::default());

        let commands = diff_buffers(&previous, &next);

        assert!(
            commands
                .iter()
                .any(|command| matches!(command, DrawCommand::ClearToEnd { x: 2, y: 0, .. }))
        );
    }

    #[test]
    fn leave_viewport_moves_cursor_below_viewport_without_clearing_when_room() {
        let width: u16 = 12;
        let height: u16 = 6;
        let backend = crate::test_backend::VT100Backend::new(width, height);
        let mut terminal = Terminal::with_options(backend).unwrap();
        terminal.set_viewport_area(Rect::new(0, 2, width, 2));

        terminal
            .draw(|frame| {
                frame
                    .buffer_mut()
                    .set_string(0, 2, "live row", Style::default());
            })
            .unwrap();
        terminal.leave_viewport().unwrap();

        let rows: Vec<String> = terminal.backend().vt100().screen().rows(0, width).collect();
        assert!(rows.iter().any(|row| row.contains("live row")));
        assert_eq!(
            terminal.backend().vt100().screen().cursor_position(),
            (4, 0)
        );
    }

    #[test]
    fn invalidating_viewport_forces_blank_cells_to_be_redrawn_inside_bordered_rows() {
        let area = Rect::new(0, 0, 8, 1);
        let mut previous = Buffer::empty(area);
        let mut next = Buffer::empty(area);
        next.set_string(0, 0, "|a b  |", Style::default());

        mark_buffer_invalid(&mut previous);
        let commands = diff_buffers(&previous, &next);

        assert!(
            commands
                .iter()
                .any(|command| matches!(command, DrawCommand::Put { x: 2, y: 0, cell } if cell.symbol() == " "))
        );
        assert!(
            commands
                .iter()
                .any(|command| matches!(command, DrawCommand::Put { x: 5, y: 0, cell } if cell.symbol() == " "))
        );
    }

    #[test]
    fn diff_buffers_clear_to_end_starts_after_wide_char() {
        let area = Rect::new(0, 0, 10, 1);
        let mut previous = Buffer::empty(area);
        let mut next = Buffer::empty(area);

        previous.set_string(0, 0, "中文", Style::default());
        next.set_string(0, 0, "中", Style::default());

        let commands = diff_buffers(&previous, &next);
        assert!(
            commands
                .iter()
                .any(|command| matches!(command, DrawCommand::ClearToEnd { x: 2, y: 0, .. }))
        );
    }
}
