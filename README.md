# lilterm

`lilterm` is a small Rust crate for building inline, viewport-based terminal UIs that keep native terminal scrollback intact.

The goal is the terminal behavior used by coding-agent UIs:

- keep the main UI inline instead of using the alternate screen
- preserve native scrollback
- insert completed output above the live viewport
- render only the unstable/live UI in the viewport
- avoid flicker and viewport bouncing

## Core idea

```rust,ignore
let mut session = lilterm::init()?;

loop {
    let tick = app.tick();

    if !tick.history_lines.is_empty() {
        session.insert_history_lines(tick.history_lines);
    }

    let size = session.terminal().size()?;
    let height = app.desired_height(size.width, size.height);

    session.draw(height, |frame| {
        app.render(frame);
    })?;
}

session.finish()?;
```

`draw` is safe to call whenever your loop wakes. `lilterm` diffs the rendered buffer and only writes terminal updates when something actually changed.

## Examples

### Minimal streaming demo

```sh
cargo run --example streaming
```

This example has no extra textarea dependency. It includes a tiny local prompt editor so the important `lilterm` usage is easy to see.

### Ratatui widgets + ratatui-textarea + smol

```sh
cargo run --example widgets_textarea
```

This example uses:

- regular ratatui widget rendering (`Block`, `Paragraph`, `Widget::render`)
- `ratatui-textarea` for prompt input
- `smol` + async/await for the event loop
- `lilterm` for scrollback insertion and inline viewport management

### Long growing message with a pinned prompt

```sh
cargo run --example long_message
```

This intentionally streams one long live paragraph. It uses `draw_layout_tail` and `Region` to auto-measure rendered inline rows, move newly-overflowed rows into native scrollback, and keep the prompt pinned to the bottom.

## api

### session

```rust,ignore
let mut session = lilterm::init()?;
// ...
session.finish()?;
```

`init()` enters raw terminal mode and returns a restore-on-drop session. `finish()` restores terminal modes explicitly.

### insert stable history

```rust,ignore
session.insert_history_lines(vec![
    ratatui::text::Line::from("agent> completed line"),
]);
```

these lines are inserted above the live viewport, into native terminal scrollback.

### draw live viewport

```rust,ignore
session.draw(height, |frame| {
    let area = frame.area();
    my_widget.render(area, frame.buffer_mut());
    frame.set_cursor_position((x, y));
})?;
```

Only render the live/unstable UI here: prompt editor, current partial response, status, spinner, etc.

### draw an auto-measured scrollback-backed live tail

```rust,ignore
session.draw_layout_tail(
    &mut tail_state,
    [
        lilterm::Region::inline_min(1),
        lilterm::Region::pinned_bottom(prompt_height),
    ],
    |frame| {
        app.render_message(frame.area(0), frame.buffer_mut());
        app.render_prompt(frame.area(1), frame.buffer_mut());
    },
)?;
```

Inline regions are rendered into an offscreen virtual buffer and measured from the rows actually touched by ratatui widgets. Newly-overflowed top rows are inserted into native scrollback, while `pinned_bottom` rows remain live in the viewport and are never committed to scrollback.

Available inline constructors:

```rust,ignore
lilterm::Region::inline();
lilterm::Region::inline_min(1);
lilterm::Region::inline_max(2000);
```

## Debug tracing

Set `LILTERM_TRACE` to log layout decisions to a file:

```sh
LILTERM_TRACE=/tmp/lilterm-trace.log cargo run --example long_message
```

`LILTERM_TRACE=1` writes to `/tmp/lilterm-trace.log`. The file is appended to, so remove it before a fresh run if needed.

## Notes

- The main UI does not use the alternate screen.
- Full-height viewports can still insert output into backscroll.
- The crate currently targets ratatui/crossterm-based applications.
