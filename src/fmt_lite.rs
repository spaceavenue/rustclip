//! Formatting methods due to absence of `core::fmt` and heap allocations: a fixed-capacity,
//! stack-allocated string builder ([`StringOnStack`]) and the [`FmtLite`] trait it's built
//! around.

use core::ffi::{CStr, c_char};

/// Lightweight formatter trait in absence of `core::fmt`.
pub trait FmtLite {
  fn format_into<const N: usize>(self, buf: &mut StringOnStack<N>);
}

impl<const M: usize> FmtLite for StringOnStack<M> {
  fn format_into<const N: usize>(self, buf: &mut StringOnStack<N>) {
    buf.push_self(self);
  }
}

impl FmtLite for &str {
  fn format_into<const N: usize>(self, buf: &mut StringOnStack<N>) {
    buf.push_str(self);
  }
}

impl FmtLite for &CStr {
  fn format_into<const N: usize>(self, buf: &mut StringOnStack<N>) {
    buf.push_cstr(self);
  }
}

impl FmtLite for char {
  fn format_into<const N: usize>(self, buf: &mut StringOnStack<N>) {
    buf.push_char(self);
  }
}

impl FmtLite for u32 {
  fn format_into<const N: usize>(self, buf: &mut StringOnStack<N>) {
    buf.push_u32(self);
  }
}

impl FmtLite for i32 {
  fn format_into<const N: usize>(self, buf: &mut StringOnStack<N>) {
    buf.push_i32(self);
  }
}

impl FmtLite for u64 {
  fn format_into<const N: usize>(self, buf: &mut StringOnStack<N>) {
    buf.push_u64(self);
  }
}

impl FmtLite for i64 {
  fn format_into<const N: usize>(self, buf: &mut StringOnStack<N>) {
    buf.push_i64(self);
  }
}

/// Allocate a fixed size string on the stack.
#[derive(Clone, Copy)]
pub struct StringOnStack<const N: usize> {
  buf: [u8; N],
  len: usize,
}

impl<const N: usize> StringOnStack<N> {
  /// Create an empty string.
  #[must_use]
  pub const fn new() -> Self {
    Self {
      buf: [0; N],
      len: 0,
    }
  }

  /// Returns the stored string as bytes.
  #[must_use]
  pub const fn capacity(&self) -> usize {
    N.saturating_sub(1)
  }

  /// Returns remaining capacity in bytes.
  #[must_use]
  pub const fn remaining_capacity(&self) -> usize {
    self.capacity().saturating_sub(self.len)
  }

  /// Current length in bytes.
  #[must_use]
  pub const fn len(&self) -> usize {
    self.len
  }

  /// Returns true if empty.
  #[must_use]
  pub const fn is_empty(&self) -> bool {
    self.len == 0
  }

  /// Clear the string.
  pub fn clear(&mut self) {
    self.len = 0;
    if N > 0 {
      self.buf[0] = 0;
    }
  }

  /// View as byte slice.
  #[must_use]
  pub fn as_bytes(&self) -> &[u8] {
    &self.buf[..self.len]
  }

  /// View as a string slice.
  #[must_use]
  pub fn as_str(&self) -> &str {
    // SAFETY: All push methods enforce UTF-8 validity.
    unsafe { str::from_utf8_unchecked(&self.buf[..self.len]) }
  }

  /// View as a mutable string slice.
  pub fn as_mut_str(&mut self) -> &mut str {
    // SAFETY: All push methods enforce UTF-8 validity.
    unsafe { str::from_utf8_unchecked_mut(&mut self.buf[..self.len]) }
  }

  /// Return a raw pointer to the C string representation.
  /// Guaranteed to be NUL-terminated at all times.
  #[must_use]
  pub fn as_ptr(&self) -> *const c_char {
    self.buf.as_ptr().cast::<c_char>()
  }

  /// View as a borrowed `CStr`. Always safe and guaranteed to terminate at the first NUL byte.
  #[must_use]
  pub fn as_c_str(&self) -> &CStr {
    if N == 0 {
      c""
    } else {
      match CStr::from_bytes_until_nul(&self.buf[..=self.len]) {
        Ok(s) => s,
        Err(_) => c"",
      }
    }
  }

  /// Push a string slice, silently truncating at character boundaries if capacity is exceeded.
  pub fn push_str(&mut self, s: &str) -> &mut Self {
    let space = self.remaining_capacity();
    let s_bytes = s.as_bytes();

    let content_len = s_bytes.iter().position(|&b| b == 0).unwrap_or(s.len());
    let mut n = content_len.min(space);
    while !s.is_char_boundary(n) {
      n -= 1;
    }

    if n > 0 {
      self.buf[self.len..self.len + n].copy_from_slice(&s_bytes[..n]);
      self.len += n;
      if self.len < N {
        self.buf[self.len] = 0; // Maintain NUL invariant
      }
    }

    self
  }

