use std::borrow::Cow;
use std::io;
use std::io::Write;

use crossterm::queue;
use crossterm::terminal::BeginSynchronizedUpdate;
use crossterm::terminal::EndSynchronizedUpdate;
use ratatui::backend::Backend;
use ratatui::buffer::Buffer;
use ratatui::layout::Margin;
use ratatui::layout::Offset;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::text::Line;

use crate::history::InsertHistoryMode;
use crate::history::insert_history_lines_with_mode;
use crate::terminal::Frame;
use crate::terminal::Terminal;

type HeightFn<'a> = dyn Fn(u16) -> u16 + 'a;
type RenderFn<'a> = dyn Fn(Rect, &mut Buffer) + 'a;
type CursorFn<'a> = dyn Fn(Rect) -> Option<Position> + 'a;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbackTailState {
    committed_rows: u16,
    width: u16,
    pinned_bottom_height: u16,
    visible_rows: u16,
}

impl ScrollbackTailState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn committed_rows(&self) -> u16 {
        self.committed_rows
    }

    pub fn reset(&mut self) {
        let old = *self;
        *self = Self::default();
        if old != Self::default() {
            crate::trace::log_args(format_args!(
                "scrollback_tail_state.reset old={old:?} new={:?}",
                self
            ));
        }
    }
}

pub struct LiveRegion<'a> {
    pub id: Cow<'a, str>,
    min_height: u16,
    max_height: Option<u16>,
    desired_height: Box<HeightFn<'a>>,
    render: Box<RenderFn<'a>>,
    cursor: Option<Box<CursorFn<'a>>>,
}

impl<'a> LiveRegion<'a> {
    pub fn new(id: impl Into<Cow<'a, str>>) -> Self {
        Self {
            id: id.into(),
            min_height: 0,
            max_height: None,
            desired_height: Box::new(|_| 0),
            render: Box::new(|_, _| {}),
            cursor: None,
        }
    }

    pub fn min_height(mut self, height: u16) -> Self {
        self.min_height = height;
        self
    }

    pub fn max_height(mut self, height: u16) -> Self {
        self.max_height = Some(height);
        self
    }

    pub fn fixed_height(mut self, height: u16) -> Self {
        self.min_height = height;
        self.max_height = Some(height);
        self.desired_height = Box::new(move |_| height);
        self
    }

    pub fn height(mut self, height: impl Fn(u16) -> u16 + 'a) -> Self {
        self.desired_height = Box::new(height);
        self
    }

    pub fn render(mut self, render: impl Fn(Rect, &mut Buffer) + 'a) -> Self {
        self.render = Box::new(render);
        self
    }

    pub fn cursor(mut self, cursor: impl Fn(Rect) -> Option<Position> + 'a) -> Self {
        self.cursor = Some(Box::new(cursor));
        self
    }

    fn desired_height(&self, width: u16) -> u16 {
        let height = (self.desired_height)(width).max(self.min_height);
        match self.max_height {
            Some(max_height) => height.min(max_height),
            None => height,
        }
    }

    fn render_into(&self, area: Rect, buffer: &mut Buffer) {
        (self.render)(area, buffer);
    }

