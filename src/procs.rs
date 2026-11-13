//! Reads processes straight from libproc. One or two syscalls per process per refresh for the
//! numbers that change (cpu time, memory, parent), and the expensive details (path, command
//! line, working directory) only when asked. Every `unsafe` block is a single C call into a
//! buffer of the right size; everything exported is safe.
//!
//! Only our own user's processes can be read this way. For everyone else's the kernel gives
//! out just parent, owner and short name; their numbers come from `ps` (see `sources`).

use std::ffi::CStr;
use std::mem::{MaybeUninit, size_of};

/// What changes from refresh to refresh, for a process we may inspect.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Task {
    pub ppid: u32,
    pub uid: u32,
    pub start_time: u64,
    pub threads: u32,
    /// User + system cpu time so far, in nanoseconds.
    pub cpu_ns: u64,
    pub resident: u64,
    pub name: String,
}

/// The little the kernel tells anyone about any process.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Short {
    pub ppid: u32,
    pub uid: u32,
    pub zombie: bool,
    pub name: String,
}

fn cstr(bytes: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = bytes.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[repr(C)]
struct Timebase {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_timebase_info(info: *mut Timebase) -> libc::c_int;
}

/// (numer, denom) to turn mach time units into nanoseconds.
fn timebase() -> (u64, u64) {
    static TB: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();
    *TB.get_or_init(|| {
        let mut tb = Timebase { numer: 1, denom: 1 };
        // SAFETY: the call only fills the two-field struct we pass.
        unsafe { mach_timebase_info(&mut tb) };
        (tb.numer.max(1) as u64, tb.denom.max(1) as u64)
    })
}

fn mach_to_ns(t: u64) -> u64 {
    let (n, d) = timebase();
    (t as u128 * n as u128 / d as u128) as u64
}

fn pidinfo<T>(pid: u32, flavor: libc::c_int) -> Option<T> {
    let mut info = MaybeUninit::<T>::zeroed();
    let size = size_of::<T>() as libc::c_int;
    // SAFETY: T is the plain C struct that belongs to `flavor` at every call site below, and
    // the kernel writes at most `size` bytes into it.
    let n = unsafe { libc::proc_pidinfo(pid as libc::c_int, flavor, 0, info.as_mut_ptr().cast(), size) };
    // SAFETY: a full-size return means the struct was filled in; all-zero is also a valid value.
    (n == size).then(|| unsafe { info.assume_init() })
}

/// Everything that changes, in one syscall. `None` for other users' processes and zombies.
pub fn task(pid: u32) -> Option<Task> {
    let all: libc::proc_taskallinfo = pidinfo(pid, libc::PROC_PIDTASKALLINFO)?;
    let (b, t) = (&all.pbsd, &all.ptinfo);
    let name = if b.pbi_name[0] != 0 { cstr(&b.pbi_name) } else { cstr(&b.pbi_comm) };
    Some(Task {
        ppid: b.pbi_ppid,
        uid: b.pbi_uid,
        start_time: b.pbi_start_tvsec,
        threads: t.pti_threadnum.max(0) as u32,
        cpu_ns: mach_to_ns(t.pti_total_user + t.pti_total_system),
        resident: t.pti_resident_size,
        name,
    })
}

/// Parent, owner, state and short name: readable for every process, root's included.
pub fn short(pid: u32) -> Option<Short> {
    let s: libc::proc_bsdshortinfo = pidinfo(pid, libc::PROC_PIDT_SHORTBSDINFO)?;
    Some(Short { ppid: s.pbsi_ppid, uid: s.pbsi_uid, zombie: s.pbsi_status == 5, name: cstr(&s.pbsi_comm) })
}

/// The memory figure Activity Monitor shows (physical footprint), for our own processes.
pub fn footprint(pid: u32) -> Option<u64> {
    let mut ru = MaybeUninit::<libc::rusage_info_v4>::zeroed();
    // SAFETY: the buffer is a rusage_info_v4 and RUSAGE_INFO_V4 asks for exactly that.
    let rc = unsafe { libc::proc_pid_rusage(pid as libc::c_int, libc::RUSAGE_INFO_V4, ru.as_mut_ptr().cast()) };
    // SAFETY: filled in on success.
    (rc == 0).then(|| unsafe { ru.assume_init() }.ri_phys_footprint)
}

pub fn exe(pid: u32) -> Option<String> {
    let mut buf = [0u8; 4096];
    // SAFETY: the buffer size is passed and the kernel writes at most that many bytes.
    let n = unsafe { libc::proc_pidpath(pid as libc::c_int, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

pub fn cwd(pid: u32) -> Option<String> {
    let info: libc::proc_vnodepathinfo = pidinfo(pid, libc::PROC_PIDVNODEPATHINFO)?;
    // SAFETY: vip_path is MAXPATHLEN bytes declared as [[c_char; 32]; 32]; the kernel
    // nul-terminates the path inside it, so reading a C string from its start stays in bounds.
    let path = unsafe { CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast()) };
    let path = path.to_string_lossy();
    (!path.is_empty()).then(|| path.into_owned())
}

/// The argument vector, from the kernel's copy of the process's arguments.
pub fn cmdline(pid: u32) -> Vec<String> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut size: libc::size_t = 0;
    // SAFETY: a null buffer asks only for the size.
    if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) } != 0 || size == 0 {
        return Vec::new();
    }
    let mut buf = vec![0u8; size];
    // SAFETY: the buffer is `size` bytes and the kernel updates `size` to what it wrote.
    if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) } != 0 {
        return Vec::new();
    }
    buf.truncate(size);
    parse_procargs(&buf)
}

