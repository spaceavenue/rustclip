use rustix::fd::OwnedFd;
use wllib::error::{SysError, WireError};
use wllib::fmt_lite::StringOnStack;
use wllib::transport::Connection;
use wllib::wire::Message;

use crate::io::write_stderr;

/// Write a wire error to stderr tagged with `prefix`.
pub fn write_wire_diagnostic(prefix: &str, err: &WireError) {
  match err {
    WireError::Sys(e) => write_sys_diagnostic(prefix, e),
    WireError::Protocol(p) => {
      let mut s = StringOnStack::<128>::new();
      s.push(prefix)
        .push(" wayland protocol error: object ")
        .push(p.object_id)
        .push(", code ")
        .push(p.code)
        .push(", message: ")
        .push(p.message.as_str())
        .push("\n");
      write_stderr(s);
    }
    WireError::InvalidPath => {
      let mut s = StringOnStack::<128>::new();
      s.push(prefix)
        .push(" WAYLAND_DISPLAY or XDG_RUNTIME_DIR not set (or invalid socket path)\n");
      write_stderr(s);
    }
    WireError::BufferTooSmall => {
      let mut s = StringOnStack::<128>::new();
      s.push(prefix)
        .push(" receive buffer too small for wayland framing\n");
      write_stderr(s);
    }
    WireError::ConnectionClosed => {
      let mut s = StringOnStack::<128>::new();
      s.push(prefix).push(" connection closed by compositor\n");
      write_stderr(s);
    }
  }
}

/// Write a syscall error to stderr tagged with `prefix`.
pub fn write_sys_diagnostic(prefix: &str, err: &SysError) {
  let mut s = StringOnStack::<128>::new();
  s.push(prefix)
    .push(" syscall failed: ")
    .push(err.call)
    .push(" (")
    .push(err.errno)
    .push(")\n");
  write_stderr(s);
}

/// Extension trait on [`Connection`] for sending error logged messages.
pub trait ConnectionExt {
  fn send_logged(&self, msg: &Message, fd: Option<&OwnedFd>);
}

impl ConnectionExt for Connection {
  fn send_logged(&self, msg: &Message, fd: Option<&OwnedFd>) {
    if let Err(e) = self.send(msg, fd) {
      write_wire_diagnostic("[rustclip]", &e);
    }
  }
}

pub enum AppError {
  Wire(WireError),
  Sys(SysError),
  // no offer/selection currently available when data was requested
  NothingCopied,
  // the mime type requested with -t isn't offered by the current selection
  MimeNotAvailable,
  // stdout was redirected to a file and a mime type was inferred from its extension, but that
  // type isn't offered by the current selection. distinct from `MimeNotAvailable` so the
  // diagnostic can tell the user *why* a type was expected (they asked for it vs. it was
  // guessed).
  InferredMimeNotAvailable,
}

impl AppError {
  pub fn write_diagnostic(&self) {
    match self {
      AppError::Wire(e) => write_wire_diagnostic("[rustclip]", e),
      AppError::Sys(e) => write_sys_diagnostic("[rustclip]", e),
      AppError::NothingCopied => write_stderr("[rustclip]: nothing is currently copied\n"),
      AppError::MimeNotAvailable => {
        write_stderr("[rustclip]: requested mime type not offered by the current selection\n");
      }
      AppError::InferredMimeNotAvailable => {
        write_stderr(
          "[rustclip]: clipboard content is not available as the inferred output type\n",
        );
      }
    }
  }
}

impl From<WireError> for AppError {
  fn from(e: WireError) -> Self {
    AppError::Wire(e)
  }
}
