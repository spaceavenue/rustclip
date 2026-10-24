// Fixed-capacity mime-type storage and matching.
//
// Each mime type a selection offers arrives as the argument of its own separate `offer` event. So,
// it has to be copied out into owned storage the moment it arrives since the event payload it came
// from doesn't outlive the `dispatch_once` call that delivered it. Storage is a fixed
// `[u8; MAX_MIME_LEN]` per entry because no allocator.
//
// Mime inference has two independent halves:
// - Content-based (wl-copy, reading stdin): magic-byte sniffing (`sniff_mime`, cheap and
//   in-process) first, then `file --mime-type -` as a fallback, with the memfd's content piped
//   directly into its stdin — no path/procfs involved, so there's nothing to resolve or trust
//   beyond the subprocess's own stdout (which is still validated for `type/subtype` shape before
//   being trusted, since `file` can print an error message to stdout with exit status 0).
// - Name-based (wl-paste, writing to a redirected file): a small built-in extension table, not a
//   port of the freedesktop shared-mime-info glob database — see `EXTENSION_MIME_TABLE`.
//
// Offer classification and request selection (`classify_offer_types`/`mime_type_to_request`)
// prioritize explicit requested types, then inferred types, then UTF-8 / plain text fallbacks —
// matching wl-clipboard's behavior of the same name.

use core::str;

use libc::{SIG_DFL, SIGHUP, SIGPIPE};
use rustix::fd::{AsFd, AsRawFd, OwnedFd};
use rustix::fs::{CWD, Mode, OFlags, SeekFrom, open, readlinkat_raw, seek};
use rustix::io::read;
use rustix::pipe::{PipeFlags, pipe_with};
use rustix::stdio;
use wllib::fmt_lite::StringOnStack;

pub const MAX_MIME_LEN: usize = 128;
pub const MAX_MIME_TYPES: usize = 128;

pub type MimeType = StringOnStack<MAX_MIME_LEN>;

/// Generic plain text formats offered by wl-copy by default or alongside text MIME types.
pub const GENERIC_TEXT_OFFERS: [&str; 5] = [
  "text/plain",
  "text/plain;charset=utf-8",
  "TEXT",
  "STRING",
  "UTF8_STRING",
];

/// A heuristic to detect if a MIME type is plain text or text-compatible.
#[must_use]
pub fn is_text_mime(mime: &str) -> bool {
  // Types that explicitly declare they're textual
  mime.starts_with("text/")
    || mime == "TEXT"
    || mime == "STRING"
    || mime == "UTF8_STRING"
    // Common script and markup types
    || mime.contains("json")
    || mime.ends_with("script")
    || mime.ends_with("xml")
    || mime.ends_with("yaml")
    || mime.ends_with("csv")
    || mime.ends_with("ini")
    // Special-case PGP and SSH keys
    || mime.contains("application/vnd.ms-publisher")
    || mime.ends_with("pgp-keys")
}

/// Infer common MIME types from the initial bytes of the data.
#[must_use]
pub fn sniff_mime(buf: &[u8]) -> Option<&'static str> {
  // PNG: \x89PNG\r\n\x1a\n
  if buf.len() >= 8 && &buf[..8] == b"\x89PNG\r\n\x1a\n" {
    return Some("image/png");
  }
  // JPEG: \xFF\xD8\xFF
  if buf.len() >= 3 && &buf[..3] == b"\xff\xd8\xff" {
    return Some("image/jpeg");
  }
  // GIF: GIF87a or GIF89a
  if buf.len() >= 6 && (&buf[..6] == b"GIF87a" || &buf[..6] == b"GIF89a") {
    return Some("image/gif");
  }
  // WebP: RIFF....WEBP
  if buf.len() >= 12 && &buf[..4] == b"RIFF" && &buf[8..12] == b"WEBP" {
    return Some("image/webp");
  }
  // PDF: %PDF-
  if buf.len() >= 5 && &buf[..5] == b"%PDF-" {
    return Some("application/pdf");
  }
  // SVG / XML
  let trimmed = buf.trim_ascii_start();
  if trimmed.starts_with(b"<?xml") || trimmed.starts_with(b"<svg") {
    return Some("image/svg+xml");
  }
  // BMP: BM
  if buf.len() >= 2 && &buf[..2] == b"BM" {
    return Some("image/bmp");
  }
  // TIFF: little-endian "II*\0" or big-endian "MM\0*"
  if buf.len() >= 4 && (&buf[..4] == b"II*\0" || &buf[..4] == b"MM\0*") {
    return Some("image/tiff");
  }
  // ICO: \0\0\x01\0
  if buf.len() >= 4 && &buf[..4] == b"\x00\x00\x01\x00" {
    return Some("image/x-icon");
  }
  None
}

