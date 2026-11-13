//! macOS calls only the clean-up tracker needs: idle time, the frontmost app, tty lookups.
//! As in `native`, every `unsafe` block is one C call with its invariant stated.

use std::ffi::CStr;
use std::mem::{MaybeUninit, size_of};

/// The controlling terminal's device number, if the process has one.
pub fn tty_dev(pid: u32) -> Option<u32> {
    let dev = crate::native::bsdinfo(pid)?.e_tdev;
    (dev != u32::MAX && dev != 0).then_some(dev)
}

/// When the terminal device was last read from, which is when someone last typed into it.
pub fn tty_atime(dev: u32) -> Option<u64> {
    // SAFETY: devname returns a pointer to a static buffer or null; we copy it out at once.
    let name = unsafe {
        let p = libc::devname(dev as libc::dev_t, libc::S_IFCHR);
        if p.is_null() {
            return None;
        }
        CStr::from_ptr(p).to_string_lossy().into_owned()
    };
    if !name.starts_with("tty") {
        return None;
    }
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(format!("/dev/{name}")).ok()?;
    (meta.rdev() as u32 == dev).then(|| meta.atime().max(0) as u64)
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
}

#[repr(C)]
struct ProcessSerialNumber {
    high: u32,
    low: u32,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn GetFrontProcess(psn: *mut ProcessSerialNumber) -> i32;
    fn GetProcessPID(psn: *const ProcessSerialNumber, pid: *mut libc::pid_t) -> i32;
}

/// The pid of the frontmost app. These two calls are old (Carbon) but need no run loop and no
/// permission, unlike NSWorkspace, which goes stale without an event loop.
pub fn front_pid() -> Option<u32> {
    let mut psn = ProcessSerialNumber { high: 0, low: 0 };
    let mut pid: libc::pid_t = 0;
    // SAFETY: both calls only write to the out-parameters we own.
    let ok = unsafe { GetFrontProcess(&mut psn) == 0 && GetProcessPID(&psn, &mut pid) == 0 };
    (ok && pid > 0).then_some(pid as u32)
}

/// The executable path of a process, via libproc.
pub fn proc_path(pid: u32) -> Option<String> {
    let mut buf = [0u8; 4096];
    // SAFETY: the buffer size is passed and the kernel writes at most that many bytes.
    let n = unsafe { libc::proc_pidpath(pid as libc::c_int, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

/// Seconds since the last system-wide input of each kind. Needs no permission.
pub fn input_ages() -> crate::interact::InputAges {
    // kCGEventSourceStateCombinedSessionState = 0; event types from CGEventTypes.h
    // SAFETY: plain C function with value arguments.
    let age = |t: u32| unsafe { CGEventSourceSecondsSinceLastEventType(0, t) };
    crate::interact::InputAges {
        key: age(10).min(age(12)),              // key down, modifier change
        click: age(1).min(age(3)).min(age(25)), // left, right, other mouse down
        scroll: age(22),
    }
}

pub fn boot_time() -> u64 {
    let mut tv = MaybeUninit::<libc::timeval>::zeroed();
    let mut size = size_of::<libc::timeval>();
    // SAFETY: kern.boottime is a timeval; size is passed in and out.
    let rc = unsafe { libc::sysctlbyname(c"kern.boottime".as_ptr(), tv.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) };
    if rc == 0 {
        // SAFETY: filled in by sysctl.
        unsafe { tv.assume_init() }.tv_sec.max(0) as u64
    } else {
        0
    }
}


pub fn now_f64() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_time_and_boot_time_are_sane() {
        let a = input_ages();
        assert!(a.since_input() >= 0.0);
        let boot = boot_time();
        assert!(boot > 1_500_000_000 && boot <= crate::native::now());
    }

    #[test]
    fn our_own_path_resolves() {
        let p = proc_path(std::process::id()).unwrap();
        assert!(p.contains("parasight"), "{p}");
    }
}
