#![no_std]
#![no_main]
#![allow(clippy::result_large_err)]

use core::ffi::{c_char, c_int};

use rustclip::cli::{Arg, ArgParser, get_args_iter};
use rustclip::error::write_wire_diagnostic;
use rustclip::io::write_stderr;
use rustclip::mime::{self, MimeType};
use rustclip::state::{Action, State};
use rustix::stdio;
use wllib::dispatch::dispatch_once;
use wllib::registry::crawl;

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

  let mut conn = match rustclip::connect_wayland() {
    Ok(c) => c,
    Err(e) => {
      write_wire_diagnostic("wl-paste", &e);
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
    write_wire_diagnostic("wl-paste", &e);
    return 1;
  }

  if !state.global.require("wl-paste") {
    return 1;
  }

  if let Err(e) = state.create_device(&mut conn) {
    write_wire_diagnostic("wl-paste", &e);
    return 1;
  }

  // main wayland event dispatch loop
  loop {
    if let Err(e) = dispatch_once(&mut conn, &mut state) {
      write_wire_diagnostic("wl-paste", &e);
      return 1;
    }
    if state.had_empty_selection {
      write_stderr("[wl-paste]: nothing is currently copied\n");
      return 1;
    }
  }
}