/// Return the path of the file linked to a file descriptor.
#[must_use]
pub fn path_for_fd<Fd: AsFd>(fd: Fd) -> Option<StringOnStack<256>> {
  let mut proc_path = StringOnStack::<64>::new();
  proc_path.push_str("/proc/self/fd/").push_i32(fd.as_fd().as_raw_fd());

  let mut buf = [0u8; 256];
  let n = readlinkat_raw(CWD, proc_path.as_c_str(), &mut buf).ok()?;
  let s = str::from_utf8(&buf[..n]).ok()?;
  let mut res = StringOnStack::<256>::new();
  res.push_str(s);
  Some(res)
}

/// Common filename extension -> MIME type mappings, covering the formats most likely to show up
/// in a `wl-paste > file.ext` redirection. Will expand as needed.
const EXTENSION_MIME_TABLE: &[(&str, &str)] = &[
  ("png", "image/png"),
  ("jpg", "image/jpeg"),
  ("jpeg", "image/jpeg"),
  ("gif", "image/gif"),
  ("bmp", "image/bmp"),
  ("webp", "image/webp"),
  ("ico", "image/x-icon"),
  ("tif", "image/tiff"),
  ("tiff", "image/tiff"),
  ("svg", "image/svg+xml"),
  ("pdf", "application/pdf"),
  ("txt", "text/plain"),
  ("md", "text/markdown"),
  ("json", "application/json"),
  ("html", "text/html"),
  ("htm", "text/html"),
  ("xml", "text/xml"),
  ("csv", "text/csv"),
  ("yaml", "text/yaml"),
  ("yml", "text/yaml"),
  ("zip", "application/zip"),
  ("tar", "application/x-tar"),
  ("gz", "application/gzip"),
  ("mp3", "audio/mpeg"),
  ("mp4", "video/mp4"),
  ("wav", "audio/wav"),
  ("ogg", "audio/ogg"),
];

/// Infer a MIME type from a file path's extension via `EXTENSION_MIME_TABLE`. Returns `None` for
/// extensions it doesn't recognize.
#[must_use]
pub fn infer_from_name(file_path: &str) -> Option<MimeType> {
  let filename = match file_path.rfind('/') {
    Some(idx) => &file_path[idx + 1..],
    None => file_path,
  };
  let ext = match filename.rfind('.') {
    Some(idx) if idx > 0 && idx + 1 < filename.len() => &filename[idx + 1..],
    _ => return None,
  };

  EXTENSION_MIME_TABLE
    .iter()
    .find(|(e, _)| e.eq_ignore_ascii_case(ext))
    .map(|(_, mime)| MimeType::from(*mime))
}

/// Infer a MIME type for an open fd's content.
#[must_use]
pub fn infer_from_fd(fd: &OwnedFd) -> Option<MimeType> {
  let mut header = [0u8; 64];

  seek(&fd, SeekFrom::Start(0)).ok()?;
  let amount_read = read(&fd, &mut header).ok()?;
  seek(&fd, SeekFrom::Start(0)).ok()?;

  if amount_read > 0
    && let Some(sniffed) = sniff_mime(&header[..amount_read as usize])
  {
    return Some(MimeType::from(sniffed));
  }
  query_file_for_fd(fd)
}

fn query_file_for_fd(fd: &OwnedFd) -> Option<MimeType> {
  let (read_fd, write_fd) = pipe_with(PipeFlags::CLOEXEC).ok()?;

  let pid = unsafe { libc::fork() };
  if pid < 0 {
    return None;
  }

  if pid == 0 {
    // close child's read end of the pipe.
    drop(read_fd);
    // route fd to stdin, write end to stdout of child process, then close child's write end.
    let _ = stdio::dup2_stdin(fd);
    let _ = stdio::dup2_stdout(&write_fd);
    drop(write_fd);

    // undo the inherited SIG_IGN. `file` shouldnt silently swallow a `SIGPIPE` from it's own stdout
    // pipe closing early.
    unsafe {
      libc::signal(SIGHUP, SIG_DFL);
      libc::signal(SIGPIPE, SIG_DFL);
    }

    if let Ok(devnull) = open(c"/dev/null", OFlags::WRONLY, Mode::empty()) {
      let _ = stdio::dup2_stderr(&devnull);
      drop(devnull);
    }

    let argv: [*const libc::c_char; 5] = [
      c"file".as_ptr(),
      // drop leading "-: "
      c"-b".as_ptr(),
      // print type/subtype. -i/--mine print '; charset=...' as well
      c"--mime-type".as_ptr(),
      c"-".as_ptr(),
      core::ptr::null(),
    ];

    unsafe {
      libc::execvp(argv[0], argv.as_ptr());
      // only reached if execvp() fails
      libc::_exit(1);
    }
  }

  // close parent's write end of the pipe so that the EOF arrives to the read end below.
  drop(write_fd);

  // this child needs an actual `waitpid` so its exit status can be checked below.
  let mut wstatus = 0;
  unsafe { libc::waitpid(pid, &raw mut wstatus, 0) };
  if !libc::WIFEXITED(wstatus) || libc::WEXITSTATUS(wstatus) != 0 {
    return None;
  }

  let mut buf = [0u8; 256];
  let n = read(read_fd, &mut buf).ok()?;

  let slice = &buf[..n as usize];
  let trimmed = match str::from_utf8(slice) {
    Ok(s) => s.trim_ascii(),
    Err(e) => match str::from_utf8(&slice[..e.valid_up_to()]) {
      Ok(s) => s.trim_ascii(),
      Err(_) => return None,
    },
  };

  // annoyingly, `file` prints failure messages to stdout instead of stderr with exit status 0. a
  // mime type is exactly `type/subtype`, so require that format instead of trusting exit status
  let looks_like_mime = !trimmed.contains(char::is_whitespace)
    && match trimmed.split_once('/') {
      Some((ty, subty)) => !ty.is_empty() && !subty.is_empty() && !subty.contains('/'),
      None => false,
    };
  if trimmed.is_empty() || trimmed.starts_with("inode/") || !looks_like_mime {
    return None;
  }

  Some(MimeType::from(trimmed))
}

