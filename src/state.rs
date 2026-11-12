use core::ffi::{CStr, c_char};
use core::ptr;

use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::{Mode, OFlags, open};
use rustix::io::read;
use rustix::pipe::{PipeFlags, pipe_with};
use rustix::stdio::{dup2_stdin, stdout};
use wllib::dispatch::EventHandler;
use wllib::error::{SysResultExt, WireError};
use wllib::protocols::{
  zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_offer_v1,
};
use wllib::registry::{GlobalHandler, bind, clamp_version};
use wllib::transport::{Connection, Ctx};
use wllib::wire::{Message, Writer};

use crate::env::get_envp;
use crate::error::{AppError, write_wire_diagnostic};
use crate::exit;
use crate::fmt_lite::StringOnStack;
use crate::io::{write_fd, write_stderr, write_stdout};
use crate::mime::{
  MAX_MIME_TYPES, MimeType, classify_offer_types, is_text_mime, mime_type_to_request,
};
use crate::syscall::{Fork, execvpe, fork};

const MANAGER_VERSION: u32 = 2;

#[derive(Default)]
pub struct Global {
  // wl_seat global id
  pub seat_id: u32,
  // zwlr_data_control_manager global id
  pub manager_id: u32,
}

impl Global {
  /// Bind the globals every clipboard binary needs. Call this from your `GlobalHandler::on_global`.
  /// We bind to the minimum of the client's wanted version and the server's advertised version.
  pub fn bind_global(
    &mut self,
    cx: &mut Ctx<'_>,
    name: u32,
    interface: &str,
    version: u32,
  ) -> Result<(), WireError> {
    match interface {
      "wl_seat" => {
        if self.seat_id != 0 {
          return Ok(());
        }
        let id = cx.alloc()?;
        bind(cx, name, interface, clamp_version(1, version), id)?;
        self.seat_id = id;
      }
      "zwlr_data_control_manager_v1" => {
        let id = cx.alloc()?;
        bind(
          cx,
          name,
          interface,
          clamp_version(MANAGER_VERSION, version),
          id,
        )?;
        self.manager_id = id;
      }
      _ => (),
    }
    Ok(())
  }

  /// Check that `crawl` found everything we need. Prints a diagnostic tagged with `prog` and
  /// returns `false` if not.
  #[must_use]
  pub fn require(&self, prog: &str) -> bool {
    let mut s = StringOnStack::<96>::new();
    if self.manager_id == 0 {
      s.push("[")
        .push(prog)
        .push("]: compositor does not support wlr-data-control\n");
    } else if self.seat_id == 0 {
      s.push("[").push(prog).push("]: no wl_seat found\n");
    } else {
      return true;
    }
    write_stderr(s);
    false
  }

  /// Ask the manager for a data device on the seat, so selection events start flowing. Returns
  /// the new device's object id.
  pub fn create_device<const N: usize>(&self, conn: &mut Connection<N>) -> Result<u32, WireError> {
    let device_id = conn.alloc_id()?;
    let mut w = Writer::<16>::new(
      self.manager_id,
      zwlr_data_control_manager_v1::request::GET_DATA_DEVICE,
    );
    w.u32(device_id).u32(self.seat_id);
    conn.send(w.finish()?, &[])?;
    Ok(device_id)
  }
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

  /// Create the data device and remember its id so its events can be recognised.
  pub fn create_device<const N: usize>(
    &mut self,
    conn: &mut Connection<N>,
  ) -> Result<(), WireError> {
    self.device_id = self.global.create_device(conn)?;
    Ok(())
  }

  // if we get a data offer, we update building the building id to whatever the data offer sets.
  // otherwise, we check which clipboard we're operating on and handle the selection.
  fn handle_device_event(&mut self, cx: &mut Ctx<'_>, msg: &Message<'_>) -> Result<(), WireError> {
    let opcode = msg.header.opcode;
    let mut args = msg.args();
    if opcode == zwlr_data_control_device_v1::event::DATA_OFFER {
      self.building_id = args.u32()?;
      self.building_mime_len = 0;
      return Ok(());
    }
    let is_target_event = if self.use_primary {
      opcode == zwlr_data_control_device_v1::event::PRIMARY_SELECTION
    } else {
      opcode == zwlr_data_control_device_v1::event::SELECTION
    };
    if is_target_event {
      self.handle_selection(cx, args.u32()?);
    }
    Ok(())
  }