    fn cursor_position(&self, area: Rect) -> Option<Position> {
        self.cursor.as_ref().and_then(|cursor| cursor(area))
    }
}

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
        let old_len = self.pending_history_lines.len();
        self.pending_history_lines.extend(lines);
        crate::trace::log_args(format_args!(
            "inline.insert_history_lines old_pending={old_len} new_pending={} viewport={:?}",
            self.pending_history_lines.len(),
            self.terminal.viewport_area
        ));
    }

    pub fn commit<I>(&mut self, lines: I)
    where
        I: IntoIterator<Item = Line<'static>>,
    {
        self.insert_history_lines(lines);
    }

    pub fn draw_regions<'a, I>(&mut self, regions: I) -> io::Result<()>
    where
        I: IntoIterator<Item = LiveRegion<'a>>,
    {
        let regions: Vec<LiveRegion<'a>> = regions.into_iter().collect();
        let screen_size = self.terminal.size()?;
        let desired_heights: Vec<u16> = regions
            .iter()
            .map(|region| region.desired_height(screen_size.width))
            .collect();
        let height = desired_heights
            .iter()
            .copied()
            .fold(0u16, u16::saturating_add)
            .min(screen_size.height);
        let rects = allocate_regions(Rect::new(0, 0, screen_size.width, height), &desired_heights);
        crate::trace::log_changed_args(
            "inline.draw_regions.layout",
            format_args!(
                "screen={screen_size:?} requested_height={height} desired_heights={desired_heights:?} rects={rects:?} viewport={:?}",
                self.terminal.viewport_area
            ),
        );

        self.draw(height, |frame| {
            let area = frame.area();
            let offset_y = area.y;
            let mut cursor = None;
            for (region, rect) in regions.iter().zip(rects) {
                if rect.height == 0 {
                    continue;
                }
                let rect = Rect::new(area.x + rect.x, offset_y + rect.y, rect.width, rect.height);
                region.render_into(rect, frame.buffer_mut());
                if let Some(position) = region.cursor_position(rect) {
                    cursor = Some(position);
                }
            }
            if let Some(position) = cursor {
                frame.set_cursor_position(position);
            }
        })
    }

    pub fn draw_scrollback_tail<F, P>(
        &mut self,
        state: &mut ScrollbackTailState,
        full_height: u16,
        pinned_bottom_height: u16,
        render_full: F,
        render_pinned_bottom: P,
    ) -> io::Result<()>
    where
        F: FnOnce(Rect, &mut Buffer),
        P: FnOnce(Rect, &mut Buffer),
    {
        self.draw_scrollback_tail_with_chrome(
            state,
            full_height,
            pinned_bottom_height,
            Margin::default(),
            render_full,
            |_, _| {},
            render_pinned_bottom,
        )
    }

    pub fn finish_scrollback_tail<F>(
        &mut self,
        state: &mut ScrollbackTailState,
        full_height: u16,
        render_full: F,
    ) -> io::Result<()>
    where
        F: FnOnce(Rect, &mut Buffer),
    {
        let screen_size = self.terminal.size()?;
        let width = screen_size.width;
        crate::trace::log_args(format_args!(
            "inline.finish_scrollback_tail start screen={screen_size:?} full_height={full_height} state={state:?} viewport={:?}",
            self.terminal.viewport_area
        ));
        let mut full_buffer = Buffer::empty(Rect::new(0, 0, width, full_height));
        if width > 0 && full_height > 0 {
            render_full(full_buffer.area, &mut full_buffer);
        }

        self.with_synchronized_update(|this| {
            let mut needs_full_repaint = Self::flush_pending_history_lines(
                &mut this.terminal,
                &mut this.pending_history_lines,
                this.history_mode,
            )?;

            if state.width != width {
                let old_state = *state;
                if state.width == 0 {
                    state.width = width;
                } else {
                    state.width = width;
                    state.committed_rows = state.committed_rows.min(full_height);
                }
                crate::trace::log_args(format_args!(
                    "inline.finish_scrollback_tail state_width_change old={old_state:?} new={state:?} width={width} full_height={full_height}"
                ));
            }

            let viewport_area = this.terminal.viewport_area;
            let pinned_bottom_height = state.pinned_bottom_height.min(viewport_area.height);
            let inferred_visible_rows = full_height
                .saturating_sub(state.committed_rows)
                .min(viewport_area.height.saturating_sub(pinned_bottom_height));
            let visible_rows = if state.visible_rows == 0 {
                inferred_visible_rows
            } else {
                state
                    .visible_rows
                    .min(viewport_area.height.saturating_sub(pinned_bottom_height))
            };
            let target_committed_rows = full_height.saturating_sub(visible_rows);

            if target_committed_rows > state.committed_rows {
                let rows_to_insert = target_committed_rows - state.committed_rows;
                crate::trace::log_args(format_args!(
                    "inline.finish_scrollback_tail insert_overflow start_row={} rows_to_insert={rows_to_insert} full_height={full_height} visible_rows={visible_rows} state={state:?}",
                    state.committed_rows
                ));
                needs_full_repaint |= Self::insert_buffer_rows(
                    &mut this.terminal,
                    &full_buffer,
                    state.committed_rows,
                    rows_to_insert,
                    this.history_mode,
                )?;
                state.committed_rows = target_committed_rows;
            }

            let viewport_area = this.terminal.viewport_area;
            let pinned_bottom_height = pinned_bottom_height.min(viewport_area.height);
            let visible_rows = visible_rows.min(viewport_area.height.saturating_sub(pinned_bottom_height));
            let tail_area = Rect::new(
                viewport_area.x,
                viewport_area.y,
                viewport_area.width,
                visible_rows,
            );
            if visible_rows > 0 {
                Self::draw_buffer_rows_to_area(
                    &mut this.terminal,
                    &full_buffer,
                    state.committed_rows,
                    tail_area,
                )?;
            }

            if pinned_bottom_height < viewport_area.height {
                let pinned_area = Rect::new(
                    viewport_area.x,
                    viewport_area.bottom().saturating_sub(pinned_bottom_height),
                    viewport_area.width,
                    pinned_bottom_height,
                );
                crate::trace::log_args(format_args!(
                    "inline.finish_scrollback_tail shrink_to_pinned old_area={viewport_area:?} pinned_area={pinned_area:?} visible_rows={visible_rows} needs_full_repaint={needs_full_repaint}"
                ));
                this.terminal.set_viewport_area_preserving_overlap(pinned_area);
            }

            crate::trace::log_args(format_args!(
                "inline.finish_scrollback_tail done needs_full_repaint={needs_full_repaint} reset_state_from={state:?} viewport={:?}",
                this.terminal.viewport_area
            ));
            state.reset();
            Ok(())
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw_scrollback_tail_with_chrome<F, C, P>(
        &mut self,
        state: &mut ScrollbackTailState,
        full_height: u16,
        pinned_bottom_height: u16,
        tail_margin: Margin,
        render_full: F,
        render_tail_chrome: C,
        render_pinned_bottom: P,
    ) -> io::Result<()>
    where
        F: FnOnce(Rect, &mut Buffer),
        C: FnOnce(Rect, &mut Buffer),
        P: FnOnce(Rect, &mut Buffer),
    {
        let mut pending_viewport_area = self.pending_viewport_area()?;
        let screen_size = self.terminal.size()?;
        let width = screen_size.width;
        let pinned_bottom_height = pinned_bottom_height.min(screen_size.height);
        let tail_outer_available = screen_size.height.saturating_sub(pinned_bottom_height);
        let vertical_chrome = tail_margin.vertical.saturating_mul(2);
        let tail_content_available = tail_outer_available.saturating_sub(vertical_chrome);
        let tail_content_height = full_height.min(tail_content_available);
        let tail_outer_height = tail_content_height
            .saturating_add(vertical_chrome)
            .min(tail_outer_available);
        let viewport_height = tail_outer_height.saturating_add(pinned_bottom_height);
        let overflow_rows = full_height.saturating_sub(tail_content_height);
        let source_width = width
            .saturating_sub(tail_margin.horizontal.saturating_mul(2))
            .max(1);

        crate::trace::log_changed_args(
            "inline.scrollback_tail.layout",
            format_args!(
                "screen={screen_size:?} full_height={full_height} pinned_bottom_height={pinned_bottom_height} tail_margin={tail_margin:?} tail_outer_height={tail_outer_height} tail_content_height={tail_content_height} viewport_height={viewport_height} overflow_rows={overflow_rows} source_width={source_width} state={state:?} viewport={:?}",
                self.terminal.viewport_area
            ),
        );

        let mut full_buffer = Buffer::empty(Rect::new(0, 0, width, full_height));
        if width > 0 && full_height > 0 {
            render_full(Rect::new(0, 0, source_width, full_height), &mut full_buffer);
        }

        self.with_synchronized_update(|this| {
            if let Some(new_area) = pending_viewport_area.take() {
                this.terminal.set_viewport_area(new_area);
                this.terminal.clear()?;
            }

            let mut needs_full_repaint = Self::flush_pending_history_lines(
                &mut this.terminal,
                &mut this.pending_history_lines,
                this.history_mode,
            )?;
            needs_full_repaint |= Self::update_inline_viewport(
                &mut this.terminal,
                viewport_height,
                this.history_mode,
            )?;

            if state.width != width {
                let old_state = *state;
                if state.width == 0 {
                    state.width = width;
                } else {
                    state.width = width;
                    state.committed_rows = overflow_rows;
                }
                crate::trace::log_args(format_args!(
                    "inline.draw_scrollback_tail_with_chrome state_width_change old={old_state:?} new={state:?} width={width} overflow_rows={overflow_rows}"
                ));
            }
            if state.pinned_bottom_height != pinned_bottom_height
                || state.visible_rows != tail_content_height
            {
                let old_state = *state;
                state.pinned_bottom_height = pinned_bottom_height;
                state.visible_rows = tail_content_height;
                crate::trace::log_args(format_args!(
                    "inline.draw_scrollback_tail_with_chrome state_layout_change old={old_state:?} new={state:?}"
                ));
            }

            if overflow_rows > state.committed_rows {
                let rows_to_insert = overflow_rows - state.committed_rows;
                crate::trace::log_args(format_args!(
                    "inline.draw_scrollback_tail_with_chrome insert_overflow start_row={} rows_to_insert={rows_to_insert} overflow_rows={overflow_rows} state_before={state:?}",
                    state.committed_rows
                ));
                needs_full_repaint |= Self::insert_buffer_rows(
                    &mut this.terminal,
                    &full_buffer,
                    state.committed_rows,
                    rows_to_insert,
                    this.history_mode,
                )?;
                state.committed_rows = overflow_rows;
                crate::trace::log_args(format_args!(
                    "inline.draw_scrollback_tail_with_chrome state_after_insert state={state:?}"
                ));
            }

            // Scrollback-backed live tails mutate the physical terminal with
            // scroll-region operations while the wrapped text can also reflow
            // on every chunk. Force a full repaint so spaces are emitted too;
            // otherwise stale glyphs from previous wraps or viewport chrome can
            // survive in cells the widget now considers blank.
            let _ = needs_full_repaint;
            this.terminal.invalidate_viewport();

            this.terminal.draw(|frame| {
                let area = frame.area();
                let pinned_height = pinned_bottom_height.min(area.height);
                let tail_outer_height = area.height.saturating_sub(pinned_height);
                let tail_outer_area = Rect::new(area.x, area.y, area.width, tail_outer_height);
                let tail_content_area = tail_outer_area.inner(tail_margin);
                let pinned_area = Rect::new(
                    area.x,
                    area.bottom().saturating_sub(pinned_height),
                    area.width,
                    pinned_height,
                );

                crate::trace::log_changed_args(
                    "inline.scrollback_tail.areas",
                    format_args!(
                        "frame_area={area:?} tail_outer_area={tail_outer_area:?} tail_content_area={tail_content_area:?} pinned_area={pinned_area:?} state={state:?}"
                    ),
                );
                render_tail_chrome(tail_outer_area, frame.buffer_mut());
                copy_buffer_rows_to_area(
                    &full_buffer,
                    state.committed_rows,
                    tail_content_area,
                    frame.buffer_mut(),
                );
                render_pinned_bottom(pinned_area, frame.buffer_mut());
            })
        })
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
        crate::trace::log_changed_args(
            "inline.try_draw.layout",
            format_args!(
                "requested_height={height} viewport={:?}",
                self.terminal.viewport_area
            ),
        );
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

        let old_area = terminal.viewport_area;
        let mut area = old_area;
        crate::trace::log_changed_args(
            "inline.update_inline_viewport.request",
            format_args!(
                "requested_height={height} mode={mode:?} screen={size:?} old_area={old_area:?}"
            ),
        );
        area.height = height.min(size.height);
        area.width = size.width;

        if old_area.bottom() >= size.height && area.height <= old_area.height {
            area.y = size.height.saturating_sub(area.height);
        }

        if area.y > size.height {
            area.y = size.height.saturating_sub(area.height);
        }

        if area.bottom() > size.height {
            let scroll_by = area.bottom() - size.height;
            if scroll_by > 0 && area.top() > 0 {
                crate::trace::log_args(format_args!(
                    "inline.update_inline_viewport bottom_overflow scroll_by={scroll_by} area_before_scroll={area:?} mode={mode:?}"
                ));
                if matches!(mode, InsertHistoryMode::Zellij) {
                    Self::scroll_zellij_expanded_viewport(terminal, size, scroll_by)?;
                    needs_full_repaint = true;
                } else {
                    terminal.scroll_region_up_queued(0..area.top(), scroll_by)?;
                }
            }
            area.y = size.height.saturating_sub(area.height);
        }

        if area != old_area {
            let keeps_bottom_edge =
                area.width == old_area.width && area.bottom() == old_area.bottom();
            let grows_downward_from_same_top = area.width == old_area.width
                && area.top() == old_area.top()
                && area.height >= old_area.height;
            let can_preserve_overlap = keeps_bottom_edge || grows_downward_from_same_top;

            if can_preserve_overlap && !needs_full_repaint {
                crate::trace::log_args(format_args!(
                    "inline.update_inline_viewport apply_preserve_overlap old_area={old_area:?} new_area={area:?} needs_full_repaint={needs_full_repaint} keeps_bottom_edge={keeps_bottom_edge} grows_downward_from_same_top={grows_downward_from_same_top}"
                ));
                terminal.set_viewport_area_preserving_overlap(area);
            } else {
                crate::trace::log_args(format_args!(
                    "inline.update_inline_viewport apply_clear_and_set old_area={old_area:?} new_area={area:?} needs_full_repaint={needs_full_repaint} keeps_bottom_edge={keeps_bottom_edge} grows_downward_from_same_top={grows_downward_from_same_top}"
                ));
                terminal.clear()?;
                terminal.set_viewport_area(area);
            }
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

        crate::trace::log_args(format_args!(
            "inline.flush_pending_history_lines count={} mode={mode:?} viewport_before={:?}",
            pending_history_lines.len(),
            terminal.viewport_area
        ));
        let lines = pending_history_lines.clone();
        match insert_history_lines_with_mode(terminal, lines, mode) {
            Ok(()) => {
                pending_history_lines.clear();
                crate::trace::log_args(format_args!(
                    "inline.flush_pending_history_lines done viewport_after={:?}",
                    terminal.viewport_area
                ));
                Ok(true)
            }
            Err(err) => Err(err),
        }
    }

    fn draw_buffer_rows_to_area(
        terminal: &mut Terminal<B>,
        source: &Buffer,
        start_row: u16,
        target_area: Rect,
    ) -> io::Result<()> {
        if target_area.is_empty() || terminal.viewport_area.width == 0 {
            return Ok(());
        }

        let mut target = Buffer::empty(target_area);
        copy_buffer_rows_to_area(source, start_row, target_area, &mut target);
        let iter = target.content.iter().enumerate().map(|(index, cell)| {
            let (x, y) = target.pos_of(index);
            (x, y, cell)
        });
        terminal.backend_mut().draw(iter)?;
        terminal.last_known_cursor_pos = Position::new(
            target_area.x + target_area.width.saturating_sub(1),
            target_area.y + target_area.height.saturating_sub(1),
        );
        Ok(())
    }

    fn insert_buffer_rows(
        terminal: &mut Terminal<B>,
        source: &Buffer,
        start_row: u16,
        row_count: u16,
        mode: InsertHistoryMode,
    ) -> io::Result<bool> {
        if row_count == 0 || terminal.viewport_area.width == 0 {
            return Ok(false);
        }

        crate::trace::log_args(format_args!(
            "inline.insert_buffer_rows start source_area={:?} start_row={start_row} row_count={row_count} mode={mode:?} viewport_before={:?}",
            source.area, terminal.viewport_area
        ));
        let cursor_position = terminal.last_known_cursor_pos;
        let draw_rows = |buffer: &mut Buffer| {
            copy_buffer_rows_to_area(source, start_row, buffer.area, buffer);
        };

        match mode {
            InsertHistoryMode::Standard => terminal.insert_before(row_count, draw_rows)?,
            InsertHistoryMode::Zellij => {
                terminal.insert_before_without_scrolling_regions(row_count, draw_rows)?;
                terminal.invalidate_viewport();
            }
        }

        terminal.set_cursor_position(cursor_position)?;
        terminal.note_history_rows_inserted(row_count);
        crate::trace::log_args(format_args!(
            "inline.insert_buffer_rows done viewport_after={:?} restored_cursor={cursor_position:?}",
            terminal.viewport_area
        ));
        Ok(true)
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
                let new_area = terminal.viewport_area.offset(offset);
                crate::trace::log_args(format_args!(
                    "inline.pending_viewport_area screen_size={screen_size:?} last_screen_size={last_known_screen_size:?} cursor_pos={cursor_pos:?} last_cursor_pos={last_known_cursor_pos:?} old_area={:?} new_area={new_area:?}",
                    terminal.viewport_area
                ));
                return Ok(Some(new_area));
            }
        }
        Ok(None)
    }
}

fn copy_buffer_rows_to_area(
    source: &Buffer,
    start_row: u16,
    target_area: Rect,
    target: &mut Buffer,
) {
    if target_area.is_empty() || source.area.is_empty() {
        return;
    }

    let rows = target_area
        .height
        .min(source.area.height.saturating_sub(start_row));
    let columns = target_area.width.min(source.area.width);
    for row in 0..rows {
        for column in 0..columns {
            let source_position =
                Position::new(source.area.x + column, source.area.y + start_row + row);
            let target_position = Position::new(target_area.x + column, target_area.y + row);
            let Some(source_cell) = source.cell(source_position) else {
                continue;
            };
            let Some(target_cell) = target.cell_mut(target_position) else {
                continue;
            };
            *target_cell = source_cell.clone();
        }
    }
}

fn allocate_regions(area: Rect, desired_heights: &[u16]) -> Vec<Rect> {
    let mut rects = vec![Rect::new(area.x, area.y, area.width, 0); desired_heights.len()];
    let mut remaining = area.height;
    let mut bottom = area.bottom();

    for (index, desired_height) in desired_heights.iter().copied().enumerate().rev() {
        let height = desired_height.min(remaining);
        bottom = bottom.saturating_sub(height);
        rects[index] = Rect::new(area.x, bottom, area.width, height);
        remaining = remaining.saturating_sub(height);
    }

    rects
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
    fn scrollback_tail_inserts_overflow_and_draws_live_tail_above_pinned_bottom() {
        let width: u16 = 10;
        let height: u16 = 5;
        let backend = VT100Backend::new(width, height);
        let terminal = Terminal::with_options(backend).unwrap();
        let mut viewport = InlineViewport::new(terminal);
        let mut state = ScrollbackTailState::new();

        viewport
            .draw_scrollback_tail(
                &mut state,
                4,
                1,
                |area, buffer| {
                    for y in 0..area.height {
                        buffer.set_string(area.x, area.y + y, format!("row{y}"), Style::default());
                    }
                },
                |area, buffer| {
                    buffer.set_string(area.x, area.y, "prompt", Style::default());
                },
            )
            .unwrap();

        viewport
            .draw_scrollback_tail(
                &mut state,
                6,
                1,
                |area, buffer| {
                    for y in 0..area.height {
                        buffer.set_string(area.x, area.y + y, format!("row{y}"), Style::default());
                    }
                },
                |area, buffer| {
                    buffer.set_string(area.x, area.y, "prompt", Style::default());
                },
            )
            .unwrap();

        let rows: Vec<String> = viewport
            .terminal()
            .backend()
            .vt100()
            .screen()
            .rows(0, width)
            .collect();

        assert_eq!(state.committed_rows(), 2);
        assert!(rows.iter().any(|row| row.contains("row2")));
        assert!(rows.iter().any(|row| row.contains("row5")));
        assert!(rows.iter().any(|row| row.contains("prompt")));
        assert!(!rows.iter().any(|row| row.contains("row0")));
    }

    #[test]
    fn scrollback_tail_with_chrome_repaints_spaces_after_overflow() {
        use ratatui::layout::Margin;
        use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};

        let width: u16 = 36;
        let height: u16 = 8;
        let backend = VT100Backend::new(width, height);
        let terminal = Terminal::with_options(backend).unwrap();
        let mut viewport = InlineViewport::new(terminal);
        let mut state = ScrollbackTailState::new();
        let text = "This is a deliberately long streaming assistant message. ".repeat(8);

        for end in [40usize, 80, 120, 180, 240] {
            let current = &text[..end.min(text.len())];
            let full_height = Paragraph::new(current)
                .wrap(Wrap { trim: true })
                .line_count(width.saturating_sub(2).max(1)) as u16;
            viewport
                .draw_scrollback_tail_with_chrome(
                    &mut state,
                    full_height,
                    1,
                    Margin::new(1, 1),
                    |area, buffer| {
                        Paragraph::new(current)
                            .wrap(Wrap { trim: true })
                            .render(area, buffer);
                    },
                    |area, buffer| {
                        Block::default().borders(Borders::ALL).render(area, buffer);
                    },
                    |area, buffer| {
                        buffer.set_string(area.x, area.y, "prompt", Style::default());
                    },
                )
                .unwrap();
        }

        let rows: Vec<String> = viewport
            .terminal()
            .backend()
            .vt100()
            .screen()
            .rows(0, width)
            .collect();

        assert!(rows.iter().any(|row| row.contains("prompt")));
    }

    #[test]
    fn finish_scrollback_tail_leaves_visible_tail_in_place_when_viewport_is_full_height() {
        fn render_rows(area: Rect, buffer: &mut Buffer) {
            for y in 0..area.height {
                buffer.set_string(area.x, area.y + y, format!("row{y}"), Style::default());
            }
        }

        let width: u16 = 12;
        let height: u16 = 5;
        let backend = VT100Backend::new(width, height);
        let terminal = Terminal::with_options(backend).unwrap();
        let mut viewport = InlineViewport::new(terminal);
        let mut state = ScrollbackTailState::new();

        viewport
            .draw_scrollback_tail(&mut state, 8, 1, render_rows, |area, buffer| {
                buffer.set_string(area.x, area.y, "prompt", Style::default());
            })
            .unwrap();

        assert_eq!(
            viewport.terminal().viewport_area,
            Rect::new(0, 0, width, height)
        );
        assert_eq!(state.committed_rows(), 4);

        viewport
            .finish_scrollback_tail(&mut state, 8, render_rows)
            .unwrap();

        assert_eq!(state, ScrollbackTailState::new());
        assert_eq!(viewport.terminal().viewport_area, Rect::new(0, 4, width, 1));

        viewport
            .draw(1, |frame| {
                let area = frame.area();
                frame
                    .buffer_mut()
                    .set_string(area.x, area.y, "prompt", Style::default());
            })
            .unwrap();

        let rows: Vec<String> = viewport
            .terminal()
            .backend()
            .vt100()
            .screen()
            .rows(0, width)
            .collect();

        assert!(rows[0].contains("row4"));
        assert!(rows[1].contains("row5"));
        assert!(rows[2].contains("row6"));
        assert!(rows[3].contains("row7"));
        assert!(rows[4].contains("prompt"));
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
