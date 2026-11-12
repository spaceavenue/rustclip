#![no_std]

pub mod cli;
pub mod entry;
pub mod env;
pub mod error;
pub mod fmt_lite;
pub mod io;
pub mod mime;
pub mod state;
pub mod syscall;

pub use env::connect_wayland;
pub use error::{ConnectionExt, CtxExt, write_sys_diagnostic, write_wire_diagnostic};
pub use syscall::{execvp, execvpe, exit, fork, signal};
