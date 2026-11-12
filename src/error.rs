//! Diagnostic logging and error rendering for applications.

use wllib::error::{SysError, WireError};
use wllib::transport::Connection;

use crate::fmt_lite::StringOnStack;
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
        .push(p.message())
        .push("\n");
      write_stderr(s);
    }
    WireError::InvalidPath => {
      write_stderr(prefix);
      write_stderr(" WAYLAND_DISPLAY or XDG_RUNTIME_DIR not set or socket path invalid\n");
    }
    WireError::BufferTooSmall => {
      write_stderr(prefix);
      write_stderr(" receive buffer too small for wayland framing\n");
    }
    WireError::ConnectionClosed => {
      write_stderr(prefix);
      write_stderr(" connection closed by compositor\n");
    }
    WireError::Parse(err) => {
      write_stderr(prefix);
      write_stderr(" wayland message parse error: ");
      match err {
        wllib::wire::MessageError::BadSize => write_stderr("bad message size\n"),
        wllib::wire::MessageError::Truncated => write_stderr("truncated message\n"),
        wllib::wire::MessageError::BadString => write_stderr("malformed string argument\n"),
        wllib::wire::MessageError::TrailingData => {
          write_stderr("unconsumed trailing message bytes\n")
        }
        wllib::wire::MessageError::Overflow => write_stderr("message writer buffer overflow\n"),
      };
    }
    WireError::MessageTooLarge => {
      write_stderr(prefix);
      write_stderr(" message exceeds receive buffer capacity\n");
    }
    WireError::TooManyFds => {
      write_stderr(prefix);
      write_stderr(" too many file descriptors passed\n");
    }
    WireError::FdOverflow => {
      write_stderr(prefix);
      write_stderr(" file descriptor queue overflow\n");
    }
    WireError::IdsExhausted => {
      write_stderr(prefix);
      write_stderr(" client object IDs exhausted\n");
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

use rustix::fd::BorrowedFd;
use wllib::transport::Ctx;

/// Extension trait on [`Connection`] for sending error logged messages.
pub trait ConnectionExt {
  fn send_logged(&self, bytes: &[u8], fds: &[BorrowedFd<'_>]);
}

impl<const N: usize> ConnectionExt for Connection<N> {
  fn send_logged(&self, bytes: &[u8], fds: &[BorrowedFd<'_>]) {
    if let Err(e) = self.send(bytes, fds) {
      write_wire_diagnostic("[wllib]", &e);
    }
  }
}

/// Extension trait on [`Ctx`] for sending error logged messages.
pub trait CtxExt {
  fn send_logged(&self, bytes: &[u8], fds: &[BorrowedFd<'_>]);
}

impl CtxExt for Ctx<'_> {
  fn send_logged(&self, bytes: &[u8], fds: &[BorrowedFd<'_>]) {
    if let Err(e) = self.send(bytes, fds) {
      write_wire_diagnostic("[wllib]", &e);
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

impl From<wllib::wire::MessageError> for AppError {
  fn from(e: wllib::wire::MessageError) -> Self {
    AppError::Wire(WireError::Parse(e))
  }
}

impl From<WireError> for AppError {
  fn from(e: WireError) -> Self {
    AppError::Wire(e)
  }
}
