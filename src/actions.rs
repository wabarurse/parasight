//! Closing things: polite quit for apps, SIGTERM for dev process trees, `docker stop` for
//! containers. Each call waits up to five seconds and says what happened. Nothing here ever
//! sends SIGKILL except `force`, which is only reached through its own confirmation.

use crate::native;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub enum TargetKind {
    /// Ask the app to quit with an Apple event; `main` is its main process.
    App { bundle_id: Option<String> },
    /// SIGTERM the root, then whatever children are still alive.
    Tree,
    Container { id: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub name: String,
    pub kind: TargetKind,
    /// (pid, start time): the first entry is the root or main process.
    pub pids: Vec<(u32, u64)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Closed,
    StillRunning,
    Refused(String),
}

pub const WAIT: Duration = Duration::from_secs(5);

fn signal(pid: u32, start: u64, sig: libc::c_int) -> Result<(), String> {
    if pid <= 1 || pid == std::process::id() {
        return Err("not allowed".into());
    }
    if !native::alive(pid, start) {
        return Ok(());
    }
    // SAFETY: plain kill(2) on a pid whose identity (start time) was just checked.
    if unsafe { libc::kill(pid as libc::pid_t, sig) } == 0 {
        Ok(())
    } else {
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => Ok(()),
            Some(libc::EPERM) => Err("permission denied".into()),
            _ => Err("signal failed".into()),
        }
    }
}

fn wait_gone(pids: &[(u32, u64)], until: Instant) -> bool {
    loop {
        if pids.iter().all(|&(p, s)| !native::alive(p, s)) {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn term_tree(pids: &[(u32, u64)], until: Instant) -> Outcome {
    let Some(&(root, start)) = pids.first() else { return Outcome::Closed };
    let refused = signal(root, start, libc::SIGTERM).err();
    // give the root a moment to take its children down itself
    let grace = (Instant::now() + Duration::from_millis(700)).min(until);
    if !wait_gone(pids, grace) {
        for &(p, s) in &pids[1..] {
            let _ = signal(p, s, libc::SIGTERM);
        }
    }
    if wait_gone(&pids[..1], until) {
        Outcome::Closed
    } else if let Some(why) = refused {
        Outcome::Refused(why)
    } else {
        Outcome::StillRunning
    }
}

fn safe_bundle_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || ".-_".contains(c))
}

fn quit_app(bundle_id: Option<&str>, pids: &[(u32, u64)], until: Instant) -> Outcome {
    let main = &pids[..pids.len().min(1)];
    let mut asked = false;
    if let Some(id) = bundle_id.filter(|id| safe_bundle_id(id)) {
        let script = format!("tell application id \"{id}\" to quit");
        if let Ok(mut child) = Command::new("osascript").args(["-e", &script]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            // the event can block while the app shows a save dialog: do not wait past the deadline
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        asked = status.success();
                        break;
                    }
                    Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(100)),
                    _ => {
                        asked = true;
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
    }
    if !asked {
        // no bundle id, or the Apple event was not allowed: SIGTERM is the next most polite thing
        if let Some(&(pid, start)) = main.first()
            && let Err(why) = signal(pid, start, libc::SIGTERM)
        {
            return Outcome::Refused(why);
        }
    }
    if wait_gone(main, until) { Outcome::Closed } else { Outcome::StillRunning }
}

/// Closes one target the polite way and reports what happened within `WAIT`.
pub fn close(t: &Target) -> Outcome {
    let until = Instant::now() + WAIT;
    match &t.kind {
        TargetKind::Tree => term_tree(&t.pids, until),
        TargetKind::App { bundle_id } => quit_app(bundle_id.as_deref(), &t.pids, until),
        TargetKind::Container { id } => {
            match Command::new("docker").args(["stop", "-t", "4", id]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status() {
                Ok(s) if s.success() => Outcome::Closed,
                Ok(_) => Outcome::Refused("docker stop failed".into()),
                Err(_) => Outcome::Refused("docker not found".into()),
            }
        }
    }
}

/// SIGKILL. Only ever called after the user answered the separate "force kill?" question.
pub fn force(t: &Target) -> Outcome {
    if let TargetKind::Container { id } = &t.kind {
        return match Command::new("docker").args(["kill", id]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status() {
            Ok(s) if s.success() => Outcome::Closed,
            _ => Outcome::Refused("docker kill failed".into()),
        };
    }
    let mut refused = None;
    for &(p, s) in &t.pids {
        if let Err(e) = signal(p, s, libc::SIGKILL) {
            refused = Some(e);
        }
    }
    if wait_gone(&t.pids[..t.pids.len().min(1)], Instant::now() + Duration::from_secs(2)) {
        Outcome::Closed
    } else {
        refused.map(Outcome::Refused).unwrap_or(Outcome::StillRunning)
    }
}

#[cfg(test)]
mod tests {
    //! These tests signal real processes: only ones they spawn themselves.
    use super::*;
    use std::process::{Child, Command, Stdio};

    fn spawn(script: &str) -> (Child, Target) {
        let child = Command::new("/bin/sh").args(["-c", script]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        // give the shell a moment to set its trap or exec
        std::thread::sleep(Duration::from_millis(300));
        let start = crate::procs::task(child.id()).map(|t| t.start_time).unwrap_or(0);
        let target = Target { name: "test dummy".into(), kind: TargetKind::Tree, pids: vec![(child.id(), start)] };
        (child, target)
    }

    #[test]
    fn close_ends_a_process_we_spawned() {
        let (mut child, target) = spawn("exec sleep 4242");
        assert!(native::alive(child.id(), 0));
        assert_eq!(close(&target), Outcome::Closed);
        assert!(!native::alive(child.id(), 0));
        let _ = child.wait();
    }

    // design test 15
    #[test]
    fn a_process_that_ignores_sigterm_is_reported_still_running_and_not_force_killed() {
        let (mut child, target) = spawn("trap '' TERM; while :; do sleep 1; done");
        let t0 = Instant::now();
        assert_eq!(close(&target), Outcome::StillRunning);
        assert!(t0.elapsed() >= WAIT, "gave up before the full wait");
        assert!(native::alive(child.id(), 0), "it must not have been force-killed");
        // only the explicit second step kills it
        assert_eq!(force(&target), Outcome::Closed);
        assert!(!native::alive(child.id(), 0));
        let _ = child.wait();
    }

    #[test]
    fn a_reused_pid_is_left_alone() {
        let (mut child, mut target) = spawn("exec sleep 4243");
        // same pid, but a start time that says "this is not the process you meant"
        target.pids[0].1 += 1000;
        assert_eq!(close(&target), Outcome::Closed); // "that process" is gone
        assert!(native::alive(child.id(), 0), "a different process with that pid must not be signalled");
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn parasight_never_signals_itself_or_launchd() {
        assert!(signal(std::process::id(), 0, libc::SIGTERM).is_err());
        assert!(signal(1, 0, libc::SIGTERM).is_err());
    }
}
