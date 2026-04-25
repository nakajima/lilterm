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

## Notes

- The main UI does not use the alternate screen.
- Full-height viewports can still insert output into backscroll.
- There is a Zellij insertion fallback mode in the lower-level API.
- The crate currently targets ratatui/crossterm-based applications.
