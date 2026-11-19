//! Formatting without `core::fmt` or heap. Fixed-capacity string constructed by
//! [`StringOnStack::push`], which accepts anything implementing [`FmtLite`].

use core::ffi::{CStr, c_char};

/// Something that can be appended to a [`StringOnStack`].
pub trait FmtLite {
  fn append_to<const N: usize>(self, buf: &mut StringOnStack<N>);
}

impl FmtLite for &str {
  fn append_to<const N: usize>(self, buf: &mut StringOnStack<N>) {
    buf.append(self);
  }
}

impl FmtLite for &CStr {
  fn append_to<const N: usize>(self, buf: &mut StringOnStack<N>) {
    let bytes = self.to_bytes();
    let valid = match str::from_utf8(bytes) {
      Ok(text) => text,
      Err(e) => str::from_utf8(bytes.get(..e.valid_up_to()).unwrap_or(&[])).unwrap_or(""),
    };
    buf.append(valid);
  }
}

impl<const M: usize> FmtLite for StringOnStack<M> {
  fn append_to<const N: usize>(self, buf: &mut StringOnStack<N>) {
    buf.append(self.as_str());
  }
}

impl FmtLite for u64 {
  fn append_to<const N: usize>(mut self, buf: &mut StringOnStack<N>) {
    let mut digits = [b'0'; 20]; // u64::MAX has 20 digits
    let mut start = digits.len();
    loop {
      start -= 1;
      if let Some(d) = digits.get_mut(start) {
        *d = b'0' + (self % 10) as u8;
      }
      self /= 10;
      if self == 0 {
        break;
      }
    }
    let text = digits.get(start..).unwrap_or(&[]);
    buf.append(str::from_utf8(text).unwrap_or(""));
  }
}

impl FmtLite for i64 {
  fn append_to<const N: usize>(self, buf: &mut StringOnStack<N>) {
    if self < 0 {
      buf.append("-");
    }
    self.unsigned_abs().append_to(buf);
  }
}

impl FmtLite for u32 {
  fn append_to<const N: usize>(self, buf: &mut StringOnStack<N>) {
    u64::from(self).append_to(buf);
  }
}

impl FmtLite for i32 {
  fn append_to<const N: usize>(self, buf: &mut StringOnStack<N>) {
    i64::from(self).append_to(buf);
  }
}

/// Fixed-capacity, always NUL-terminated string on stack. Text past capacity (`N - 1` bytes)
/// is silently truncated on a character boundary. An embedded NUL ends the pushed text.
#[derive(Clone, Copy)]
pub struct StringOnStack<const N: usize> {
  buf: [u8; N],
  len: usize,
}

impl<const N: usize> StringOnStack<N> {
  #[must_use]
  pub const fn new() -> Self {
    Self {
      buf: [0; N],
      len: 0,
    }
  }

  #[must_use]
  pub const fn len(&self) -> usize {
    self.len
  }

  #[must_use]
  pub const fn is_empty(&self) -> bool {
    self.len == 0
  }

  pub fn clear(&mut self) {
    self.len = 0;
    if let Some(first) = self.buf.first_mut() {
      *first = 0;
    }
  }

  #[must_use]
  pub fn as_bytes(&self) -> &[u8] {
    self.buf.get(..self.len).unwrap_or(&[])
  }

  #[must_use]
  pub fn as_str(&self) -> &str {
    str::from_utf8(self.as_bytes()).unwrap_or("")
  }

  #[must_use]
  pub fn as_ptr(&self) -> *const c_char {
    self.buf.as_ptr().cast()
  }

  #[must_use]
  pub fn as_c_str(&self) -> &CStr {
    self
      .buf
      .get(..=self.len)
      .and_then(|b| CStr::from_bytes_until_nul(b).ok())
      .unwrap_or(c"")
  }

  /// Append a string or a number (anything [`FmtLite`]). Chainable.
  pub fn push<T: FmtLite>(&mut self, value: T) -> &mut Self {
    value.append_to(self);
    self
  }

  fn append(&mut self, text: &str) {
    let text = text.split('\0').next().unwrap_or("");
    let space = N.saturating_sub(1).saturating_sub(self.len);
    let mut n = text.len().min(space);
    while !text.is_char_boundary(n) {
      n -= 1;
    }
    if let Some(dst) = self.buf.get_mut(self.len..self.len + n)
      && let Some(src) = text.as_bytes().get(..n)
    {
      dst.copy_from_slice(src);
      self.len += n;
    }
    if let Some(nul) = self.buf.get_mut(self.len) {
      *nul = 0;
    }
  }
}

impl<const N: usize> Default for StringOnStack<N> {
  fn default() -> Self {
    Self::new()
  }
}

impl<const N: usize> From<&str> for StringOnStack<N> {
  fn from(text: &str) -> Self {
    let mut s = Self::new();
    s.push(text);
    s
  }
}

impl<const N: usize> core::ops::Deref for StringOnStack<N> {
  type Target = str;

  fn deref(&self) -> &str {
    self.as_str()
  }
}

impl<const N: usize> AsRef<[u8]> for StringOnStack<N> {
  fn as_ref(&self) -> &[u8] {
    self.as_bytes()
  }
}
