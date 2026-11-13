//! The few macOS calls that have no safe wrapper. Each `unsafe` block is a single libc call
//! with the invariant stated next to it; everything exported is safe.

use std::collections::HashMap;
use std::ffi::CStr;
use std::mem::{MaybeUninit, size_of};

pub(crate) fn bsdinfo(pid: u32) -> Option<libc::proc_bsdinfo> {
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the buffer is a correctly sized proc_bsdinfo and the kernel writes at most `size` bytes.
    let n = unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDTBSDINFO, 0, info.as_mut_ptr().cast(), size) };
    // SAFETY: a full-size return means the struct was filled in.
    (n == size).then(|| unsafe { info.assume_init() })
}

/// True while this exact process (same pid and start time) exists and is not a zombie.
pub fn alive(pid: u32, start_time: u64) -> bool {
    match bsdinfo(pid) {
        Some(i) => i.pbi_status != 5 && (start_time == 0 || i.pbi_start_tvsec == start_time),
        // not ours to inspect (or already a zombie): the short form still tells the two apart
        None => crate::procs::short(pid).is_some_and(|s| !s.zombie),
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// (weekday 0 = sunday, hour, minute, second) in local time.
pub fn local_time(epoch: u64) -> (u32, u32, u32, u32) {
    let t = epoch as libc::time_t;
    let mut tm = MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: localtime_r fills the tm we pass.
    let tm = unsafe {
        libc::localtime_r(&t, tm.as_mut_ptr());
        tm.assume_init()
    };
    (tm.tm_wday as u32, tm.tm_hour as u32, tm.tm_min as u32, tm.tm_sec as u32)
}

pub fn user_name(uid: u32) -> String {
    // SAFETY: getpwuid returns a pointer into static storage or null; copied out at once.
    unsafe {
        let pw = libc::getpwuid(uid);
        if pw.is_null() || (*pw).pw_name.is_null() {
            uid.to_string()
        } else {
            CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned()
        }
    }
}

pub fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// Everything parasight keeps lives here: `bin/`, `state/` and `rules.toml`.
/// `PARASIGHT_HOME` moves it; the default is `~/.parasight`.
pub fn parasight_home() -> std::path::PathBuf {
    match std::env::var_os("PARASIGHT_HOME").filter(|h| !h.is_empty()) {
        Some(h) => h.into(),
        None => std::path::PathBuf::from(home()).join(".parasight"),
    }
}

/// Earlier builds kept state in `~/.local/state/parasight` and settings in
/// `~/.config/parasight`. Move them into `parasight_home()` once, never overwriting.
pub fn move_old_files() {
    let home = std::path::PathBuf::from(home());
    let new = parasight_home();
    let moves = [
        (home.join(".local/state/parasight/last-interacted.json"), new.join("state/last-interacted.json")),
        (home.join(".config/parasight/rules.toml"), new.join("rules.toml")),
    ];
    for (from, to) in moves {
        if from.is_file() && !to.exists() && to.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok()) {
            let _ = std::fs::rename(&from, &to);
        }
    }
    // the old folders go only if nothing else is left in them
    for dir in [home.join(".local/state/parasight"), home.join(".config/parasight")] {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                if e.file_name() == "lock" {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let _ = std::fs::remove_dir(&dir);
    }
}

static QUIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_quit_signal(_: libc::c_int) {
    QUIT.store(true, std::sync::atomic::Ordering::Relaxed);
}

extern "C" fn on_hangup(_: libc::c_int) {
    // The terminal is gone: there is nothing to restore, and the input reader would spin on
    // end-of-file, so leave at once. (The nettop helper stops by itself when our pipe closes.)
    // SAFETY: _exit is async-signal-safe.
    unsafe { libc::_exit(129) }
}

/// Makes SIGTERM and SIGINT set a flag instead of killing us, so the terminal can be restored
/// and state saved; a hangup exits straight away. Returns the flag.
pub fn quit_flag() -> &'static std::sync::atomic::AtomicBool {
    // SAFETY: both handlers only do async-signal-safe work (an atomic store, _exit).
    unsafe {
        for sig in [libc::SIGTERM, libc::SIGINT] {
            libc::signal(sig, on_quit_signal as extern "C" fn(libc::c_int) as libc::sighandler_t);
        }
        libc::signal(libc::SIGHUP, on_hangup as extern "C" fn(libc::c_int) as libc::sighandler_t);
    }
    &QUIT
}

pub fn all_pids() -> Vec<u32> {
    // SAFETY: a null buffer asks only for the byte count.
    let bytes = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if bytes <= 0 {
        return Vec::new();
    }
    let mut pids = vec![0 as libc::pid_t; bytes as usize + 64];
    // SAFETY: the buffer holds `pids.len()` pid_t values and its byte size is passed.
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * size_of::<libc::pid_t>()) as libc::c_int) };
    pids.truncate(n.max(0) as usize);
    pids.into_iter().filter(|&p| p > 0).map(|p| p as u32).collect()
}

