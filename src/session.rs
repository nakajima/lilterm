use std::io;
use std::io::IsTerminal;
use std::io::Stdout;
use std::io::stdin;
use std::io::stdout;
use std::ops::Deref;
use std::ops::DerefMut;
use std::panic;
use std::sync::Once;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use crossterm::event::DisableBracketedPaste;
use crossterm::event::DisableFocusChange;
use crossterm::event::EnableBracketedPaste;
use crossterm::event::EnableFocusChange;
use crossterm::execute;
use crossterm::terminal::DisableLineWrap;
use crossterm::terminal::EnableLineWrap;
use crossterm::terminal::disable_raw_mode;
use crossterm::terminal::enable_raw_mode;
use ratatui::backend::CrosstermBackend;

use crate::inline::InlineViewport;
use crate::terminal::Terminal;

pub type CrosstermTerminal = Terminal<CrosstermBackend<Stdout>>;
pub type CrosstermInlineViewport = InlineViewport<CrosstermBackend<Stdout>>;

static RESTORE_HOOKS: Once = Once::new();
static TERMINAL_NEEDS_RESTORE: AtomicBool = AtomicBool::new(false);

pub fn set_modes() -> io::Result<()> {
    install_restore_hooks();
    TERMINAL_NEEDS_RESTORE.store(true, Ordering::SeqCst);

    let result = (|| {
        // Live viewport rows are unstable UI, not logical terminal output. If they
        // are marked as soft-wrapped, terminal resize reflow can leak old viewport
        // borders into scrollback.
        execute!(stdout(), EnableBracketedPaste, DisableLineWrap)?;
        enable_raw_mode()?;
        let _ = execute!(stdout(), EnableFocusChange);
        Ok(())
    })();

    if result.is_err() {
        let _ = restore();
    }

    result
}

pub fn restore() -> io::Result<()> {
    if TERMINAL_NEEDS_RESTORE
        .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Ok(());
    }

    let result = restore_modes();
    if result.is_err() {
        TERMINAL_NEEDS_RESTORE.store(true, Ordering::SeqCst);
    }
    result
}

fn install_restore_hooks() {
    RESTORE_HOOKS.call_once(|| {
        let previous_hook = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            emergency_restore();
            previous_hook(info);
        }));

        install_exit_restore_hook();
    });
}

#[cfg(unix)]
fn install_exit_restore_hook() {
    unsafe {
        let _ = libc::atexit(emergency_restore_atexit);
    }
}

#[cfg(not(unix))]
fn install_exit_restore_hook() {}

#[cfg(unix)]
extern "C" fn emergency_restore_atexit() {
    emergency_restore();
}

fn emergency_restore() {
    let _ = restore();
}

fn restore_modes() -> io::Result<()> {
    let bracketed_paste_result = execute!(stdout(), DisableBracketedPaste, EnableLineWrap);
    let _ = execute!(stdout(), DisableFocusChange);
    let raw_mode_result = disable_raw_mode();
    let cursor_result = execute!(stdout(), crossterm::cursor::Show);

    bracketed_paste_result?;
    raw_mode_result?;
    cursor_result?;
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
        let terminal = match Terminal::with_options(backend) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = restore();
                return Err(error);
            }
        };
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
        let leave_result = self.viewport.terminal_mut().leave_viewport();
        let restore_result = restore();
        if restore_result.is_ok() {
            self.restore_on_drop = false;
        }

        leave_result.and(restore_result)
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
            let _ = self.viewport.terminal_mut().leave_viewport();
            let _ = restore();
        }
    }
}
