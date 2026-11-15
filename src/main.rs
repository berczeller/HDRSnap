// No console window in release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(windows)]
macro_rules! log {
    ($($t:tt)*) => { $crate::win::util::log_write(&format!($($t)*)) };
}
#[cfg(windows)]
pub(crate) use log;

#[cfg(windows)]
mod ui;
#[cfg(windows)]
mod win;

fn main() {
    #[cfg(windows)]
    win::app::run();
    #[cfg(not(windows))]
    eprintln!("HDRSnap is a Windows application.");
}
