//! The `garibaldb` client.
//!
//! The binary is a thin shell over these modules. Everything that decides
//! something lives here, so a test reaches it without a terminal.

pub mod args;
pub mod conn;
pub mod render;
pub mod repl;
