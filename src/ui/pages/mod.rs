//! One module per page on the rail.  Each exposes a pure-data view model
//! (testable without egui) and a thin `render` that draws it.

pub mod config;
pub mod jobs;
pub mod logs;
pub mod models;
pub mod worker;
