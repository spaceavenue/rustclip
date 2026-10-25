#![no_std]
#![no_main]
#![allow(clippy::result_large_err)]

use core::ffi::{c_char, c_int};

use rustclip::cli::{Arg, ArgParser, get_args_iter};
use rustclip::mime::{self, MimeType};
use rustclip::state::{Action, State};
use rustix::stdio;
use wllib::dispatch::dispatch_once;
use wllib::io::write_stderr;
use wllib::protocols::zwlr_data_control_manager_v1;
use wllib::registry::crawl;
use wllib::transport::Connection;
use wllib::wire::Message;

#[link(name = "c", kind = "static")]
unsafe extern "C" {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn main(argc: isize, argv: *const *mut c_char) -> c_int {
  let mut use_primary = false;
  let mut list_types = false;
  let mut no_newline = false;
  let mut wanted_mime: Option<MimeType> = None;

  let args_iter = unsafe { get_args_iter(if argc < 0 { 0 } else { argc as usize }, argv) };
  let mut parser = ArgParser::new(args_iter);
  while let Some(arg) = parser.next_arg() {
    match arg {
      Arg::Short('p') | Arg::Long("use-primary") | Arg::Long("primary") => use_primary = true,
      Arg::Short('l') | Arg::Long("list-types") => list_types = true,
      Arg::Short('n') | Arg::Long("no-newline") => no_newline = true,
      Arg::Short('t') | Arg::Long("type") => {
        wanted_mime = parser.next_val().map(MimeType::from);
      }
      _ => (),
    }
  }

  let inferred_mime = if wanted_mime.is_none() {
    if let Some(stdout_path) = mime::path_for_fd(unsafe { stdio::stdout() }) {
      mime::infer_from_name(stdout_path.as_str())
    } else {
      None
    }
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
  let action = if list_types {
    Action::ListTypes
  } else {
    Action::PrintAndExit
  };
  let mut state = State::init(use_primary, wanted_mime, inferred_mime, no_newline, action);

  if let Err(e) = crawl(&mut conn, &mut state) {
    e.write_diagnostic();
    return 1;
  }

  if state.global.manager_id == 0 {
    write_stderr("[wl-paste]: compositor does not support wlr-data-control\n");
    return 1;
  }
  if state.global.seat_id == 0 {
    write_stderr("[wl-paste]: no wl_seat found\n");
    return 1;
  }

  let device_id = conn.alloc_id();
  let mut msg = Message::new(
    state.global.manager_id,
    zwlr_data_control_manager_v1::request::GET_DATA_DEVICE,
  );

  msg.write_u32(device_id);
  msg.write_u32(state.global.seat_id);
  conn.send_logged(&msg, None);
  state.device_id = device_id;

  // main wayland event dispatch loop
  loop {
    if let Err(e) = dispatch_once(&mut conn, &mut state) {
      e.write_diagnostic();
      return 1;
    }
    if state.had_empty_selection {
      write_stderr("[wl-paste]: nothing is currently copied\n");
      return 1;
    }
  }
}
