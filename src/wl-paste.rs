#![no_std]
#![no_main]

use rustclip::mime::{self, MimeType};
use rustclip::state::{Action, State};
use wllib::cli;
use wllib::dispatch::dispatch_once;
use wllib::io::write_stderr;
use wllib::protocols::zwlr_data_control_manager_v1;
use wllib::registry::crawl;
use wllib::transport::Connection;
use wllib::wire::Message;

#[link(name = "c", kind = "static")]
unsafe extern "C" {}

unsafe extern "C" {
  static optarg: *const libc::c_char;
  static mut optind: libc::c_int;
}

const OPTSTRING: *const i8 = c"plnt:".as_ptr();

const LONGOPTS: [cli::LongOption; 6] = [
  cli::LongOption::new(c"use-primary", cli::NO_ARGUMENT, 'p'),
  cli::LongOption::new(c"primary", cli::NO_ARGUMENT, 'p'),
  cli::LongOption::new(c"list-types", cli::NO_ARGUMENT, 'l'),
  cli::LongOption::new(c"no-newline", cli::NO_ARGUMENT, 'n'),
  cli::LongOption::new(c"type", cli::REQUIRED_ARGUMENT, 't'),
  cli::LONG_OPTION_TERMINATOR,
];

#[unsafe(no_mangle)]
pub unsafe extern "C" fn main(argc: isize, argv: *const *mut libc::c_char) -> libc::c_int {
  let mut use_primary = false;
  let mut list_types = false;
  let mut no_newline = false;
  let mut wanted_mime: Option<MimeType> = None;
  let mut longindex = 0;
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
      'l' => list_types = true,
      'n' => no_newline = true,
      't' if !unsafe { optarg.is_null() } => {
        wanted_mime = Some(MimeType::from(unsafe { core::ffi::CStr::from_ptr(optarg) }));
      }
      _ => (),
    }
  }

  let inferred_mime = if wanted_mime.is_none() {
    if let Some(stdout_path) = mime::path_for_fd(1) {
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
      unsafe { libc::exit(1) };
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
    unsafe { libc::exit(1) };
  }

  if state.global.manager_id == 0 {
    write_stderr("[wl-paste]: compositor does not support wlr-data-control\n");
    unsafe { libc::exit(1) };
  }
  if state.global.seat_id == 0 {
    write_stderr("[wl-paste]: no wl_seat found\n");
    unsafe { libc::exit(1) };
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
      unsafe { libc::exit(1) };
    }
    if state.had_empty_selection {
      write_stderr("[wl-paste]: nothing is currently copied\n");
      unsafe { libc::exit(1) };
    }
  }
}