  /// Push a `CStr`'s content (excluding its own NUL terminator), silently truncating at a valid
  /// UTF-8 boundary if capacity is exceeded or the content isn't UTF-8 at all past some point.
  pub fn push_cstr(&mut self, s: &CStr) -> &mut Self {
    let space = self.remaining_capacity();
    if space == 0 {
      return self;
    }

    let bytes = s.to_bytes();
    let max_len = bytes.len().min(space);
    let slice = &bytes[..max_len];

    let n = match str::from_utf8(slice) {
      Ok(valid) => valid.len(),
      Err(err) => err.valid_up_to(),
    };

    if n > 0 {
      self.buf[self.len..self.len + n].copy_from_slice(&slice[..n]);
      self.len += n;
      if self.len < N {
        self.buf[self.len] = 0; // Maintain NUL invariant
      }
    }

    self
  }

  /// Push a single character.
  pub fn push_char(&mut self, c: char) -> &mut Self {
    let mut encode_buf = [0u8; 4];
    let s = c.encode_utf8(&mut encode_buf);
    self.push_str(s)
  }

  /// Push another `StringOnStack`.
  pub fn push_self<const M: usize>(&mut self, s: StringOnStack<M>) {
    self.push_str(s.as_str());
  }

  /// Push an unsigned 64-bit integer formatted in base 10.
  pub fn push_u64(&mut self, mut num: u64) -> &mut Self {
    // the loop below never executes for 0 (the `while num > 0` guard), which would otherwise
    // push an empty string instead of "0".
    if num == 0 {
      return self.push_str("0");
    }
    let mut tmp = [0u8; 20]; // u64::MAX is 20 decimal digits

    let mut idx = 20;
    while num > 0 {
      idx -= 1;
      tmp[idx] = b'0' + (num % 10) as u8;
      num /= 10;
    }
    // SAFETY: tmp contains only ASCII digits '0'-'9'
    let s = unsafe { str::from_utf8_unchecked(&tmp[idx..]) };
    self.push_str(s)
  }

  /// Push a signed 64-bit integer formatted in base 10.
  pub fn push_i64(&mut self, num: i64) -> &mut Self {
    if num < 0 {
      self.push_str("-");
      self.push_u64(num.unsigned_abs())
    } else {
      self.push_u64(num as u64)
    }
  }

  /// Push an unsigned 32-bit integer formatted in base 10.
  pub fn push_u32(&mut self, num: u32) -> &mut Self {
    self.push_u64(u64::from(num))
  }

  /// Push a signed 32-bit integer formatted in base 10.
  pub fn push_i32(&mut self, num: i32) -> &mut Self {
    self.push_i64(i64::from(num))
  }

  /// Push anything that implements `FmtLite` (currently: &str, char, u32, i32)
  pub fn push<T: FmtLite>(&mut self, value: T) -> &mut Self {
    value.format_into(self);
    self
  }
}

impl<const N: usize> core::ops::Deref for StringOnStack<N> {
  type Target = str;

  #[inline]
  fn deref(&self) -> &str {
    self.as_str()
  }
}

impl<const N: usize> core::ops::DerefMut for StringOnStack<N> {
  #[inline]
  fn deref_mut(&mut self) -> &mut str {
    self.as_mut_str()
  }
}

impl<const N: usize> Default for StringOnStack<N> {
  fn default() -> Self {
    Self::new()
  }
}

impl<const N: usize> AsRef<[u8]> for StringOnStack<N> {
  fn as_ref(&self) -> &[u8] {
    self.as_bytes()
  }
}

impl<const N: usize> PartialEq for StringOnStack<N> {
  fn eq(&self, other: &Self) -> bool {
    self.as_bytes() == other.as_bytes()
  }
}

impl<const N: usize> PartialEq<str> for StringOnStack<N> {
  fn eq(&self, other: &str) -> bool {
    self.as_bytes() == other.as_bytes()
  }
}

impl<const N: usize> PartialEq<CStr> for StringOnStack<N> {
  fn eq(&self, other: &CStr) -> bool {
    self.as_bytes() == other.to_bytes()
  }
}

impl<const N: usize> From<&str> for StringOnStack<N> {
  fn from(value: &str) -> Self {
    let mut s = Self::new();
    s.push(value);
    s
  }
}

impl<const N: usize> From<&CStr> for StringOnStack<N> {
  fn from(value: &CStr) -> Self {
    let mut s = Self::new();
    s.push(value);
    s
  }
}
