use std::io;
use std::io::IsTerminal;
use std::io::Stdout;
use std::io::stdin;
use std::io::stdout;
use std::ops::Deref;
use std::ops::DerefMut;

use crossterm::event::DisableBracketedPaste;
use crossterm::event::DisableFocusChange;
use crossterm::event::EnableBracketedPaste;
use crossterm::event::EnableFocusChange;
use crossterm::execute;
use crossterm::terminal::disable_raw_mode;
use crossterm::terminal::enable_raw_mode;
use ratatui::backend::CrosstermBackend;

use crate::inline::InlineViewport;
use crate::terminal::Terminal;

pub type CrosstermTerminal = Terminal<CrosstermBackend<Stdout>>;
pub type CrosstermInlineViewport = InlineViewport<CrosstermBackend<Stdout>>;

pub fn set_modes() -> io::Result<()> {
    execute!(stdout(), EnableBracketedPaste)?;
    enable_raw_mode()?;
    let _ = execute!(stdout(), EnableFocusChange);
    Ok(())
}

pub fn restore() -> io::Result<()> {
    execute!(stdout(), DisableBracketedPaste)?;
    let _ = execute!(stdout(), DisableFocusChange);
    disable_raw_mode()?;
    let _ = execute!(stdout(), crossterm::cursor::Show);
    Ok(())
}

#[cfg(unix)]
fn flush_terminal_input_buffer() {
    let result = unsafe { libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH) };
    if result != 0 {
        let _ = io::Error::last_os_error();
    }
}

#[cfg(windows)]
fn flush_terminal_input_buffer() {}

#[cfg(not(any(unix, windows)))]
fn flush_terminal_input_buffer() {}

pub fn init() -> io::Result<Session> {
    Session::start()
}

#[derive(Debug)]
pub struct Session {
    viewport: CrosstermInlineViewport,
    restore_on_drop: bool,
}

impl Session {
    pub fn start() -> io::Result<Self> {
        if !stdin().is_terminal() {
            return Err(io::Error::other("stdin is not a terminal"));
        }
        if !stdout().is_terminal() {
            return Err(io::Error::other("stdout is not a terminal"));
        }

        set_modes()?;
        flush_terminal_input_buffer();

        let backend = CrosstermBackend::new(stdout());
        let terminal = Terminal::with_options(backend)?;
        Ok(Self {
            viewport: InlineViewport::new(terminal),
            restore_on_drop: true,
        })
    }

    pub fn viewport(&self) -> &CrosstermInlineViewport {
        &self.viewport
    }

    pub fn viewport_mut(&mut self) -> &mut CrosstermInlineViewport {
        &mut self.viewport
    }

    pub fn finish(mut self) -> io::Result<()> {
        let result = restore();
        if result.is_ok() {
            self.restore_on_drop = false;
        }
        result
    }
}

impl Deref for Session {
    type Target = CrosstermInlineViewport;

    fn deref(&self) -> &Self::Target {
        &self.viewport
    }
}

impl DerefMut for Session {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.viewport
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.restore_on_drop {
            let _ = restore();
        }
    }
}
