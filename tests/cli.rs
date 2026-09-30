#![cfg(all(unix, not(target_os = "macos")))]

pub mod support;
#[path = "support/command.rs"]
pub mod test_command;

#[path = "cli/mod.rs"]
mod cases;
