use core::arch::global_asm;
use core::ffi::c_char;

use crate::env::set_envp;

global_asm!(
  "
  .text
  .global _start
  _start:
    xor rbp, rbp
    pop rdi
    mov rsi, rsp
    and rsp, -16
    call {entry}
    mov rdi, rax
    mov rax, 231
    syscall
  ", entry = sym rust_entry
);

unsafe extern "C" fn rust_entry(argc: isize, argv: *const *mut c_char) -> i32 {
  let envp = unsafe { argv.add(argc as usize + 1).cast::<*const c_char>() };
  set_envp(envp);
  unsafe { main(argc, argv) }
}

unsafe extern "C" {
  fn main(argc: isize, argv: *const *mut c_char) -> i32;
}
