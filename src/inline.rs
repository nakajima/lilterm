use std::io;
use std::io::Write;

use crossterm::queue;
use crossterm::terminal::BeginSynchronizedUpdate;
use crossterm::terminal::EndSynchronizedUpdate;
use ratatui::backend::Backend;
use ratatui::buffer::Buffer;
use ratatui::buffer::Cell;
use ratatui::layout::Offset;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::text::Line;

use crate::history::insert_history_lines;
use crate::terminal::Frame;
use crate::terminal::Terminal;

const DEFAULT_INLINE_MAX_HEIGHT: u16 = 2000;
const MEASUREMENT_MARK: &str = "\u{e000}";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    Inline { min_height: u16, max_height: u16 },
    PinnedBottom { height: u16 },
}

impl Region {
    pub fn inline() -> Self {
        Self::Inline {
            min_height: 0,
            max_height: DEFAULT_INLINE_MAX_HEIGHT,
        }
    }

    pub fn inline_min(min_height: u16) -> Self {
        Self::Inline {
            min_height,
            max_height: DEFAULT_INLINE_MAX_HEIGHT.max(min_height),
        }
    }

    pub fn inline_max(max_height: u16) -> Self {
        Self::Inline {
            min_height: 0,
            max_height,
        }
    }

    pub fn pinned_bottom(height: u16) -> Self {
        Self::PinnedBottom { height }
    }
}

pub struct LayoutFrame<'a> {
    areas: &'a [Rect],
    buffer: &'a mut Buffer,
}

impl LayoutFrame<'_> {
    pub fn areas(&self) -> &[Rect] {
        self.areas
    }

    pub fn area(&self, index: usize) -> Rect {
        self.areas[index]
    }

    pub fn buffer_mut(&mut self) -> &mut Buffer {
        self.buffer
    }
}

struct MeasuredLayout {
    buffer: Buffer,
    scrollback_height: u16,
    pinned_bottom_height: u16,
}

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

#[derive(Debug, Clone)]
pub struct InlineViewport<B>
where
    B: Backend<Error = io::Error> + Write,
{
    terminal: Terminal<B>,
    pending_history_lines: Vec<Line<'static>>,
}

