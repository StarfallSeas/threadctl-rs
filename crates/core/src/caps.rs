//! Capability checks — Q6 decision: `sched_fifo/rr` needs root or CAP_SYS_NICE.

/// CAP_SYS_NICE capability number (Linux capabilities.h; not exported by libc crate).
const CAP_SYS_NICE: u32 = 23;

/// Whether RT scheduling privileges are available (euid==0 or effective CAP_SYS_NICE).
pub fn can_rt_sched() -> bool {
    if unsafe { libc::geteuid() } == 0 {
        // Android Magisk modules run as root; this always short-circuits true.
        // The CAP_SYS_NICE path is kept for non-root desktop Linux.
        return true;
    }
    has_cap(CAP_SYS_NICE)
}

/// Check the effective capability bit (capget).
fn has_cap(cap: u32) -> bool {
    const V3: u32 = 0x20080522; // _LINUX_CAPABILITY_VERSION_3

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CapHeader {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CapData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }

    let mut hdr = CapHeader { version: V3, pid: 0 };
    let mut data = [CapData { effective: 0, permitted: 0, inheritable: 0 }; 2];

    // cap 0..=31 lives in data[0], 32..=63 in data[1]
    let ret = unsafe {
        libc::syscall(
            libc::SYS_capget,
            &mut hdr as *mut CapHeader,
            data.as_mut_ptr() as *mut CapData,
        )
    };
    if ret != 0 {
        return false;
    }
    let word = if cap < 32 { data[0].effective } else { data[1].effective };
    word & (1u32 << (cap % 32)) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_check_runs() {
        // Value-agnostic: only verifies the capget call path does not crash and returns bool
        let _ = can_rt_sched();
    }
}
