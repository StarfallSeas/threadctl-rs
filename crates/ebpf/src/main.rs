//! threadctl-ebpf — kernel side (P0 empty skeleton; P3 migrates 既有实现 fork/exec, P5 extends sched_switch).

#![no_std]
#![no_main]
#![allow(linker_messages)]

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
