//! Platform-independent core of HDRSnap (tone mapping, encoders, settings,
//! hotkey parsing). Kept separate so it can be unit tested anywhere.

pub mod audio;
pub mod encoders;
pub mod hotkey;
pub mod imageio;
pub mod settings;
pub mod tonemap;
