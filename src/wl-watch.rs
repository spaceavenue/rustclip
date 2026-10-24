#![no_std]
#![no_main]

use core::ffi::{c_char, c_int};

use rustclip::mime::MimeType;
use rustclip::state::{Action, State};
use wllib::cli;
use wllib::dispatch::dispatch_once;
use wllib::io::write_stderr;
use wllib::protocols::zwlr_data_control_manager_v1;
use wllib::registry::crawl;
use wllib::transport::Connection;
use wllib::wire::Message;

const SIGCHLD: c_int = 17;
const SIG_IGN: usize = 1;

#[link(name = "c", kind = "static")]
unsafe extern "C" {}

unsafe extern "C" {
  static optarg: *const c_char;
  static mut optind: c_int;
  fn signal(sig: c_int, handler: usize) -> usize;
}

const OPTSTRING: *const i8 = c"pt:".as_ptr();

const LONGOPTS: [cli::LongOption; 4] = [
  cli::LongOption::new(c"use-primary", cli::NO_ARGUMENT, 'p'),
  cli::LongOption::new(c"primary", cli::NO_ARGUMENT, 'p'),
  cli::LongOption::new(c"type", cli::REQUIRED_ARGUMENT, 't'),
  cli::LONG_OPTION_TERMINATOR,
];

#[unsafe(no_mangle)]
pub unsafe extern "C" fn main(argc: isize, argv: *const *mut c_char) -> c_int {
  let mut use_primary = false;
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
      't' if !unsafe { optarg.is_null() } => {
        wanted_mime = Some(MimeType::from(unsafe { core::ffi::CStr::from_ptr(optarg) }));
      }
      _ => (),
    }
  }

  let optind_val = unsafe { optind };
  if optind_val >= argc as c_int {
    write_stderr("Usage: wl-watch [-p|--use-primary] [-t|--type mime] <command> [args...]\n");
    return 1;
  }
  let command_argv = unsafe { argv.add(optind_val as usize) };
  let command_argc = (argc as c_int - optind_val) as usize;

  unsafe { signal(SIGCHLD, SIG_IGN) };

  let mut conn = match Connection::connect() {
    Ok(c) => c,
    Err(e) => {
      e.write_diagnostic();
      return 1;
    }
  };
  let action = Action::PipeToCommand {
    argv: command_argv,
    argc: command_argc,
  };
  let mut state = State::init(use_primary, wanted_mime, None, false, action);

  if let Err(e) = crawl(&mut conn, &mut state) {
    e.write_diagnostic();
    return 1;
  }

  if state.global.manager_id == 0 {
    write_stderr(b"[wl-watch]: compositor does not support wlr-data-control\n");
    return 1;
  }
  if state.global.seat_id == 0 {
    write_stderr(b"[wl-watch]: no wl_seat found\n");
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
  }
}
