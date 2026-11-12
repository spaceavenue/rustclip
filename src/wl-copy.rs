#![no_std]
#![no_main]
#![allow(clippy::result_large_err)]

use core::ffi::{c_char, c_int};

use rustclip::cli::{Arg, ArgParser, get_args_iter};
use rustclip::error::{AppError, write_wire_diagnostic};
use rustclip::fmt_lite::StringOnStack;
use rustclip::io::{write_fd, write_stderr};
use rustclip::mime::{self, GENERIC_TEXT_OFFERS, MimeType, infer_from_fd};
use rustclip::state::Global;
use rustclip::syscall::{Fork, fork};
use rustix::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use rustix::fs::{MemfdFlags, SeekFrom, ftruncate, memfd_create, seek};
use rustix::io::read;
use rustix::stdio::stdin;
use wllib::dispatch::{EventHandler, dispatch_once};
use wllib::error::{SysError, SysResultExt, WireError};
use wllib::protocols::{
  zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_source_v1,
};
use wllib::registry::{GlobalHandler, crawl};
use wllib::transport::{Connection, Ctx};
use wllib::wire::{Message, Writer};

struct State {
  global: Global,
  memfd: OwnedFd,
  source_id: u32,
  paste_once: bool,
  finished: bool,
}

impl GlobalHandler for State {
  fn on_global(
    &mut self,
    cx: &mut Ctx<'_>,
    name: u32,
    interface: &str,
    version: u32,
  ) -> Result<(), WireError> {
    self.global.bind_global(cx, name, interface, version)
  }
}

impl EventHandler for State {
  // serve `send`/`cancelled` events.
  fn handle_event(&mut self, cx: &mut Ctx<'_>, msg: Message<'_>) -> Result<(), WireError> {
    if msg.header.object_id != self.source_id {
      return Ok(());
    }
    match msg.header.opcode {
      zwlr_data_control_source_v1::event::SEND => {
        if let Some(target_fd) = cx.pop_fd() {
          let _ = write_content_to_fd(self.memfd.as_fd(), &target_fd);
          if self.paste_once {
            self.finished = true;
          }
        } else {
          write_stderr("[wl-copy]: got a send event with no accompanying fd, ignoring\n");
        }
      }
      zwlr_data_control_source_v1::event::CANCELLED => self.finished = true,
      _ => (),
    }
    Ok(())
  }
}

fn offer<const N: usize>(
  conn: &Connection<N>,
  source_id: u32,
  mime: &str,
) -> Result<(), WireError> {
  let mut w = Writer::<192>::new(source_id, zwlr_data_control_source_v1::request::OFFER);
  w.str(mime);
  conn.send(w.finish()?, &[])
}