/// KERN_PROCARGS2 layout: argc (4 bytes), the exec path, nul padding, then argc nul-terminated
/// arguments (the environment follows; it is never read).
pub fn parse_procargs(buf: &[u8]) -> Vec<String> {
    if buf.len() < 4 {
        return Vec::new();
    }
    let argc = i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]).max(0) as usize;
    let rest = &buf[4..];
    let Some(path_end) = rest.iter().position(|&b| b == 0) else { return Vec::new() };
    let mut i = path_end;
    while i < rest.len() && rest[i] == 0 {
        i += 1;
    }
    rest[i..].split(|&b| b == 0).take(argc).map(|a| String::from_utf8_lossy(a).into_owned()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_our_own_process() {
        let me = std::process::id();
        let t = task(me).expect("own task info");
        assert_eq!(t.uid, unsafe { libc::getuid() });
        assert!(t.threads >= 1 && t.resident > 0 && t.start_time > 1_500_000_000);
        assert!(t.name.starts_with("parasight"), "{}", t.name);
        let s = short(me).unwrap();
        assert_eq!((s.ppid, s.uid, s.zombie), (t.ppid, t.uid, false));
        assert!(footprint(me).unwrap() > 0);
        assert!(exe(me).unwrap().contains("parasight"));
        assert_eq!(cwd(me), std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned()));
        let args = cmdline(me);
        assert_eq!(args.len(), std::env::args().count());
        assert!(args[0].contains("parasight"));
    }

    #[test]
    fn cpu_time_moves_forward_in_nanoseconds() {
        let me = std::process::id();
        let a = task(me).unwrap().cpu_ns;
        let t0 = std::time::Instant::now();
        let mut x = 0u64;
        while t0.elapsed().as_millis() < 60 {
            x = x.wrapping_mul(31).wrapping_add(7);
        }
        std::hint::black_box(x);
        let spent = task(me).unwrap().cpu_ns - a;
        // about 60 ms of spinning: anything between 20 ms and 2 s proves the unit is right
        assert!((20_000_000..2_000_000_000).contains(&spent), "{spent}");
    }

    #[test]
    fn launchd_is_only_visible_in_short_form() {
        assert!(task(1).is_none());
        let s = short(1).unwrap();
        assert_eq!((s.ppid, s.uid, s.name.as_str()), (0, 0, "launchd"));
    }

    #[test]
    fn procargs_layout() {
        let mut buf = 3i32.to_ne_bytes().to_vec();
        buf.extend(b"/usr/bin/python3\0\0\0\0python3\0-m\0http.server\0HOME=/Users/me\0SECRET=1\0");
        assert_eq!(parse_procargs(&buf), ["python3", "-m", "http.server"]);
        assert!(parse_procargs(&[1, 0]).is_empty());
        assert!(parse_procargs(&0i32.to_ne_bytes()).is_empty());
    }
}
