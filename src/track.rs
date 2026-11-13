//! `parasight track`: the last-interacted sampler by itself, with no ui, so the clean-up view
//! has real history the moment it is opened. Opt-in; `--install` makes it a login item.

use crate::collect::Collector;
use crate::interact::State;
use crate::rules::Rules;
use crate::sources;
use crate::tracker::{self, Tracker};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::time::Duration;

const LABEL: &str = "com.parasight.track";

fn plist_path() -> PathBuf {
    PathBuf::from(crate::native::home()).join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The LaunchAgent that starts `parasight track` at login and keeps it running.
pub fn launch_agent_plist(exe: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array><string>{}</string><string>track</string></array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ProcessType</key><string>Background</string>
</dict>
</plist>
"#,
        xml_escape(exe)
    )
}

fn launchctl(args: &[&str]) -> bool {
    Command::new("launchctl").args(args).status().map(|s| s.success()).unwrap_or(false)
}

fn install() -> i32 {
    let Ok(exe) = std::env::current_exe() else {
        eprintln!("parasight: cannot find my own path");
        return 1;
    };
    let path = plist_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&path, launch_agent_plist(&exe.to_string_lossy())) {
        eprintln!("parasight: could not write {}: {e}", path.display());
        return 1;
    }
    // SAFETY: getuid cannot fail.
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    let _ = launchctl(&["bootout", &domain, &path.to_string_lossy()]);
    if launchctl(&["bootstrap", &domain, &path.to_string_lossy()]) {
        println!("installed: parasight track now starts at login ({})", path.display());
        println!("remove it with: parasight track --uninstall");
        0
    } else {
        eprintln!("parasight: wrote {} but launchctl would not load it", path.display());
        1
    }
}

fn uninstall() -> i32 {
    let path = plist_path();
    // SAFETY: getuid cannot fail.
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    let _ = launchctl(&["bootout", &domain, &path.to_string_lossy()]);
    match std::fs::remove_file(&path) {
        Ok(()) => println!("removed {}", path.display()),
        Err(_) => println!("nothing installed"),
    }
    0
}

pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("--install") => return install(),
        Some("--uninstall") => return uninstall(),
        Some("--print-plist") => {
            print!("{}", launch_agent_plist(&std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()));
            return 0;
        }
        Some(other) => {
            eprintln!("parasight track: unknown option {other} (try --install, --uninstall)");
            return 2;
        }
        None => {}
    }
    let quit = crate::native::quit_flag();
    // network use is judged from byte counters, so a slow nettop is enough here
    let shared = sources::start(sources::Options { docker: true, net_interval: 5 });
    let mut col = Collector::new(shared.clone(), Rules::load());
    let mut tracker = Tracker::start(shared.clone(), tracker::Options { state_path: Some(State::default_path()), persist: true });
    if !tracker.is_writer() {
        eprintln!("parasight track: another parasight is already tracking; nothing to do");
        shared.shutdown();
        return 1;
    }
    eprintln!("parasight track: sampling input and network use (ctrl-c to stop)");
    // apps are sampled every second on the tracker's own thread; dev items only need a look
    // now and then, because network use is judged from byte counters, not from a moment
    let mut n = 0u32;
    while !quit.load(Ordering::Relaxed) {
        if n.is_multiple_of(10) {
            col.tick();
            tracker.observe(&col);
        }
        n += 1;
        std::thread::sleep(Duration::from_secs(1));
    }
    tracker.save();
    shared.shutdown();
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_agent_runs_track_at_login() {
        let p = launch_agent_plist("/Users/a & b/bin/parasight");
        let v = plist::Value::from_reader_xml(p.as_bytes()).unwrap();
        let d = v.as_dictionary().unwrap();
        assert_eq!(d["Label"].as_string(), Some(LABEL));
        let args: Vec<&str> = d["ProgramArguments"].as_array().unwrap().iter().filter_map(|a| a.as_string()).collect();
        assert_eq!(args, ["/Users/a & b/bin/parasight", "track"]);
        assert_eq!(d["RunAtLoad"].as_boolean(), Some(true));
    }
}
