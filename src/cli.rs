//! Command-line argument parsing.

use core::ffi::{CStr, c_char};
use core::slice;

/// Get an iterator over `argv`.
///
/// # Safety
/// If `argc > 0`, `argv` must be valid to read up to `argc` pointers,
/// each pointing to a valid null-terminated C string.
pub unsafe fn get_args_iter<'a>(
  argc: usize,
  argv: *const *mut c_char,
) -> impl Iterator<Item = &'a str> {
  let argv_slice = if argc > 0 && !argv.is_null() {
    unsafe { slice::from_raw_parts(argv, argc) }
  } else {
    &[]
  };

  argv_slice.get(1..).unwrap_or(&[]).iter().map(|&ptr| {
    if ptr.is_null() {
      ""
    } else {
      unsafe { CStr::from_ptr(ptr).to_str().unwrap_or("") }
    }
  })
}

#[derive(Debug, PartialEq, Eq)]
pub enum Arg<'a> {
  Short(char),
  Long(&'a str),
  Value(&'a str),
}

pub struct ArgParser<'a, I> {
  iter: I,
  short_cluster: Option<&'a str>,
  attached_value: Option<&'a str>,
  stopped: bool,
}

impl<'a, I: Iterator<Item = &'a str>> ArgParser<'a, I> {
  pub fn new(iter: I) -> Self {
    Self {
      iter,
      short_cluster: None,
      attached_value: None,
      stopped: false,
    }
  }

  pub fn next_val(&mut self) -> Option<&'a str> {
    if let Some(val) = self.attached_value.take() {
      return Some(val);
    }
    if let Some(cluster) = self.short_cluster.take() {
      return Some(cluster);
    }
    self.iter.next()
  }

  pub fn next_arg(&mut self) -> Option<Arg<'a>> {
    if let Some(v) = self.attached_value.take() {
      return Some(Arg::Value(v));
    }

    if let Some(cluster) = self.short_cluster.take() {
      let mut chars = cluster.chars();
      if let Some(ch) = chars.next() {
        let rest = chars.as_str();
        if !rest.is_empty() {
          self.short_cluster = Some(rest);
        }
        return Some(Arg::Short(ch));
      }
    }

    let item = self.iter.next()?;

    if self.stopped {
      return Some(Arg::Value(item));
    }

    if item == "--" {
      self.stopped = true;
      return self.next_arg();
    }

    if let Some(long) = item.strip_prefix("--") {
      if let Some((key, val)) = long.split_once('=') {
        self.attached_value = Some(val);
        return Some(Arg::Long(key));
      }
      return Some(Arg::Long(long));
    }

    if let Some(short) = item.strip_prefix("-")
      && !short.is_empty()
    {
      let mut chars = short.chars();
      let first = chars.next()?;
      let rest = chars.as_str();
      if !rest.is_empty() {
        self.short_cluster = Some(rest);
      }
      return Some(Arg::Short(first));
    }

    Some(Arg::Value(item))
  }
}

impl<'a, I: Iterator<Item = &'a str>> Iterator for ArgParser<'a, I> {
  type Item = Arg<'a>;

  fn next(&mut self) -> Option<Self::Item> {
    self.next_arg()
  }
}
