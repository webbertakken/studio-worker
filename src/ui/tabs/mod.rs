//! Per-tab modules.  Each module exposes a pure-data view model
//! (testable without egui) and a thin `render` function that draws
//! the view model into an `egui::Ui`.

pub mod about;
pub mod config;
pub mod jobs;
pub mod logs;
pub mod status;