// create the source, install it as the selection, and unless `foreground`, fork into the
// background. returns `Ok(false)` in the parent after forking, where there's nothing left to do.
fn setup<const N: usize>(
  conn: &mut Connection<N>,
  state: &mut State,
  primary_mime: Option<&str>,
  use_primary: bool,
  foreground: bool,
) -> Result<bool, AppError> {
  let manager_id = state.global.manager_id;

  let source_id = conn.alloc_id()?;
  let mut w = Writer::<16>::new(
    manager_id,
    zwlr_data_control_manager_v1::request::CREATE_DATA_SOURCE,
  );
  w.u32(source_id);
  conn.send(w.finish()?, &[])?;
  state.source_id = source_id;

  if let Some(m) = primary_mime {
    offer(conn, source_id, m)?;
  }
  if primary_mime.is_none_or(mime::is_text_mime) {
    for &pref in &GENERIC_TEXT_OFFERS {
      if primary_mime != Some(pref) {
        offer(conn, source_id, pref)?;
      }
    }
  }

  let device_id = state.global.create_device(conn)?;

  let set_selection_opcode = if use_primary {
    zwlr_data_control_device_v1::request::SET_PRIMARY_SELECTION
  } else {
    zwlr_data_control_device_v1::request::SET_SELECTION
  };
  let mut w = Writer::<16>::new(device_id, set_selection_opcode);
  w.u32(source_id);
  conn.send(w.finish()?, &[])?;

  if !foreground {
    return match fork().syserr("fork")? {
      Fork::Child => Ok(true),
      Fork::Parent(_) => Ok(false),
    };
  }
  Ok(true)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn main(argc: isize, argv: *const *mut c_char) -> c_int {
  let mut use_primary = false;
  let mut paste_once = false;
  let mut trim_newline = false;
  let mut foreground = false;
  let mut wanted_mime: Option<MimeType> = None;
  let mut is_stdin = true;

  if argc <= 0 || argv.is_null() {
    return 1;
  }

  let memfd = match memfd_create(c"wl-clip", MemfdFlags::CLOEXEC) {
    Ok(mfd) => mfd,
    Err(e) => {
      AppError::Sys(SysError::from_errno("memfd_create", e)).write_diagnostic();
      return 1;
    }
  };

  let args_iter = unsafe { get_args_iter(argc as usize, argv) };
  let mut parser = ArgParser::new(args_iter);
  while let Some(arg) = parser.next_arg() {
    match arg {
      Arg::Short('p') | Arg::Long("primary") => use_primary = true,
      Arg::Short('o') | Arg::Long("paste-once") => paste_once = true,
      Arg::Short('n') | Arg::Long("trim-newline") => trim_newline = true,
      Arg::Short('f') | Arg::Long("foreground") => foreground = true,
      Arg::Short('t') | Arg::Long("type") => {
        wanted_mime = parser.next_val().map(MimeType::from);
      }
      Arg::Value(val) => {
        if let Err(e) = write_args_to_fd(&memfd, val, is_stdin) {
          e.write_diagnostic();
          return 1;
        }
        is_stdin = false;
      }
      _ => (),
    }
  }

  if is_stdin && let Err(e) = write_stdin_to_fd(&memfd) {
    e.write_diagnostic();
    return 1;
  }

  if trim_newline {
    trim_trailing_newline(&memfd);
  }

  // only stdin content is inferred from. an explicit argument list is already text the user
  // typed on the command line, and `-t` always takes priority over either.
  let inferred_mime = if is_stdin && wanted_mime.is_none() {
    infer_from_fd(&memfd)
  } else {
    None
  };

  let mut conn = match rustclip::connect_wayland() {
    Ok(c) => c,
    Err(e) => {
      write_wire_diagnostic("wl-copy", &e);
      return 1;
    }
  };

  let mut state = State {
    global: Global::default(),
    memfd,
    source_id: 0,
    paste_once,
    finished: false,
  };
  if let Err(e) = crawl(&mut conn, &mut state) {
    write_wire_diagnostic("wl-copy", &e);
    return 1;
  }

  if !state.global.require("wl-copy") {
    return 1;
  }

  let primary_mime: Option<&str> = wanted_mime
    .as_ref()
    .map(StringOnStack::as_str)
    .or(inferred_mime.as_ref().map(StringOnStack::as_str));

  match setup(&mut conn, &mut state, primary_mime, use_primary, foreground) {
    Ok(true) => {}
    Ok(false) => return 0,
    Err(e) => {
      e.write_diagnostic();
      return 1;
    }
  }

  while !state.finished {
    match dispatch_once(&mut conn, &mut state) {
      Ok(()) => {}
      Err(WireError::ConnectionClosed) => break,
      Err(e) => {
        write_wire_diagnostic("wl-copy", &e);
        break;
      }
    }
  }
  0
}

/// trims a single trailing '\n' in the given fd.
fn trim_trailing_newline(fd: &OwnedFd) {
  let Ok(size) = seek(fd, SeekFrom::End(0)).syserr("seek") else {
    return;
  };
  let mut last_byte = [0u8; 1];
  if size > 0 {
    let _ = seek(fd, SeekFrom::Start(size - 1));
  }
  if let Ok(1) = read(fd, &mut last_byte)
    && last_byte[0] == b'\n'
  {
    let Ok(_) = ftruncate(fd, size - 1) else {
      return;
    };
  };
}

/// writes data from `source_fd` to `target_fd`
fn write_content_to_fd(source_fd: BorrowedFd, target_fd: &OwnedFd) -> Result<(), AppError> {
  // if not stdin, reset the file offset of the fd to the beginning before copying.
  if source_fd.as_raw_fd() != 0 {
    seek(source_fd, SeekFrom::Start(0)).syserr("seek")?;
  }
  let mut chunk = [0u8; 4096];
  loop {
    let n = read(source_fd, &mut chunk).syserr("read")?;
    if n == 0 {
      break;
    }
    write_fd(target_fd, &chunk[..n], None).map_err(AppError::Sys)?;
  }
  Ok(())
}

/// writes whatever was received from stdin to the given fd
fn write_stdin_to_fd(fd: &OwnedFd) -> Result<(), AppError> {
  let stdin = unsafe { stdin() };
  write_content_to_fd(stdin, fd)
}

/// writes arguments to the fd. `first` suppresses the space (' ') before the first arg.
fn write_args_to_fd(fd: &OwnedFd, val: &str, first: bool) -> Result<(), AppError> {
  if !first {
    write_fd(fd, b" ", None).map_err(AppError::Sys)?;
  }
  write_fd(fd, val.as_bytes(), None).map_err(AppError::Sys)?;
  Ok(())
}
