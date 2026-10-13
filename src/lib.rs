//! Whole-disk file search for Windows: a fuzzy name index over every entry
//! on every fixed drive, kept live from change notifications, plus a
//! trigram content index for the text files in your folders. `Engine` runs
//! it all in-process; `server` wraps one in a daemon answering JSON lines on
//! a named pipe, which the `scry` command talks to.

pub mod content;
mod engine;
pub mod index;
pub mod live;
pub mod paths;
pub mod query;
pub mod server;
mod sys;
#[cfg(test)]
mod tests;
pub mod walk;
mod watch;

pub use content::{FileMatches, Grep, GrepResult};
pub use engine::{Engine, Found, Options, Status, default_dir, default_home, elevated, log};
pub use query::{GrepMode, Order, Query, human_age, human_size};
