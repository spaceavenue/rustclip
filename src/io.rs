//! File-descriptor write helpers.

use rustix::fd::AsFd;
use rustix::io::{Errno, write};
use rustix::stdio;
use wllib::error::SysError;

/// Writes `msg` to the file descriptor. Retries on partial write and `EINTR` until all bytes have
/// been written or a real error occurs.
///
/// `count` optionally caps how many bytes are written. `None` implies writing all bytes.
pub fn write_fd<Fd: AsFd>(
  fd: Fd,
  msg: impl AsRef<[u8]>,
  count: Option<usize>,
) -> Result<(), SysError> {
  let msg = msg.as_ref();
  let len = match count {
    Some(c) => c.min(msg.len()),
    None => msg.len(),
  };

  let mut msg = &msg[..len];
  while !msg.is_empty() {
    let res = match write(fd.as_fd(), msg) {
      Ok(0) => break,
      Ok(n) => n,
      Err(Errno::INTR) => continue,
      Err(err) => return Err(SysError::from_errno("write", err)),
    };
    msg = &msg[res..];
  }
  Ok(())
}

/// Writes the buffer to stderr.
pub fn write_stderr(msg: impl AsRef<[u8]>) {
  let fd = unsafe { stdio::stderr() };
  let _ = write_fd(fd, msg, None);
}

/// Writes the buffer to stdout.
pub fn write_stdout(msg: impl AsRef<[u8]>) {
  let fd = unsafe { stdio::stdout() };
  let _ = write_fd(fd, msg, None);
}
