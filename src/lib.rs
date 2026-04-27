pub mod history;
pub mod inline;
pub mod session;
pub mod terminal;
pub mod trace;

pub use history::insert_history_lines;
pub use inline::InlineViewport;
pub use inline::LayoutFrame;
pub use inline::Region;
pub use inline::ScrollbackTailState;
pub use session::CrosstermInlineViewport;
pub use session::CrosstermTerminal;
pub use session::Session;
pub use session::init;
pub use session::restore;
pub use session::set_modes;
pub use terminal::Frame;
pub use terminal::Terminal;

#[cfg(test)]
mod test_backend;
