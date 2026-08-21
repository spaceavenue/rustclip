use wllib::error::{SysError, WireError};
use wllib::io::write_stderr;

pub enum AppError {
  Wire(WireError),
  Sys(SysError),
  // no offer/selection currently available when data was requested
  NothingCopied,
  // the mime type requested with -t isn't offered by the current selection
  MimeNotAvailable,
  // stdout was redirected to a file and a mime type was inferred from its extension, but that
  // type isn't offered by the current selection. distinct from `MimeNotAvailable` so the
  // diagnostic can tell the user *why* a type was expected (they asked for it vs. it was
  // guessed).
  InferredMimeNotAvailable,
}

impl AppError {
  pub fn write_diagnostic(&self) {
    match self {
      AppError::Wire(e) => e.write_diagnostic(),
      AppError::Sys(e) => WireError::Sys(*e).write_diagnostic(),
      AppError::NothingCopied => write_stderr(b"rustclip: nothing is currently copied\n"),
      AppError::MimeNotAvailable => {
        write_stderr("rustclip: requested mime type not offered by the current selection\n");
      }
      AppError::InferredMimeNotAvailable => {
        write_stderr("rustclip: clipboard content is not available as the inferred output type\n");
      }
    }
  }
}

impl From<WireError> for AppError {
  fn from(e: WireError) -> Self {
    AppError::Wire(e)
  }
}
