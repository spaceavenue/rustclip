//! Direct syscall and process management helpers.

pub mod exit {
  /// Exit the process immediately with the given status code.
  #[inline]
  pub fn exit(code: i32) -> ! {
    #[cfg(target_arch = "x86_64")]
    unsafe {
      const SYS_EXIT_GROUP: usize = 231;
      core::arch::asm!(
        "syscall",
        in("rax") SYS_EXIT_GROUP,
        in("rdi") code,
        options(noreturn)
      );
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
      const SYS_EXIT_GROUP: usize = 94;
      core::arch::asm!(
        "svc #0",
        in("x8") SYS_EXIT_GROUP,
        in("x0") code,
        options(noreturn)
      );
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
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
    wllib::io::write_stderr(msg);
    exit(1)
  }
}

pub mod signal {
  use core::ffi::c_int;

  pub const SIGHUP: c_int = 1;
  pub const SIGINT: c_int = 2;
  pub const SIGQUIT: c_int = 3;
  pub const SIGKILL: c_int = 9;
  pub const SIGUSR1: c_int = 10;
  pub const SIGPIPE: c_int = 13;
  pub const SIGALRM: c_int = 14;
  pub const SIGTERM: c_int = 15;
  pub const SIGCHLD: c_int = 17;

  pub const SIG_DFL: usize = 0;
  pub const SIG_IGN: usize = 1;

  unsafe extern "C" {
    pub fn signal(signum: c_int, handler: usize) -> usize;
    pub fn sigaction(signum: c_int, act: *const SigAction, oldact: *mut SigAction) -> c_int;
  }

  #[repr(C)]
  #[derive(Clone, Copy)]
  pub struct SigAction {
    pub sa_handler: usize,
    pub sa_flags: c_int,
    pub sa_restorer: Option<unsafe extern "C" fn()>,
    pub sa_mask: [u64; 16],
  }

  impl Default for SigAction {
    fn default() -> Self {
      Self {
        sa_handler: SIG_DFL,
        sa_flags: 0,
        sa_restorer: None,
        sa_mask: [0; 16],
      }
    }
  }
}
