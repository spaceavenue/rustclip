#![no_std]
#![no_main]
#![allow(clippy::result_large_err)]

use core::ffi::{c_char, c_int};

use rustclip::cli::{Arg, ArgParser, get_args_iter};
use rustclip::error::write_wire_diagnostic;
use rustclip::io::write_stderr;
use rustclip::mime::MimeType;
use rustclip::state::{Action, State};
use rustclip::syscall::signal::{SIG_IGN, SIGCHLD, signal};
use wllib::dispatch::dispatch_once;
use wllib::registry::crawl;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn main(argc: isize, argv: *const *mut c_char) -> c_int {
  let mut use_primary = false;
  let mut wanted_mime: Option<MimeType> = None;
  let mut cmd_idx = None;

  if argc <= 1 || argv.is_null() {
    write_stderr("Usage: wl-watch [-p|--use-primary] [-t|--type mime] <command> [args...]\n");
    return 1;
  }

  let args_iter = unsafe { get_args_iter(argc as usize, argv) };
  let mut parser = ArgParser::new(args_iter);
  while let Some(arg) = parser.next_arg() {
    match arg {
      Arg::Short('p') | Arg::Long("use-primary") | Arg::Long("primary") => {
        use_primary = true;
      }
      Arg::Short('t') | Arg::Long("type") => {
        wanted_mime = parser.next_val().map(MimeType::from);
      }
      Arg::Value(val) => {
        for i in 1..argc as usize {
          if unsafe { *argv.add(i) } as *const u8 == val.as_ptr() {
            cmd_idx = Some(i);
            break;
          }
        }
        break;
      }
      _ => (),
    }
  }

  let Some(cmd_idx) = cmd_idx else {
    write_stderr("Usage: wl-watch [-p|--use-primary] [-t|--type mime] <command> [args...]\n");
    return 1;
  };
  let command_argv = unsafe { argv.add(cmd_idx) };
  let command_argc = argc as usize - cmd_idx;

  let _ = unsafe { signal(SIGCHLD, SIG_IGN) };

  let mut conn = match rustclip::connect_wayland() {
    Ok(c) => c,
    Err(e) => {
      write_wire_diagnostic("wl-watch", &e);
      return 1;
    }
  };
  let action = Action::PipeToCommand {
    argv: command_argv,
    argc: command_argc,
  };
  let mut state = State::init(use_primary, wanted_mime, None, false, action);

  if let Err(e) = crawl(&mut conn, &mut state) {
    write_wire_diagnostic("wl-watch", &e);
    return 1;
  }

  if !state.global.require("wl-watch") {
    return 1;
  }

  if let Err(e) = state.create_device(&mut conn) {
    write_wire_diagnostic("wl-watch", &e);
    return 1;
  }

  // main wayland event dispatch loop
  loop {
    if let Err(e) = dispatch_once(&mut conn, &mut state) {
      write_wire_diagnostic("wl-watch", &e);
      return 1;
    }
  }
}
