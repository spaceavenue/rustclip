//! Direct syscall and process management helpers.

use core::arch::asm;
use core::ffi::{CStr, c_char, c_int};
use core::hint::unreachable_unchecked;

use rustix::io::Errno;

const SYS_FORK: usize = 57;
const SYS_EXECVE: usize = 59;
const SYS_EXIT_GROUP: usize = 231;
const SYS_RT_SIGACTION: usize = 13;

#[inline(always)]
fn check_ret(ret: isize) -> Result<usize, Errno> {
  if (-4095..0).contains(&ret) {
    Err(Errno::from_raw_os_error(-ret as i32))
  } else {
    Ok(ret as usize)
  }
}

pub enum Fork {
  Child,
  Parent(u32),
}

/// Execute a fork syscall.
pub fn fork() -> Result<Fork, Errno> {
  let ret: isize;
  unsafe {
    asm!(
      "syscall",
      in("rax") SYS_FORK,
      lateout("rax") ret,
      lateout("rcx") _,
      lateout("r11") _,
      options(nostack)
    );
  }

  match check_ret(ret)? {
    0 => Ok(Fork::Child),
    _ => Ok(Fork::Parent(ret as u32)),
  }
}

/// Execute an execve syscall.
///
/// # SAFETY:
/// `path` must point to a null-terminated C string.
/// `argv` and `envp` must point to null-terminated arrays of null-terminated string pointers.
pub unsafe fn execve(
  path: *const c_char,
  argv: *const *const c_char,
  envp: *const *const c_char,
) -> Errno {
  let ret: isize;
  unsafe {
    asm!(
      "syscall",
      in("rax") SYS_EXECVE,
      in("rdi") path,
      in("rsi") argv,
      in("rdx") envp,
      lateout("rax") ret,
      lateout("rcx") _,
      lateout("r11") _,
      options(nostack)
    );
  }
  match check_ret(ret) {
    // only returns errors
    Ok(_) => unsafe { unreachable_unchecked() },
    Err(e) => e,
  }
}

/// Execute a file with arguments, searching `PATH` if `file` does not contain a slash.
///
/// # Safety
/// `file` must point to a null-terminated C string.
/// `argv` must point to a null-terminated array of null-terminated string pointers.
pub unsafe fn execvp(file: *const c_char, argv: *const *const c_char) -> Errno {
  let envp = crate::env::get_envp();
  let empty_envp: [*const c_char; 1] = [core::ptr::null()];
  let envp = if envp.is_null() {
    empty_envp.as_ptr()
  } else {
    envp
  };

  let cstr = unsafe { CStr::from_ptr(file) };
  let bytes = cstr.to_bytes();

  if bytes.contains(&b'/') {
    return unsafe { execve(file, argv, envp) };
  }

  let mut path_buf = wllib::fmt_lite::StringOnStack::<512>::new();
  let has_path = crate::env::get_env("PATH", &mut path_buf);
  let path_str = if has_path {
    path_buf.as_str()
  } else {
    "/usr/bin:/bin"
  };

  let mut last_err = Errno::NOENT;

  for dir in path_str.split(':') {
    let dir = if dir.is_empty() { "." } else { dir };
    let mut candidate = wllib::fmt_lite::StringOnStack::<256>::new();
    candidate.push(dir);
    if !candidate.ends_with('/') {
      candidate.push("/");
    }
    candidate.push(cstr.to_str().unwrap_or(""));

    let err = unsafe { execve(candidate.as_c_str().as_ptr(), argv, envp) };
    if err == Errno::ACCESS {
      last_err = Errno::ACCESS;
    } else if err != Errno::NOENT {
      return err;
    }
  }

  last_err
}

pub mod exit {

  use super::{SYS_EXIT_GROUP, asm};

  /// Exit the process immediately with the given status code.
  #[inline]
  pub fn exit(code: i32) -> ! {
    // implement by itself for noreturn
    #[cfg(target_arch = "x86_64")]
    unsafe {
      asm!(
        "syscall",
        in("rax") SYS_EXIT_GROUP,
        in("rdi") code,
        options(nostack, noreturn)
      );
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
      unsafe extern "C" {
        #[link_name = "exit"]
        fn c_exit(status: core::ffi::c_int) -> !;
      }
      unsafe { c_exit(code) }
    }
  }

  /// Write an error message to stderr and exit with code 1.
  #[inline]
  pub fn die(msg: &str) -> ! {
    crate::io::write_stderr(msg);
    exit(1)
  }
}

pub mod signal {
  // this was a bit more complicated to figure out.
  // kernel expects a pointer to a kernel_sigaction struct so we define that. it carries the
  // handler, the flags, the restorer and a mask.
  // a custom handler in particular requires SA_RESTORER in sa_flags, and sa_restorer pointing to
  // something that calls SYS_rt_sigreturn.
  use super::{Errno, SYS_RT_SIGACTION, asm, c_int, check_ret};

  pub const SIGHUP: c_int = 1;
  pub const SIGUSR1: c_int = 10;
  pub const SIGPIPE: c_int = 13;
  pub const SIGCHLD: c_int = 17;

  pub const SIG_DFL: usize = 0;
  pub const SIG_IGN: usize = 1;

  pub const SA_RESTORER: usize = 0x04000000;
  pub const SA_RESTART: usize = 0x10000000;

  #[cfg(target_arch = "x86_64")]
  #[unsafe(naked)]
  pub unsafe extern "C" fn default_restorer() {
    core::arch::naked_asm!(
      "mov rax, 15", // SYS_rt_sigreturn
      "syscall"
    );
  }

  #[repr(C)]
  pub struct KernelSigaction {
    pub sa_handler: usize,
    pub sa_flags: usize,
    pub sa_restorer: usize,
    pub sa_mask: u64,
  }

  impl KernelSigaction {
    /// Construct a generic signal action.
    pub fn new(handler: usize, flags: usize) -> Self {
      #[cfg(target_arch = "x86_64")]
      let (sa_flags, sa_restorer) = if handler == SIG_DFL || handler == SIG_IGN {
        (flags, 0)
      } else {
        (flags | SA_RESTORER, default_restorer as *const () as usize)
      };

      #[cfg(not(target_arch = "x86_64"))]
      let (sa_flags, sa_restorer) = (flags, 0);

      Self {
        sa_handler: handler,
        sa_flags,
        sa_restorer,
        sa_mask: 0,
      }
    }
  }

  /// Execute a sigaction syscall.
  pub unsafe fn sigaction(
    sig: i32,
    act: *const KernelSigaction,
    oldact: *mut KernelSigaction,
  ) -> Result<(), Errno> {
    let ret: isize;
    unsafe {
      asm!(
        "syscall",
        in("rax") SYS_RT_SIGACTION,
        in("rdi") sig,
        in("rsi") act,
        in("rdx") oldact,
        in("r10") size_of::<u64>(),
        lateout("rax") ret,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack)
      );
    }
    check_ret(ret).map(|_| ())
  }

  /// Wrapper for sigaction call with `SA_RESTART` set.
  pub unsafe fn signal(sig: i32, handler: usize) -> Result<(), Errno> {
    let act = KernelSigaction::new(handler, SA_RESTART);
    unsafe { sigaction(sig, &act, core::ptr::null_mut()) }
  }
}
