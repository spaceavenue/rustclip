use core::ffi::{c_char, c_int};

use rustix::fd::OwnedFd;
use rustix::fs::{Mode, OFlags, open};
use rustix::io::read;
use rustix::pipe::{PipeFlags, pipe_with};
use rustix::stdio::{dup2_stdin, stdout};
use wllib::dispatch::EventHandler;
use wllib::error::{SysError, SysResultExt, WireError};
use wllib::io::write_stdout;
use wllib::protocols::{zwlr_data_control_device_v1, zwlr_data_control_offer_v1};
use wllib::registry::{GlobalHandler, bind, clamp_version};
use wllib::transport::Connection;
use wllib::wire::{Message, read_str, read_u32};

use crate::error::AppError;
use crate::exit;
use crate::mime::{
  MAX_MIME_TYPES, MimeType, classify_offer_types, is_text_mime, mime_type_to_request,
};

unsafe extern "C" {
  fn fork() -> c_int;
  fn execvp(file: *const c_char, argv: *const *const c_char) -> c_int;
  fn setenv(name: *const c_char, value: *const c_char, overwrite: c_int) -> c_int;
}

// tracks registered object ids
#[derive(Default)]
pub struct Global {
  // wl_seat global id
  pub seat_id: u32,
  // zwlr_data_control_manager global id
  pub manager_id: u32,
}

pub enum Action {
  PrintAndExit,
  ListTypes,
  PipeToCommand {
    argv: *const *mut c_char,
    argc: usize,
  },
}

pub struct State {
  pub global: Global,
  pub device_id: u32,
  pub use_primary: bool,
  pub wanted_mime: Option<MimeType>,
  pub inferred_mime: Option<MimeType>,
  pub no_newline: bool,
  pub action: Action,
  // set when a null offer id (cleared/emtpy clipboard) is received, so that a caller like
  // `wl-paste` can differentiate between "nothing was selected" and "still waiting..."
  pub had_empty_selection: bool,
  // the offer object currently being *built* via data_offer + offer events, before a `selection`
  // event confirms it as the active one. `building_id` is 0 until data_offer names one.
  building_id: u32,
  building_mimes: [MimeType; MAX_MIME_TYPES],
  building_mime_len: usize,
}

impl State {
  #[must_use]
  pub fn init(
    use_primary: bool,
    wanted_mime: Option<MimeType>,
    inferred_mime: Option<MimeType>,
    no_newline: bool,
    action: Action,
  ) -> Self {
    Self {
      global: Global::default(),
      device_id: 0,
      use_primary,
      wanted_mime,
      inferred_mime,
      no_newline,
      action,
      had_empty_selection: false,
      building_id: 0,
      building_mimes: [MimeType::default(); MAX_MIME_TYPES],
      building_mime_len: 0,
    }
  }

  // if we get a data offer, we update building the building id to whatever the data offer sets.
  // otherwise, we check which clipboard we're operating on and handle the selection.
  fn handle_device_event(&mut self, conn: &mut Connection, opcode: u16, data: &[u8]) {
    if opcode == zwlr_data_control_device_v1::event::DATA_OFFER {
      self.building_id = read_u32(data, 0);
      self.building_mime_len = 0;
      return;
    }
    let is_target_event = if self.use_primary {
      opcode == zwlr_data_control_device_v1::event::PRIMARY_SELECTION
    } else {
      opcode == zwlr_data_control_device_v1::event::SELECTION
    };
    if is_target_event {
      self.handle_selection(conn, read_u32(data, 0));
    }
  }

