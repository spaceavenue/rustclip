#![no_std]
#![no_main]

// `wl-copy` doesn't use `wllib::dispatch`/`EventHandler` for its runtime loop, because
// `zwlr_data_control_source_v1::send` hands this process a file descriptor as ancillary data which
// `dispatch_once`'s loop has no way to carry back to a handler. it wasn't designed around any event
// needing more than the plain argument bytes. so, this binary runs its own loop directly on top of
// `Connection::recv_with_fd`, and only uses `crawl`/`GlobalHandler` for the registry phase.

use rustclip::error::AppError;
use rustclip::mime::{self, GENERIC_TEXT_OFFERS, MimeType};
use rustix::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use rustix::fs::{MemfdFlags, SeekFrom, ftruncate, memfd_create, seek};
use rustix::io::read;
use rustix::stdio::stdin;
use wllib::cli;
use wllib::dispatch::EventHandler;
use wllib::error::{SysError, SysResultExt, WireError};
use wllib::fmt_lite::StringOnStack;
use wllib::io::{write_fd, write_stderr};
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
          Err(e) => e.write_diagnostic(),
        }
      }
      "zwlr_data_control_manager_v1" => {
        let id = conn.alloc_id();
        match bind(conn, name, interface, clamp_version(2, version), id) {
          Ok(()) => self.global.manager_id = id,
          Err(e) => e.write_diagnostic(),
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

#[link(name = "c", kind = "static")]
unsafe extern "C" {}

unsafe extern "C" {
  static optarg: *const libc::c_char;
  static mut optind: libc::c_int;
}

const OPTSTRING: *const i8 = c"pont:f".as_ptr();

const LONGOPTS: [cli::LongOption; 6] = [
  cli::LongOption::new(c"primary", cli::NO_ARGUMENT, 'p'),
  cli::LongOption::new(c"paste-once", cli::NO_ARGUMENT, 'o'),
  cli::LongOption::new(c"trim-newline", cli::NO_ARGUMENT, 'n'),
  cli::LongOption::new(c"foreground", cli::NO_ARGUMENT, 'f'),
  cli::LongOption::new(c"type", cli::REQUIRED_ARGUMENT, 't'),
  cli::LONG_OPTION_TERMINATOR,
];

#[unsafe(no_mangle)]
pub unsafe extern "C" fn main(argc: isize, argv: *const *mut libc::c_char) -> libc::c_int {
  let mut use_primary = false;
  let mut paste_once = false;
  let mut trim_newline = false;
  let mut foreground = false;
  let mut wanted_mime: Option<MimeType> = None;
  let mut longindex: libc::c_int = 0;
  unsafe { optind = 1 };

  loop {
    let c = unsafe {
      cli::getopt_long(
        argc as _,
        argv,
        OPTSTRING,
        LONGOPTS.as_ptr(),
        &raw mut longindex,
      )
    };
    if c == -1 {
      break;
    }
    match c as u8 as char {
      'p' => use_primary = true,
      'o' => paste_once = true,
      'n' => trim_newline = true,
      'f' => foreground = true,
      't' if !unsafe { optarg.is_null() } => {
        wanted_mime = Some(MimeType::from(unsafe { core::ffi::CStr::from_ptr(optarg) }));
      }
      _ => (),
    }
  }

  // Create an anonymous file descriptor in RAM to hold the clipboard data
  let optind_val = unsafe { optind };
  let memfd = match memfd_create(c"wl-clip", MemfdFlags::CLOEXEC) {
    Ok(mfd) => mfd,
    Err(e) => {
      AppError::Sys(SysError::from_errno("memfd_create", e)).write_diagnostic();
      return 1;
    }
  };

  let is_stdin = optind_val >= argc as libc::c_int;
  if is_stdin {
    if let Err(e) = write_stdin_to_fd(&memfd) {
      e.write_diagnostic();
      return 1;
    }
  } else if let Err(e) = write_args_to_fd(&memfd, argv, optind_val as usize, argc.cast_unsigned()) {
    e.write_diagnostic();
    return 1;
  }

  if trim_newline {
    trim_trailing_newline(&memfd);
  }

  // only stdin content is inferred from. an explicit argument list is already text the user
  // typed on the command line, and `-t` always takes priority over either.
  let inferred_mime = if is_stdin && wanted_mime.is_none() {
    mime::infer_from_fd(&memfd)
  } else {
    None
  };

  let mut conn = match Connection::connect() {
    Ok(c) => c,
    Err(e) => {
      e.write_diagnostic();
      return 1;
    }
  };

  let mut state = State {
    global: Global::default(),
  };
  if let Err(e) = crawl(&mut conn, &mut state) {
    e.write_diagnostic();
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
    let pid = unsafe { libc::fork() };
    if pid < 0 {
      write_stderr("[wl-copy]: failed to fork process\n");
      return 1;
    } else if pid > 0 {
      return 0;
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
        e.write_diagnostic();
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
    let _ = seek(&fd, SeekFrom::Start(size - 1));
  }
  if let Ok(1) = read(&fd, &mut last_byte)
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
  // if close {
  //   unsafe { libc::close(target_fd) };
  // }
  Ok(())
}

/// writes whatever was received from stdin to the given fd
fn write_stdin_to_fd(fd: &OwnedFd) -> Result<(), AppError> {
  let stdin = unsafe { stdin() };
  write_content_to_fd(stdin, fd, false)
}

/// writes argv to the given fd
fn write_args_to_fd(
  fd: &OwnedFd,
  argv: *const *mut libc::c_char,
  start: usize,
  argc: usize,
) -> Result<(), AppError> {
  for i in start..argc {
    if i > start {
      write_fd(fd, b" ", Some(1)).map_err(AppError::Sys)?;
    }
    let arg = unsafe { *argv.add(i) };
    let bytes = unsafe { core::ffi::CStr::from_ptr(arg) }.to_bytes();
    write_fd(fd, bytes, None).map_err(AppError::Sys)?;
  }
  Ok(())
}