impl<B> InlineViewport<B>
where
    B: Backend<Error = io::Error> + Write,
{
    pub fn new(terminal: Terminal<B>) -> Self {
        Self {
            terminal,
            pending_history_lines: Vec::new(),
        }
    }

    pub fn terminal(&self) -> &Terminal<B> {
        &self.terminal
    }

    pub fn terminal_mut(&mut self) -> &mut Terminal<B> {
        &mut self.terminal
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

    pub fn draw_layout_tail<R, F>(
        &mut self,
        state: &mut ScrollbackTailState,
        regions: R,
        render: F,
    ) -> io::Result<()>
    where
        R: AsRef<[Region]>,
        F: FnOnce(&mut LayoutFrame<'_>),
    {
        let screen_size = self.terminal.size()?;
        let measured = Self::measure_layout(screen_size, regions.as_ref(), render)?;
        self.draw_tail_buffer(
            state,
            &measured.buffer,
            measured.scrollback_height,
            measured.pinned_bottom_height,
        )
    }

    pub fn finish_layout_tail<R, F>(
        &mut self,
        state: &mut ScrollbackTailState,
        regions: R,
        render: F,
    ) -> io::Result<()>
    where
        R: AsRef<[Region]>,
        F: FnOnce(&mut LayoutFrame<'_>),
    {
        let screen_size = self.terminal.size()?;
        let measured = Self::measure_layout(screen_size, regions.as_ref(), render)?;
        self.finish_tail_buffer(
            state,
            &measured.buffer,
            measured.scrollback_height,
            measured.pinned_bottom_height,
        )
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

            let mut needs_full_repaint = Self::update_inline_viewport(&mut this.terminal, height)?;
            needs_full_repaint |= Self::flush_pending_history_lines(
                &mut this.terminal,
                &mut this.pending_history_lines,
            )?;

            if needs_full_repaint {
                this.terminal.invalidate_viewport();
            }

            this.terminal.try_draw(draw_fn)
        })
    }

    fn measure_layout<F>(
        screen_size: Size,
        regions: &[Region],
        render: F,
    ) -> io::Result<MeasuredLayout>
    where
        F: FnOnce(&mut LayoutFrame<'_>),
    {
        let mut pinned_total = 0u16;
        let mut probe_height = 0u16;
        let mut probe_areas = Vec::with_capacity(regions.len());
        let mut saw_pinned_bottom = false;

        for region in regions {
            match *region {
                Region::Inline {
                    min_height,
                    max_height,
                } => {
                    if saw_pinned_bottom {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "inline regions must come before pinned bottom regions",
                        ));
                    }
                    let height = max_height.max(min_height);
                    probe_areas.push(Rect::new(0, probe_height, screen_size.width, height));
                    probe_height = probe_height.saturating_add(height);
                }
                Region::PinnedBottom { height } => {
                    saw_pinned_bottom = true;
                    let height = height.min(screen_size.height.saturating_sub(pinned_total));
                    probe_areas.push(Rect::new(
                        0,
                        probe_height.saturating_add(pinned_total),
                        screen_size.width,
                        height,
                    ));
                    pinned_total = pinned_total.saturating_add(height);
                }
            }
        }

        let virtual_height = probe_height.saturating_add(pinned_total);
        let mut probe_buffer = Buffer::empty(Rect::new(0, 0, screen_size.width, virtual_height));
        mark_measurement_buffer(&mut probe_buffer);
        {
            let mut frame = LayoutFrame {
                areas: &probe_areas,
                buffer: &mut probe_buffer,
            };
            render(&mut frame);
        }

        let mut inline_total = 0u16;
        let mut measured_areas = Vec::with_capacity(regions.len());
        for (region, area) in regions.iter().zip(probe_areas.iter().copied()) {
            match *region {
                Region::Inline {
                    min_height,
                    max_height,
                } => {
                    let touched_height = touched_height(&probe_buffer, area);
                    let height = touched_height.max(min_height).min(max_height);
                    measured_areas.push(Rect { height, ..area });
                    inline_total = inline_total.saturating_add(height);
                }
                Region::PinnedBottom { .. } => measured_areas.push(area),
            }
        }
        clear_measurement_marks(&mut probe_buffer);

        let total_height = inline_total.saturating_add(pinned_total);
        let mut buffer = Buffer::empty(Rect::new(0, 0, screen_size.width, total_height));
        let mut inline_y = 0u16;
        let mut pinned_y = inline_total;
        for (region, area) in regions.iter().zip(measured_areas.iter().copied()) {
            match *region {
                Region::Inline { .. } => {
                    let target_area = Rect::new(0, inline_y, screen_size.width, area.height);
                    copy_buffer_rows_to_area(
                        &probe_buffer,
                        area.y.saturating_sub(probe_buffer.area.y),
                        target_area,
                        &mut buffer,
                    );
                    inline_y = inline_y.saturating_add(area.height);
                }
                Region::PinnedBottom { .. } => {
                    let target_area = Rect::new(0, pinned_y, screen_size.width, area.height);
                    copy_buffer_rows_to_area(
                        &probe_buffer,
                        area.y.saturating_sub(probe_buffer.area.y),
                        target_area,
                        &mut buffer,
                    );
                    pinned_y = pinned_y.saturating_add(area.height);
                }
            }
        }

        crate::trace::log_changed_args(
            "inline.measure_layout",
            format_args!(
                "screen={screen_size:?} regions={regions:?} scrollback_height={inline_total} pinned_bottom_height={pinned_total}"
            ),
        );

        Ok(MeasuredLayout {
            buffer,
            scrollback_height: inline_total,
            pinned_bottom_height: pinned_total,
        })
    }

    fn draw_tail_buffer(
        &mut self,
        state: &mut ScrollbackTailState,
        full_buffer: &Buffer,
        scrollback_height: u16,
        pinned_bottom_height: u16,
    ) -> io::Result<()> {
        let mut pending_viewport_area = self.pending_viewport_area()?;
        let screen_size = self.terminal.size()?;
        let width = screen_size.width;
        let pinned_bottom_height = pinned_bottom_height.min(screen_size.height);
        let tail_available = screen_size.height.saturating_sub(pinned_bottom_height);
        let visible_tail_height = scrollback_height.min(tail_available);
        let viewport_height = visible_tail_height.saturating_add(pinned_bottom_height);
        let overflow_rows = scrollback_height.saturating_sub(visible_tail_height);

        crate::trace::log_changed_args(
            "inline.scrollback_tail.layout",
            format_args!(
                "screen={screen_size:?} scrollback_height={scrollback_height} pinned_bottom_height={pinned_bottom_height} visible_tail_height={visible_tail_height} viewport_height={viewport_height} overflow_rows={overflow_rows} state={state:?} viewport={:?}",
                self.terminal.viewport_area
            ),
        );

        self.with_synchronized_update(|this| {
            if let Some(new_area) = pending_viewport_area.take() {
                this.terminal.set_viewport_area(new_area);
                this.terminal.clear()?;
            }

            let mut needs_full_repaint = Self::flush_pending_history_lines(
                &mut this.terminal,
                &mut this.pending_history_lines,
            )?;
            needs_full_repaint |=
                Self::update_inline_viewport(&mut this.terminal, viewport_height)?;

            if state.width != width {
                let old_state = *state;
                if state.width == 0 {
                    state.width = width;
                } else {
                    state.width = width;
                    state.committed_rows = overflow_rows;
                }
                crate::trace::log_args(format_args!(
                    "inline.draw_layout_tail state_width_change old={old_state:?} new={state:?} width={width} overflow_rows={overflow_rows}"
                ));
            }
            if state.pinned_bottom_height != pinned_bottom_height
                || state.visible_rows != visible_tail_height
            {
                let old_state = *state;
                state.pinned_bottom_height = pinned_bottom_height;
                state.visible_rows = visible_tail_height;
                crate::trace::log_args(format_args!(
                    "inline.draw_layout_tail state_layout_change old={old_state:?} new={state:?}"
                ));
            }

            if overflow_rows > state.committed_rows {
                let rows_to_insert = overflow_rows - state.committed_rows;
                crate::trace::log_args(format_args!(
                    "inline.draw_layout_tail insert_overflow start_row={} rows_to_insert={rows_to_insert} overflow_rows={overflow_rows} state_before={state:?}",
                    state.committed_rows
                ));
                needs_full_repaint |= Self::insert_buffer_rows(
                    &mut this.terminal,
                    full_buffer,
                    state.committed_rows,
                    rows_to_insert,
                )?;
                state.committed_rows = overflow_rows;
                crate::trace::log_args(format_args!(
                    "inline.draw_layout_tail state_after_insert state={state:?}"
                ));
            }

            let _ = needs_full_repaint;
            this.terminal.invalidate_viewport();

            this.terminal.draw(|frame| {
                let area = frame.area();
                let pinned_height = pinned_bottom_height.min(area.height);
                let tail_area = Rect::new(
                    area.x,
                    area.y,
                    area.width,
                    area.height.saturating_sub(pinned_height),
                );
                let pinned_area = Rect::new(
                    area.x,
                    area.bottom().saturating_sub(pinned_height),
                    area.width,
                    pinned_height,
                );

                crate::trace::log_changed_args(
                    "inline.scrollback_tail.areas",
                    format_args!(
                        "frame_area={area:?} tail_area={tail_area:?} pinned_area={pinned_area:?} state={state:?}"
                    ),
                );
                copy_buffer_rows_to_area(
                    full_buffer,
                    state.committed_rows,
                    tail_area,
                    frame.buffer_mut(),
                );
                copy_buffer_rows_to_area(
                    full_buffer,
                    scrollback_height,
                    pinned_area,
                    frame.buffer_mut(),
                );
            })
        })
    }

    fn finish_tail_buffer(
        &mut self,
        state: &mut ScrollbackTailState,
        full_buffer: &Buffer,
        scrollback_height: u16,
        pinned_bottom_height: u16,
    ) -> io::Result<()> {
        let screen_size = self.terminal.size()?;
        let width = screen_size.width;
        crate::trace::log_args(format_args!(
            "inline.finish_layout_tail start screen={screen_size:?} scrollback_height={scrollback_height} state={state:?} viewport={:?}",
            self.terminal.viewport_area
        ));

        self.with_synchronized_update(|this| {
            let mut needs_full_repaint = Self::flush_pending_history_lines(
                &mut this.terminal,
                &mut this.pending_history_lines,
            )?;

            if state.width != width {
                let old_state = *state;
                if state.width == 0 {
                    state.width = width;
                } else {
                    state.width = width;
                    state.committed_rows = state.committed_rows.min(scrollback_height);
                }
                crate::trace::log_args(format_args!(
                    "inline.finish_layout_tail state_width_change old={old_state:?} new={state:?} width={width} scrollback_height={scrollback_height}"
                ));
            }

            let viewport_area = this.terminal.viewport_area;
            let pinned_bottom_height = if pinned_bottom_height == 0 {
                state.pinned_bottom_height
            } else {
                pinned_bottom_height
            }
            .min(viewport_area.height);
            let inferred_visible_rows = scrollback_height
                .saturating_sub(state.committed_rows)
                .min(viewport_area.height.saturating_sub(pinned_bottom_height));
            let visible_rows = if state.visible_rows == 0 {
                inferred_visible_rows
            } else {
                state
                    .visible_rows
                    .min(viewport_area.height.saturating_sub(pinned_bottom_height))
            };
            let target_committed_rows = scrollback_height.saturating_sub(visible_rows);

            if target_committed_rows > state.committed_rows {
                let rows_to_insert = target_committed_rows - state.committed_rows;
                crate::trace::log_args(format_args!(
                    "inline.finish_layout_tail insert_overflow start_row={} rows_to_insert={rows_to_insert} scrollback_height={scrollback_height} visible_rows={visible_rows} state={state:?}",
                    state.committed_rows
                ));
                needs_full_repaint |= Self::insert_buffer_rows(
                    &mut this.terminal,
                    full_buffer,
                    state.committed_rows,
                    rows_to_insert,
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
                    full_buffer,
                    state.committed_rows,
                    tail_area,
                )?;
            }

            if pinned_bottom_height > 0 {
                let pinned_area = Rect::new(
                    viewport_area.x,
                    viewport_area.bottom().saturating_sub(pinned_bottom_height),
                    viewport_area.width,
                    pinned_bottom_height,
                );
                Self::draw_buffer_rows_to_area(
                    &mut this.terminal,
                    full_buffer,
                    scrollback_height,
                    pinned_area,
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
                    "inline.finish_layout_tail shrink_to_pinned old_area={viewport_area:?} pinned_area={pinned_area:?} visible_rows={visible_rows} needs_full_repaint={needs_full_repaint}"
                ));
                this.terminal.set_viewport_area_preserving_overlap(pinned_area);
            }

            crate::trace::log_args(format_args!(
                "inline.finish_layout_tail done needs_full_repaint={needs_full_repaint} reset_state_from={state:?} viewport={:?}",
                this.terminal.viewport_area
            ));
            state.reset();
            Ok(())
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

    fn update_inline_viewport(terminal: &mut Terminal<B>, height: u16) -> io::Result<bool> {
        let size = terminal.size()?;
        let needs_full_repaint = false;

        let old_area = terminal.viewport_area;
        let mut area = old_area;
        crate::trace::log_changed_args(
            "inline.update_inline_viewport.request",
            format_args!("requested_height={height} screen={size:?} old_area={old_area:?}"),
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
                    "inline.update_inline_viewport bottom_overflow scroll_by={scroll_by} area_before_scroll={area:?}"
                ));
                terminal.scroll_region_up_queued(0..area.top(), scroll_by)?;
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

    fn flush_pending_history_lines(
        terminal: &mut Terminal<B>,
        pending_history_lines: &mut Vec<Line<'static>>,
    ) -> io::Result<bool> {
        if pending_history_lines.is_empty() {
            return Ok(false);
        }

        crate::trace::log_args(format_args!(
            "inline.flush_pending_history_lines count={} viewport_before={:?}",
            pending_history_lines.len(),
            terminal.viewport_area
        ));
        let lines = pending_history_lines.clone();
        match insert_history_lines(terminal, lines) {
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
    ) -> io::Result<bool> {
        if row_count == 0 || terminal.viewport_area.width == 0 {
            return Ok(false);
        }

        crate::trace::log_args(format_args!(
            "inline.insert_buffer_rows start source_area={:?} start_row={start_row} row_count={row_count} viewport_before={:?}",
            source.area, terminal.viewport_area
        ));
        let cursor_position = terminal.last_known_cursor_pos;
        let draw_rows = |buffer: &mut Buffer| {
            copy_buffer_rows_to_area(source, start_row, buffer.area, buffer);
        };

        terminal.insert_before(row_count, draw_rows)?;

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

fn mark_measurement_buffer(buffer: &mut Buffer) {
    for cell in &mut buffer.content {
        set_measurement_mark(cell);
    }
}

fn set_measurement_mark(cell: &mut Cell) {
    *cell = Cell::default();
    cell.set_symbol(MEASUREMENT_MARK);
}

fn is_measurement_mark(cell: &Cell) -> bool {
    cell.symbol() == MEASUREMENT_MARK
}

fn clear_measurement_marks(buffer: &mut Buffer) {
    for cell in &mut buffer.content {
        if cell.symbol() == MEASUREMENT_MARK {
            cell.set_symbol(" ");
        }
    }
}

fn touched_height(buffer: &Buffer, area: Rect) -> u16 {
    if area.is_empty() {
        return 0;
    }

    let mut touched_height = 0u16;
    for y in area.top()..area.bottom() {
        let touched = (area.left()..area.right()).any(|x| {
            buffer
                .cell(Position::new(x, y))
                .is_some_and(|cell| !is_measurement_mark(cell))
        });
        if touched {
            touched_height = y.saturating_sub(area.y).saturating_add(1);
        }
    }
    touched_height
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_backend::VT100Backend;
    use ratatui::layout::Rect;
    use ratatui::style::Color;
    use ratatui::style::Style;
    use ratatui::text::Line;

    fn clear_from_cursor_down_count(bytes: &[u8]) -> usize {
        bytes
            .windows(3)
            .filter(|window| *window == b"\x1b[J")
            .count()
    }

    fn render_numbered_rows_count(row_count: u16, area: Rect, buffer: &mut Buffer) {
        for y in 0..row_count.min(area.height) {
            buffer.set_string(area.x, area.y + y, format!("row{y}"), Style::default());
        }
    }

    fn draw_layout_numbered_rows(
        viewport: &mut InlineViewport<VT100Backend>,
        state: &mut ScrollbackTailState,
        row_count: u16,
        pinned_bottom_height: u16,
    ) {
        viewport
            .draw_layout_tail(
                state,
                [
                    Region::inline_min(1),
                    Region::pinned_bottom(pinned_bottom_height),
                ],
                |frame| {
                    render_numbered_rows_count(row_count, frame.area(0), frame.buffer_mut());
                    let prompt_area = frame.area(1);
                    frame.buffer_mut().set_string(
                        prompt_area.x,
                        prompt_area.y,
                        "prompt",
                        Style::default(),
                    );
                },
            )
            .unwrap();
    }

    fn finish_layout_numbered_rows(
        viewport: &mut InlineViewport<VT100Backend>,
        state: &mut ScrollbackTailState,
        row_count: u16,
        pinned_bottom_height: u16,
    ) {
        viewport
            .finish_layout_tail(
                state,
                [
                    Region::inline_min(1),
                    Region::pinned_bottom(pinned_bottom_height),
                ],
                |frame| {
                    render_numbered_rows_count(row_count, frame.area(0), frame.buffer_mut());
                    let prompt_area = frame.area(1);
                    frame.buffer_mut().set_string(
                        prompt_area.x,
                        prompt_area.y,
                        "prompt",
                        Style::default(),
                    );
                },
            )
            .unwrap();
    }

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

        draw_layout_numbered_rows(&mut viewport, &mut state, 4, 1);
        draw_layout_numbered_rows(&mut viewport, &mut state, 6, 1);

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
    fn layout_tail_measures_rendered_inline_rows_and_inserts_overflow() {
        let width: u16 = 10;
        let height: u16 = 5;
        let backend = VT100Backend::new(width, height);
        let terminal = Terminal::with_options(backend).unwrap();
        let mut viewport = InlineViewport::new(terminal);
        let mut state = ScrollbackTailState::new();

        viewport
            .draw_layout_tail(
                &mut state,
                [Region::inline_min(1), Region::pinned_bottom(1)],
                |frame| {
                    let area = frame.area(0);
                    for y in 0..4 {
                        frame.buffer_mut().set_string(
                            area.x,
                            area.y + y,
                            format!("row{y}"),
                            Style::default(),
                        );
                    }
                    let prompt_area = frame.area(1);
                    frame.buffer_mut().set_string(
                        prompt_area.x,
                        prompt_area.y,
                        "prompt",
                        Style::default(),
                    );
                },
            )
            .unwrap();

        viewport
            .draw_layout_tail(
                &mut state,
                [Region::inline_min(1), Region::pinned_bottom(1)],
                |frame| {
                    let area = frame.area(0);
                    for y in 0..6 {
                        frame.buffer_mut().set_string(
                            area.x,
                            area.y + y,
                            format!("row{y}"),
                            Style::default(),
                        );
                    }
                    let prompt_area = frame.area(1);
                    frame.buffer_mut().set_string(
                        prompt_area.x,
                        prompt_area.y,
                        "prompt",
                        Style::default(),
                    );
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
    fn layout_tail_measurement_does_not_leak_marker_styles() {
        use ratatui::widgets::{Paragraph, Widget};

        let width: u16 = 20;
        let height: u16 = 6;
        let measured = InlineViewport::<VT100Backend>::measure_layout(
            Size::new(width, height),
            &[Region::inline_min(1), Region::pinned_bottom(1)],
            |frame| {
                Paragraph::new("hello")
                    .style(Style::default().fg(Color::Cyan))
                    .render(frame.area(0), frame.buffer_mut());
                let prompt_area = frame.area(1);
                frame.buffer_mut().set_string(
                    prompt_area.x,
                    prompt_area.y,
                    "prompt",
                    Style::default(),
                );
            },
        )
        .unwrap();

        for cell in &measured.buffer.content {
            assert_ne!(cell.symbol(), MEASUREMENT_MARK);
            assert_eq!(cell.bg, Color::Reset);
        }
    }

    #[test]
    fn scrollback_tail_repaints_spaces_after_overflow() {
        use ratatui::widgets::{Paragraph, Widget, Wrap};

        let width: u16 = 36;
        let height: u16 = 8;
        let backend = VT100Backend::new(width, height);
        let terminal = Terminal::with_options(backend).unwrap();
        let mut viewport = InlineViewport::new(terminal);
        let mut state = ScrollbackTailState::new();
        let text = "This is a deliberately long streaming assistant message. ".repeat(8);

        for end in [40usize, 80, 120, 180, 240] {
            let current = &text[..end.min(text.len())];
            viewport
                .draw_layout_tail(
                    &mut state,
                    [Region::inline_min(1), Region::pinned_bottom(1)],
                    |frame| {
                        Paragraph::new(current)
                            .wrap(Wrap { trim: true })
                            .render(frame.area(0), frame.buffer_mut());
                        let prompt_area = frame.area(1);
                        frame.buffer_mut().set_string(
                            prompt_area.x,
                            prompt_area.y,
                            "prompt",
                            Style::default(),
                        );
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
    fn finish_layout_tail_leaves_visible_tail_in_place_when_viewport_is_full_height() {
        let width: u16 = 12;
        let height: u16 = 5;
        let backend = VT100Backend::new(width, height);
        let terminal = Terminal::with_options(backend).unwrap();
        let mut viewport = InlineViewport::new(terminal);
        let mut state = ScrollbackTailState::new();

        draw_layout_numbered_rows(&mut viewport, &mut state, 8, 1);

        assert_eq!(
            viewport.terminal().viewport_area,
            Rect::new(0, 0, width, height)
        );
        assert_eq!(state.committed_rows(), 4);

        finish_layout_numbered_rows(&mut viewport, &mut state, 8, 1);

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
    fn finish_layout_tail_from_unrendered_state_inserts_all_inline_rows() {
        let width: u16 = 12;
        let height: u16 = 5;
        let backend = VT100Backend::new(width, height);
        let terminal = Terminal::with_options(backend).unwrap();
        let mut viewport = InlineViewport::new(terminal);
        let mut state = ScrollbackTailState::new();

        viewport
            .draw(1, |frame| {
                let area = frame.area();
                frame
                    .buffer_mut()
                    .set_string(area.x, area.y, "prompt", Style::default());
            })
            .unwrap();
        let baseline_clears = clear_from_cursor_down_count(viewport.terminal().backend().output());

        finish_layout_numbered_rows(&mut viewport, &mut state, 4, 1);

        assert_eq!(state, ScrollbackTailState::new());
        assert_eq!(viewport.terminal().viewport_area, Rect::new(0, 4, width, 1));
        assert_eq!(
            clear_from_cursor_down_count(viewport.terminal().backend().output()),
            baseline_clears,
            "finishing an unrendered tail should insert history without clearing the screen"
        );

        let rows: Vec<String> = viewport
            .terminal()
            .backend()
            .vt100()
            .screen()
            .rows(0, width)
            .collect();

        assert!(rows[0].contains("row0"));
        assert!(rows[1].contains("row1"));
        assert!(rows[2].contains("row2"));
        assert!(rows[3].contains("row3"));
        assert!(rows[4].contains("prompt"));
    }

    #[test]
    fn scrollback_tail_grows_down_from_non_bottom_prompt_without_from_cursor_down_clears() {
        let width: u16 = 12;
        let height: u16 = 6;
        let backend = VT100Backend::new(width, height);
        let mut terminal = Terminal::with_options(backend).unwrap();
        terminal.set_viewport_area(Rect::new(0, 1, width, 1));
        let mut viewport = InlineViewport::new(terminal);
        let mut state = ScrollbackTailState::new();

        viewport
            .draw(1, |frame| {
                let area = frame.area();
                frame
                    .buffer_mut()
                    .set_string(area.x, area.y, "prompt", Style::default());
            })
            .unwrap();
        let baseline_clears = clear_from_cursor_down_count(viewport.terminal().backend().output());

        for full_height in 1..=3 {
            draw_layout_numbered_rows(&mut viewport, &mut state, full_height, 1);
        }

        assert_eq!(state.committed_rows(), 0);
        assert_eq!(viewport.terminal().viewport_area, Rect::new(0, 1, width, 4));
        assert_eq!(
            clear_from_cursor_down_count(viewport.terminal().backend().output()),
            baseline_clears,
            "a non-bottom inline viewport should grow downward without clear-from-cursor-down"
        );

        let rows: Vec<String> = viewport
            .terminal()
            .backend()
            .vt100()
            .screen()
            .rows(0, width)
            .collect();

        assert!(rows[1].contains("row0"));
        assert!(rows[2].contains("row1"));
        assert!(rows[3].contains("row2"));
        assert!(rows[4].contains("prompt"));
    }

    #[test]
    fn scrollback_tail_flushes_pending_history_before_start_without_full_screen_clear() {
        let width: u16 = 12;
        let height: u16 = 6;
        let backend = VT100Backend::new(width, height);
        let mut terminal = Terminal::with_options(backend).unwrap();
        terminal.set_viewport_area(Rect::new(0, 1, width, 1));
        let mut viewport = InlineViewport::new(terminal);
        let mut state = ScrollbackTailState::new();

        viewport
            .draw(1, |frame| {
                let area = frame.area();
                frame
                    .buffer_mut()
                    .set_string(area.x, area.y, "prompt", Style::default());
            })
            .unwrap();
        let baseline_clears = clear_from_cursor_down_count(viewport.terminal().backend().output());

        viewport.insert_history_lines([Line::from("hist0"), Line::from("hist1")]);
        draw_layout_numbered_rows(&mut viewport, &mut state, 1, 1);

        assert_eq!(state.committed_rows(), 0);
        assert_eq!(viewport.terminal().viewport_area, Rect::new(0, 3, width, 2));
        assert_eq!(
            clear_from_cursor_down_count(viewport.terminal().backend().output()),
            baseline_clears,
            "starting a scrollback tail after pending history should not clear from cursor down"
        );

        let rows: Vec<String> = viewport
            .terminal()
            .backend()
            .vt100()
            .screen()
            .rows(0, width)
            .collect();

        assert!(rows.iter().any(|row| row.contains("hist0")));
        assert!(rows.iter().any(|row| row.contains("hist1")));
        assert!(rows[3].contains("row0"));
        assert!(rows[4].contains("prompt"));
    }

    #[test]
    fn scrollback_tail_lifecycle_preserves_screen_without_from_cursor_down_clears() {
        let width: u16 = 12;
        let height: u16 = 5;
        let backend = VT100Backend::new(width, height);
        let mut terminal = Terminal::with_options(backend).unwrap();
        terminal.set_viewport_area(Rect::new(0, height.saturating_sub(1), width, 1));
        let mut viewport = InlineViewport::new(terminal);
        let mut state = ScrollbackTailState::new();

        viewport
            .draw(1, |frame| {
                let area = frame.area();
                frame
                    .buffer_mut()
                    .set_string(area.x, area.y, "prompt", Style::default());
            })
            .unwrap();
        assert_eq!(viewport.terminal().viewport_area, Rect::new(0, 4, width, 1));
        let baseline_clears = clear_from_cursor_down_count(viewport.terminal().backend().output());

        for full_height in 1..=8 {
            draw_layout_numbered_rows(&mut viewport, &mut state, full_height, 1);
        }

        assert_eq!(state.committed_rows(), 4);
        assert_eq!(
            viewport.terminal().viewport_area,
            Rect::new(0, 0, width, height)
        );
        assert_eq!(
            clear_from_cursor_down_count(viewport.terminal().backend().output()),
            baseline_clears,
            "streaming a scrollback tail should use scroll regions and diffing, not clear from cursor down"
        );

        finish_layout_numbered_rows(&mut viewport, &mut state, 8, 1);
        assert_eq!(viewport.terminal().viewport_area, Rect::new(0, 4, width, 1));
        assert_eq!(
            clear_from_cursor_down_count(viewport.terminal().backend().output()),
            baseline_clears,
            "finishing a scrollback tail should not clear the visible screen"
        );

        viewport
            .draw(1, |frame| {
                let area = frame.area();
                frame
                    .buffer_mut()
                    .set_string(area.x, area.y, "prompt", Style::default());
            })
            .unwrap();
        assert_eq!(
            clear_from_cursor_down_count(viewport.terminal().backend().output()),
            baseline_clears,
            "redrawing the pinned prompt after finish should not clear the visible tail"
        );

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