  fn handle_selection(&mut self, cx: &Ctx<'_>, offer_id: u32) {
    // offer id 0 means empty clipboard or that it was cleared.
    if offer_id == 0 {
      self.had_empty_selection = true;
      if let Action::PipeToCommand { argv, argc } = &self.action
        && let Ok(devnull) = open(c"/dev/null", OFlags::RDONLY, Mode::empty())
      {
        run_with_stdin(devnull, *argv, *argc, c"nil", c"");
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
      self.wanted_mime.as_ref().map(StringOnStack::as_str),
      self.inferred_mime.as_ref().map(StringOnStack::as_str),
    );

    let Some(selected_mime) = mime_type_to_request(
      &classified,
      self.wanted_mime.as_ref().map(StringOnStack::as_str),
      self.inferred_mime.as_ref().map(StringOnStack::as_str),
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

    match fetch_offer(cx, offer_id, &selected_mime) {
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
            c"sensitive"
          } else {
            c"data"
          };
          let type_cstr = selected_mime.as_c_str();
          run_with_stdin(read_fd, *argv, *argc, state_cstr, type_cstr);
        }
        Action::ListTypes => unreachable!(),
      },
      Err(e) => e.write_diagnostic(),
    }
  }
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
  fn handle_event(&mut self, cx: &mut Ctx<'_>, msg: Message<'_>) -> Result<(), WireError> {
    let sender = msg.header.object_id;
    if sender == self.device_id {
      return self.handle_device_event(cx, &msg);
    }
    if sender == self.building_id
      && msg.header.opcode == zwlr_data_control_offer_v1::event::OFFER
      && self.building_mime_len < MAX_MIME_TYPES
    {
      let mime_str = msg.args().str()?;
      self.building_mimes[self.building_mime_len] = MimeType::from(mime_str);
      self.building_mime_len += 1;
    }
    Ok(())
  }
}

// send `receive(mime, write_fd)` on `offer_id` using a locally-created pipe, then return the read
// end. we close our own copy of the write end immediately after sending it. the *other* copy (now
// held by whichever process is providing the data) still needs to close its own copy too before the
// reader sees EOF, but if we kept ours open as well, EOF would never arrive at all even after the
// real writer finishes.
fn fetch_offer(cx: &Ctx<'_>, offer_id: u32, mime: &MimeType) -> Result<OwnedFd, AppError> {
  let (read_fd, write_fd) = pipe_with(PipeFlags::CLOEXEC).syserr("pipe")?;

  let mut w = Writer::<192>::new(offer_id, zwlr_data_control_offer_v1::request::RECEIVE);
  w.str(mime.as_str());
  cx.send(w.finish()?, &[write_fd.as_fd()])?;

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
    write_fd(stdout, &buf[..n as usize], None).map_err(AppError::Sys)?;
  }

  Ok(())
}

// fork, wire `read_fd` up as the child's stdin, set CLIPBOARD_STATE and CLIPBOARD_TYPE, and exec
// `argv`.
fn run_with_stdin(
  read_fd: OwnedFd,
  argv: *const *mut c_char,
  argc: usize,
  state: &CStr,
  mime_type: &CStr,
) {
  if argc == 0 {
    return;
  }
  let fork_res = match fork().syserr("fork") {
    Ok(res) => res,
    Err(e) => {
      write_wire_diagnostic("[rustclip]", &e);
      return;
    }
  };
  if let Fork::Child = fork_res {
    let _ = dup2_stdin(read_fd);

    let mut state_var = StringOnStack::<32>::new();
    state_var.push("CLIPBOARD_STATE=").push(state);

    let mut type_var = StringOnStack::<160>::new();
    type_var.push("CLIPBOARD_TYPE=").push(mime_type);

    let parent_envp = get_envp();
    let mut child_envp = [ptr::null::<c_char>(); 256];
    let mut count = 0;

    if !parent_envp.is_null() {
      let mut cur = parent_envp;
      while !unsafe { *cur }.is_null() && count < child_envp.len() - 3 {
        let entry = unsafe { *cur };
        let bytes = unsafe { CStr::from_ptr(entry).to_bytes() };
        if !bytes.starts_with(b"CLIPBOARD_STATE=") && !bytes.starts_with(b"CLIPBOARD_TYPE=") {
          child_envp[count] = entry;
          count += 1;
        }
        cur = unsafe { cur.add(1) };
      }
    }

    if !state.to_bytes().is_empty() && count < child_envp.len() - 2 {
      child_envp[count] = state_var.as_c_str().as_ptr();
      count += 1;
    }

    if !mime_type.to_bytes().is_empty() && count < child_envp.len() - 1 {
      child_envp[count] = type_var.as_c_str().as_ptr();
      count += 1;
    }

    child_envp[count] = ptr::null();

    unsafe {
      execvpe((*argv).cast_const(), argv.cast(), child_envp.as_ptr());
    }

    // only reached if exec itself failed
    exit::exit(1);
  }
}