  fn handle_selection(&mut self, conn: &mut Connection, offer_id: u32) {
    // offer id 0 means empty clipboard or that it was cleared.
    if offer_id == 0 {
      self.had_empty_selection = true;
      if let Action::PipeToCommand { argv, argc } = &self.action
        && let Ok(devnull) = open(c"/dev/null", OFlags::RDONLY, Mode::empty())
      {
        run_with_stdin(devnull, *argv, *argc, c"nil".as_ptr(), core::ptr::null());
      }
      return;
    }

    // by protocol ordering, data_offer + its offer events always immediately precede the
    // selection event naming it, so the offer just confirmed is whichever one we were building.
    if offer_id != self.building_id {
      return;
    }

    // handle listing the mime types
    if let Action::ListTypes = &self.action {
      for i in 0..self.building_mime_len {
        write_stdout(self.building_mimes[i].as_bytes());
        write_stdout(b"\n");
      }
      exit::exit(0);
    }

    let mimes = &self.building_mimes[..self.building_mime_len];
    let classified = classify_offer_types(
      mimes,
      self
        .wanted_mime
        .as_ref()
        .map(wllib::fmt_lite::StringOnStack::as_str),
      self
        .inferred_mime
        .as_ref()
        .map(wllib::fmt_lite::StringOnStack::as_str),
    );

    let Some(selected_mime) = mime_type_to_request(
      &classified,
      self
        .wanted_mime
        .as_ref()
        .map(wllib::fmt_lite::StringOnStack::as_str),
      self
        .inferred_mime
        .as_ref()
        .map(wllib::fmt_lite::StringOnStack::as_str),
    ) else {
      // route these through AppError rather than distinct inline write_stderr calls, both so the
      // three cases share one place their wording lives) and so the message isn't hardcoded to
      // "[wl-paste]:" even though this same code path also runs for wl-watch's
      // Action::PipeToCommand. AppError's diagnostics use a binary-agnostic "rustclip:"
      // prefix instead.
      let err = if classified.any.is_none() {
        AppError::NothingCopied
      } else if self.wanted_mime.is_some() {
        AppError::MimeNotAvailable
      } else {
        AppError::InferredMimeNotAvailable
      };
      err.write_diagnostic();
      if matches!(&self.action, Action::PipeToCommand { .. }) {
        return;
      }
      exit::exit(1);
    };

    match fetch_offer(conn, offer_id, &selected_mime) {
      Ok(read_fd) => match &self.action {
        Action::PrintAndExit => {
          if let Err(e) = stream_fd_to_stdout(read_fd) {
            e.write_diagnostic();
          }
          if !self.no_newline && is_text_mime(selected_mime.as_str()) {
            write_stdout(b"\n");
          }
          exit::exit(0);
        }
        Action::PipeToCommand { argv, argc } => {
          let state_cstr = if classified.has_sensitive_hint {
            c"sensitive".as_ptr()
          } else {
            c"data".as_ptr()
          };
          let type_cstr = selected_mime.as_ptr();
          run_with_stdin(read_fd, *argv, *argc, state_cstr, type_cstr);
        }
        Action::ListTypes => unreachable!(),
      },
      Err(e) => e.write_diagnostic(),
    }
  }
}
impl GlobalHandler for State {
  // bind globals matching interfaces we want. we bind to the minimum of the client's wanted
  // version and the server's advertised version
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
        match bind(conn, name, interface, clamp_version(1, version), id) {
          Ok(()) => self.global.manager_id = id,
          Err(e) => e.write_diagnostic(),
        }
      }
      _ => (),
    }
  }
}
impl EventHandler for State {
  fn handle_event(&mut self, conn: &mut Connection, sender: u32, opcode: u16, data: &[u8]) {
    if sender == self.device_id {
      self.handle_device_event(conn, opcode, data);
    }
    if sender == self.building_id
      && opcode == zwlr_data_control_offer_v1::event::OFFER
      && let Some((mime_str, _)) = read_str(data, 0)
      && self.building_mime_len < MAX_MIME_TYPES
    {
      self.building_mimes[self.building_mime_len] = MimeType::from(mime_str);
      self.building_mime_len += 1;
    }
  }
}

// send `receive(mime, write_fd)` on `offer_id` using a locally-created pipe, then return the read
// end. we close our own copy of the write end immediately after sending it. the *other* copy (now
// held by whichever process is providing the data) still needs to close its own copy too before the
// reader sees EOF, but if we kept ours open as well, EOF would never arrive at all even after the
// real writer finishes.
fn fetch_offer(conn: &mut Connection, offer_id: u32, mime: &MimeType) -> Result<OwnedFd, AppError> {
  let (read_fd, write_fd) = pipe_with(PipeFlags::CLOEXEC).syserr("pipe")?;

  let mut msg = Message::new(offer_id, zwlr_data_control_offer_v1::request::RECEIVE);
  msg.write_str(mime.as_str());
  conn.send_logged(&msg, Some(&write_fd));

  Ok(read_fd)
}

// stream `read_fd` to stdout until EOF in fixed-size chunks. clipboard content isn't assumed to
// fit in memory all at once. distinguishes a genuine read/write failure from clean EOF so the
// caller can report it.
fn stream_fd_to_stdout(read_fd: OwnedFd) -> Result<(), AppError> {
  let mut buf = [0u8; 8192];
  loop {
    let n = read(&read_fd, &mut buf).syserr("read")?;
    if n == 0 {
      break;
    }
    let stdout = unsafe { stdout() };
    wllib::io::write_fd(stdout, &buf[..n as usize], None).map_err(AppError::Sys)?;
  }

  Ok(())
}

// fork, wire `read_fd` up as the child's stdin, set CLIPBOARD_STATE and CLIPBOARD_TYPE, and exec
// `argv`.
fn run_with_stdin(
  read_fd: OwnedFd,
  argv: *const *mut c_char,
  argc: usize,
  state: *const c_char,
  mime_type: *const c_char,
) {
  if argc == 0 {
    return;
  }
  let pid = unsafe { fork() };
  if pid < 0 {
    WireError::Sys(SysError::last("fork")).write_diagnostic();
    return;
  }
  if pid == 0 {
    let _ = dup2_stdin(read_fd);
    unsafe {
      if !state.is_null() {
        setenv(c"CLIPBOARD_STATE".as_ptr(), state, 1);
      }
      if !mime_type.is_null() {
        setenv(c"CLIPBOARD_TYPE".as_ptr(), mime_type, 1);
      }
      // `argv` came straight from `main`'s own argv, which the OS guarantees is NUL-terminated at
      // argv[argc]. so, a pointer offset into it is still validly NUL-terminated and this can be
      // handed to execvp directly without rebuilding it.
      execvp((*argv).cast_const(), argv.cast());
    }
    // only reached if exec itself failed
    exit::exit(1);
  }
}
