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

### Multiple live regions

```sh
cargo run --example live_regions
```

This example uses the higher-level `LiveRegion` API. It renders multiple live-updating regions at once while completed lines from each stream are committed into native scrollback.

### Long growing message with a pinned prompt

```sh
cargo run --example long_message
```

This intentionally streams one long live paragraph. It uses `draw_scrollback_tail` to move newly-overflowed rows into native scrollback while keeping the prompt pinned to the bottom.

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

### draw a scrollback-backed live tail

```rust,ignore
let full_height = app.message_height(width);

session.draw_scrollback_tail(
    &mut tail_state,
    full_height,
    prompt_height,
    |area, buf| app.render_full_message(area, buf),
    |area, buf| app.render_prompt(area, buf),
)?;
```

The full message is rendered into an offscreen buffer. Newly-overflowed top rows are inserted into native scrollback, and only the live tail remains above the pinned bottom region.

If the live tail needs viewport chrome such as a border, keep that chrome separate from scrollback content:

```rust,ignore
session.draw_scrollback_tail_with_chrome(
    &mut tail_state,
    full_height,
    prompt_height,
    ratatui::layout::Margin::new(1, 1),
    |area, buf| app.render_full_message_content(area, buf),
    |area, buf| app.render_live_message_border(area, buf),
    |area, buf| app.render_prompt(area, buf),
)?;
```

### draw multiple live regions

```rust,ignore
session.commit(app.take_committed_lines());

session.draw_regions([
    LiveRegion::new("agent")
        .fixed_height(3)
        .render(|area, buf| agent_tail.render(area, buf)),
    LiveRegion::new("tool")
        .fixed_height(3)
        .render(|area, buf| tool_tail.render(area, buf)),
    LiveRegion::new("prompt")
        .min_height(3)
        .max_height(8)
        .height(|width| prompt.desired_height(width))
        .render(|area, buf| prompt.render(area, buf)),
])?;
```

Regions are passed in top-to-bottom order. If there is not enough terminal height, lower regions are preserved first, which keeps prompt-like bottom regions stable.

## Notes

- The main UI does not use the alternate screen.
- Full-height viewports can still insert output into backscroll.
- There is a Zellij insertion fallback mode in the lower-level API.
- The crate currently targets ratatui/crossterm-based applications.
