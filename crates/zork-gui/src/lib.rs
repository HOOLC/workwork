//! zork-gui: a GPUI desktop client for zork-gateway's local IM entry.
//!
//! The transcript contains only gateway-delivered user/assistant messages.
//! Agent tools, waits, deltas, and transcript text remain internal; activity
//! reaches the UI separately through gateway-projected status events.

pub mod api;
pub mod assets;
pub mod automation;
pub mod components;
pub mod design;
pub mod transcript;
pub mod views;
pub mod window_chrome;