// <sys/proc_info.h>: struct socket_fdinfo is 792 bytes; these offsets were read from the
// system headers with offsetof() and `finds_our_own_listener` below checks them at test time.
const SOCKET_FDINFO_SIZE: usize = 792;
const OFF_SOI_KIND: usize = 256; // psi.soi_kind
const OFF_LPORT: usize = 268; // psi.soi_proto.pri_tcp.tcpsi_ini.insi_lport (network byte order)
const OFF_TCP_STATE: usize = 344; // psi.soi_proto.pri_tcp.tcpsi_state
const PROC_PIDFDSOCKETINFO: libc::c_int = 3;
const SOCKINFO_TCP: i32 = 2;
const TSI_S_LISTEN: i32 = 1;

fn listening_ports_of(pid: u32, out: &mut Vec<u16>) {
    let fd_size = size_of::<libc::proc_fdinfo>();
    // SAFETY: a null buffer asks only for the byte count.
    let bytes = unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if bytes <= 0 {
        return; // not ours to inspect, or gone
    }
    let mut fds = vec![libc::proc_fdinfo { proc_fd: 0, proc_fdtype: 0 }; bytes as usize / fd_size + 8];
    // SAFETY: the buffer holds `fds.len()` proc_fdinfo values and its byte size is passed.
    let bytes = unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), (fds.len() * fd_size) as libc::c_int) };
    fds.truncate(bytes.max(0) as usize / fd_size);
    let read_i32 = |buf: &[u8; SOCKET_FDINFO_SIZE], off: usize| i32::from_ne_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
    for fd in fds.iter().filter(|f| f.proc_fdtype == libc::PROX_FDTYPE_SOCKET as u32) {
        let mut buf = [0u8; SOCKET_FDINFO_SIZE];
        // SAFETY: the kernel writes at most the buffer size we pass; we only read it as bytes.
        let n = unsafe { libc::proc_pidfdinfo(pid as libc::c_int, fd.proc_fd, PROC_PIDFDSOCKETINFO, buf.as_mut_ptr().cast(), SOCKET_FDINFO_SIZE as libc::c_int) };
        if n as usize != SOCKET_FDINFO_SIZE || read_i32(&buf, OFF_SOI_KIND) != SOCKINFO_TCP || read_i32(&buf, OFF_TCP_STATE) != TSI_S_LISTEN {
            continue;
        }
        let port = u16::from_be(read_i32(&buf, OFF_LPORT) as u16);
        if port != 0 && !out.contains(&port) {
            out.push(port);
        }
    }
}

/// pid -> TCP ports it listens on, for the processes we are allowed to inspect (our own user's).
pub fn listening_ports() -> HashMap<u32, Vec<u16>> {
    let mut map = HashMap::new();
    let mut ports = Vec::new();
    for pid in all_pids() {
        ports.clear();
        listening_ports_of(pid, &mut ports);
        if !ports.is_empty() {
            ports.sort_unstable();
            map.insert(pid, ports.clone());
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_our_own_listener() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let ports = listening_ports();
        assert!(ports.get(&std::process::id()).is_some_and(|p| p.contains(&port)), "port {port} not found in {:?}", ports.get(&std::process::id()));
    }

    #[test]
    fn our_own_process_is_alive() {
        assert!(alive(std::process::id(), 0));
        assert!(!alive(0x3fff_fff0, 0));
    }
}