#[derive(Default)]
pub struct ClassifiedTypes {
  pub explicit_available: bool,
  pub inferred_available: bool,
  pub plain_text_utf8_available: bool,
  pub plain_text_available: bool,
  pub has_sensitive_hint: bool,
  pub having_explicit_as_prefix: Option<MimeType>,
  pub any_text: Option<MimeType>,
  pub any: Option<MimeType>,
}

/// Classify offer types.
#[must_use]
pub fn classify_offer_types(
  available: &[MimeType],
  wanted_explicit: Option<&str>,
  inferred: Option<&str>,
) -> ClassifiedTypes {
  let mut types = ClassifiedTypes::default();

  for m in available {
    let s = m.as_str();
    if let Some(exp) = wanted_explicit {
      if s == exp {
        types.explicit_available = true;
      }
      if types.having_explicit_as_prefix.is_none() && s.starts_with(exp) {
        types.having_explicit_as_prefix = Some(*m);
      }
    }
    if inferred == Some(s) {
      types.inferred_available = true;
    }
    if s == "text/plain;charset=utf-8" {
      types.plain_text_utf8_available = true;
    }
    if s == "text/plain" {
      types.plain_text_available = true;
    }
    if types.any_text.is_none() && is_text_mime(s) {
      types.any_text = Some(*m);
    }
    if types.any.is_none() {
      types.any = Some(*m);
    }
    if s == "x-kde-passwordManagerHint" {
      types.has_sensitive_hint = true;
    }
  }

  types
}

/// Select the best matching MIME type to request.
#[must_use]
pub fn mime_type_to_request(
  types: &ClassifiedTypes,
  wanted_explicit: Option<&str>,
  inferred: Option<&str>,
) -> Option<MimeType> {
  if let Some(exp) = wanted_explicit {
    if exp == "text" {
      if types.plain_text_utf8_available {
        return Some(MimeType::from("text/plain;charset=utf-8"));
      }
      if types.plain_text_available {
        return Some(MimeType::from("text/plain"));
      }
      if let Some(any_txt) = types.any_text {
        return Some(any_txt);
      }
    } else if exp.contains('/') {
      // a fully qualified mime type only ever matches exactly, no prefix fallback.
      if types.explicit_available {
        return Some(MimeType::from(exp));
      }
    } else {
      if types.explicit_available {
        return Some(MimeType::from(exp));
      }
      if let Some(prefixed) = types.having_explicit_as_prefix {
        return Some(prefixed);
      }
    }
  } else {
    match inferred {
      None => {
        if types.plain_text_utf8_available {
          return Some(MimeType::from("text/plain;charset=utf-8"));
        }
        if types.plain_text_available {
          return Some(MimeType::from("text/plain"));
        }
        if let Some(any_txt) = types.any_text {
          return Some(any_txt);
        }
        if let Some(any) = types.any {
          return Some(any);
        }
      }
      Some(inf) if is_text_mime(inf) => {
        if types.inferred_available {
          return Some(MimeType::from(inf));
        }
        if types.plain_text_utf8_available {
          return Some(MimeType::from("text/plain;charset=utf-8"));
        }
        if types.plain_text_available {
          return Some(MimeType::from("text/plain"));
        }
        if let Some(any_txt) = types.any_text {
          return Some(any_txt);
        }
      }
      Some(inf) => {
        if types.inferred_available {
          return Some(MimeType::from(inf));
        }
      }
    }
  }

  None
}
