#![no_std]
#![no_main]
#![allow(clippy::result_large_err)]

// `wl-copy` doesn't use `wllib::dispatch`/`EventHandler` for its runtime loop, because
// `zwlr_data_control_source_v1::send` hands this process a file descriptor as ancillary data which
// `dispatch_once`'s loop has no way to carry back to a handler. it wasn't designed around any event
// needing more than the plain argument bytes. so, this binary runs its own loop directly on top of
// `Connection::recv_with_fd`, and only uses `crawl`/`GlobalHandler` for the registry phase.

use core::ffi::{c_char, c_int};

use rustclip::cli::{Arg, ArgParser, get_args_iter};
use rustclip::mime::{self, GENERIC_TEXT_OFFERS, MimeType, infer_from_fd};
use rustclip::syscall::{Fork, fork};
use rustix::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use rustix::fs::{MemfdFlags, SeekFrom, ftruncate, memfd_create, seek};
use rustix::io::read;
use rustix::stdio::stdin;
use rustclip::error::{AppError, ConnectionExt, write_wire_diagnostic};
use rustclip::io::{write_fd, write_stderr};
use wllib::dispatch::EventHandler;
use wllib::error::{SysError, SysResultExt, WireError};
use wllib::fmt_lite::StringOnStack;
use wllib::protocols::{
  zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_source_v1,
};
use wllib::registry::{GlobalHandler, bind, clamp_version, crawl};
use wllib::transport::Connection;
use wllib::wire::{Message, parse_header};

#[derive(Default)]
struct Global {
  seat_id: u32,
  manager_id: u32,
}

struct State {
  global: Global,
}

impl GlobalHandler for State {
  fn on_global(&mut self, conn: &mut Connection, name: u32, interface: &str, version: u32) {
    match interface {
      "wl_seat" => {
        if self.global.seat_id != 0 {
          return;
        }
        let id = conn.alloc_id();
        match bind(conn, name, interface, clamp_version(1, version), id) {
          Ok(()) => self.global.seat_id = id,
          Err(e) => write_wire_diagnostic("wl-copy", &e),
        }
      }
      "zwlr_data_control_manager_v1" => {
        let id = conn.alloc_id();
        match bind(conn, name, interface, clamp_version(2, version), id) {
          Ok(()) => self.global.manager_id = id,
          Err(e) => write_wire_diagnostic("wl-copy", &e),
        }
      }
      _ => (),
    }
  }
}

// stub for satisfying `crawl`'s EventHandler trait bound.
impl EventHandler for State {
  fn handle_event(&mut self, _conn: &mut Connection, _sender: u32, _opcode: u16, _data: &[u8]) {}
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
        if let Err(e) = write_args_to_fd(&memfd, val) {
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
  };
  if let Err(e) = crawl(&mut conn, &mut state) {
    write_wire_diagnostic("wl-copy", &e);
    return 1;
  }

  if state.global.manager_id == 0 {
    write_stderr("[wl-copy]: compositor does not support wlr-data-control\n");
    return 1;
  }
  if state.global.seat_id == 0 {
    write_stderr("[wl-copy]: no wl_seat found\n");
    return 1;
  }
  let manager_id = state.global.manager_id;
  let seat_id = state.global.seat_id;

  let source_id = conn.alloc_id();
  let mut create_msg = Message::new(
    manager_id,
    zwlr_data_control_manager_v1::request::CREATE_DATA_SOURCE,
  );
  create_msg.write_u32(source_id);
  conn.send_logged(&create_msg, None);

  let primary_mime: Option<&str> = wanted_mime
    .as_ref()
    .map(StringOnStack::as_str)
    .or(inferred_mime.as_ref().map(StringOnStack::as_str));

  if let Some(m) = primary_mime {
    let mut offer_msg = Message::new(source_id, zwlr_data_control_source_v1::request::OFFER);
    offer_msg.write_str(m);
    conn.send_logged(&offer_msg, None);
  }
  // can unwrap() here since we are already checking if it's None. if that check doesnt pass it is
  // bound to be Some.
  if primary_mime.is_none() || mime::is_text_mime(primary_mime.unwrap()) {
    for &pref in &GENERIC_TEXT_OFFERS {
      if primary_mime != Some(pref) {
        let mut offer_msg = Message::new(source_id, zwlr_data_control_source_v1::request::OFFER);
        offer_msg.write_str(pref);
        conn.send_logged(&offer_msg, None);
      }
    }
  }

  let device_id = conn.alloc_id();
  let mut device_msg = Message::new(
    manager_id,
    zwlr_data_control_manager_v1::request::GET_DATA_DEVICE,
  );
  device_msg.write_u32(device_id);
  device_msg.write_u32(seat_id);
  conn.send_logged(&device_msg, None);

  let set_selection_opcode = if use_primary {
    zwlr_data_control_device_v1::request::SET_PRIMARY_SELECTION
  } else {
    zwlr_data_control_device_v1::request::SET_SELECTION
  };
  let mut select_msg = Message::new(device_id, set_selection_opcode);
  select_msg.write_u32(source_id);
  conn.send_logged(&select_msg, None);

  if !foreground {
    match fork().syserr("fork") {
      Ok(Fork::Child) => {}
      Ok(Fork::Parent(_)) => return 0,
      Err(e) => {
        write_wire_diagnostic("wl-copy", &e);
        return 1;
      }
    }
  }

  // serve `send`/`cancelled` events on the child's source until someone else takes over the
  // selection (or with -o, until we've served exactly one request).
  loop {
    let mut buf = [0u8; 4096];
    let (result, mut fd) = conn.recv_with_fd(&mut buf);
    let data = match result {
      Ok(items) => items,
      Err(WireError::ConnectionClosed) => break,
      Err(e) => {
        write_wire_diagnostic("wl-copy", &e);
        break;
      }
    };

    let mut idx = 0;
    while let Some(header) = parse_header(data, idx) {
      if header.sender == source_id {
        match header.opcode {
          zwlr_data_control_source_v1::event::SEND => {
            if let Some(target_fd) = fd.take() {
              let _ = write_content_to_fd(memfd.as_fd(), &target_fd, true);
              if paste_once {
                return 0;
              }
            } else {
              write_stderr("[wl-copy]: got a send event with no accompanying fd, ignoring\n");
            }
          }
          zwlr_data_control_source_v1::event::CANCELLED => {
            return 0;
          }
          _ => (),
        }
      }
      idx += header.size as usize;
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
fn write_content_to_fd(
  source_fd: BorrowedFd,
  target_fd: &OwnedFd,
  _close: bool,
) -> Result<(), AppError> {
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
  write_content_to_fd(stdin, fd, false)
}

fn write_args_to_fd(fd: &OwnedFd, val: &str) -> Result<(), AppError> {
  write_fd(fd, b" ", Some(1)).map_err(AppError::Sys)?;
  write_fd(fd, val.as_bytes(), None).map_err(AppError::Sys)?;
  Ok(())
}
