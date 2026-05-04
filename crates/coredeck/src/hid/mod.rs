//! HID module - USB HID communication with Core Deck macropad (daemon-side)

mod commands;
mod device;
pub mod protocol;

#[cfg(target_os = "macos")]
mod hotplug_macos;

pub use device::HidManager;
