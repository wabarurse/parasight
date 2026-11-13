//! Gathers what the clean-up scorer needs: who was last interacted with (a 1 s sampler that
//! pairs the frontmost app with new input, plus network use of dev items), Dock pins, power
//! assertions and Spotlight dates. Scoring itself is pure and lives in `cleanup`.

use crate::classify;
use crate::cleanup::Facts;
use crate::collect::Collector;
use crate::interact::{InputSampler, State};
use crate::model::*;
use crate::native;
use crate::native_track as nt;
use crate::sources::{Shared, lock, run};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Things that are meant to sit in the background. The clean-up view treats them gently.
const SERVICES: &[&str] = &[
    "postgres", "redis-server", "mysqld", "mariadbd", "mongod", "memcached", "nginx", "caddy", "httpd", "ollama",
    "limactl", "lima", "colima", "dockerd", "containerd", "podman", "gvproxy", "vfkit", "ssh-agent", "gpg-agent",
    "tmux", "screen", "zellij", "mosh-server", "syncthing", "temporal", "minio", "beam.smp", "elasticsearch",
    "etcd", "consul", "vault", "tailscaled", "cloudflared", "adb", "emacs", "orbstack", "dnsmasq", "sshd",
    "clickhouse", "rabbitmq-server", "kafka", "zookeeper", "influxd", "prometheus", "grafana", "supervisord",
    "pueued", "atuin", "direnv", "keychain", "docker", "com.docker.backend", "hostagent", "ssh", "autossh", "bazel", "bazel-real",
];


pub fn is_known_service(p: &ProcInfo) -> bool {
    let b = classify::bin_name(p);
    if SERVICES.contains(&b.as_str()) || b.starts_with("qemu-system") || b.starts_with("com.docker.") {
        return true;
    }
    // build servers that detach on purpose and run as a jvm: bazel renames itself
    // "bazel(workspace)", gradle and kotlin are recognisable by their main class
    let title = p.cmd.first().map(String::as_str).unwrap_or(&p.name);
    title.starts_with("bazel(") || p.name.starts_with("bazel(") || p.cmd.iter().any(|a| a.contains("GradleDaemon") || a.contains("KotlinCompileDaemon") || a.ends_with("A-server.jar"))
}

#[derive(Default)]
struct Inner {
    /// Bundle path of the frontmost app.
    front: Mutex<Option<String>>,
    /// Bundle paths and bundle ids pinned in the Dock.
    pinned: Mutex<HashSet<String>>,
    /// bundle path -> Spotlight last-used date (epoch seconds); `None` when Spotlight has none
    last_opened: Mutex<HashMap<String, Option<u64>>>,
    assertion_pids: Mutex<HashSet<u32>>,
    state: Mutex<State>,
    /// This process owns the state file (only one writer at a time).
    writer: AtomicBool,
}

pub struct Options {
    /// Where last-interacted stamps live; `None` keeps them in memory only.
    pub state_path: Option<PathBuf>,
    /// Write the state file. When false it is read once and never written.
    pub persist: bool,
}

pub struct Tracker {
    src: Arc<Shared>,
    inner: Arc<Inner>,
    state_path: Option<PathBuf>,
    started: Instant,
    ticks: u64,
    /// Bytes each dev item had moved in total when last looked at: more now means it was used.
    seen_bytes: HashMap<String, u64>,
}

impl Tracker {
    pub fn start(src: Arc<Shared>, opts: Options) -> Tracker {
        let inner = Arc::new(Inner::default());
        let now = native::now();
        let mut lock_file = None;
        match &opts.state_path {
            Some(path) => {
                let mut st = State::load(path);
                st.resume(now, nt::boot_time());
                *lock(&inner.state) = st;
                if opts.persist {
                    lock_file = writer_lock(path);
                    inner.writer.store(lock_file.is_some(), Ordering::Relaxed);
                }
            }
            None => lock(&inner.state).resume(now, 0),
        }
        *lock(&inner.pinned) = read_dock_pins();

        let (s, i, path, persist) = (src.clone(), inner.clone(), opts.state_path.clone(), opts.persist);
        std::thread::spawn(move || input_loop(&s, &i, path, persist, lock_file));
        let (s, i) = (src.clone(), inner.clone());
        std::thread::spawn(move || slow_loop(&s, &i));
        Tracker { src, inner, state_path: opts.state_path, started: Instant::now(), ticks: 0, seen_bytes: HashMap::new() }
    }

    pub fn is_writer(&self) -> bool {
        self.inner.writer.load(Ordering::Relaxed)
    }

    pub fn tracking_since(&self) -> u64 {
        lock(&self.inner.state).tracking_since
    }

    /// Writes the state once more; called on a clean exit.
    pub fn save(&self) {
        if let Some(path) = self.state_path.as_ref().filter(|_| self.is_writer()) {
            let st = lock(&self.inner.state).clone();
            let _ = st.save(path);
        }
    }

    /// Call after every collector tick: network use is one of the two ways a dev item is
    /// "interacted with" (the other, typing into its terminal, is read from the tty when asked).
    pub fn observe(&mut self, col: &Collector) {
        self.ticks += 1;
        let net = lock(&self.src.net);
        // without nettop there is no way to know who was quiet: wait for it, but not for ever
        if !net.ready && self.started.elapsed().as_secs() < 12 {
            return;
        }
        let now = native::now();
        let mut st = lock(&self.inner.state);
        let mut keys = HashSet::new();
        for it in &col.snapshot.items {
            match &it.kind {
                ItemKind::Dev { root_pid } => {
                    let Some(root) = col.procs().get(root_pid) else { continue };
                    let key = State::proc_key(root.pid, root.start_time);
                    let bytes: u64 = it.procs.iter().map(|p| net.totals.get(&p.pid).copied().unwrap_or(0)).sum();
                    let before = self.seen_bytes.insert(key.clone(), bytes);
                    let active = it.rx + it.tx > 0.0 || before.is_some_and(|b| bytes > b);
                    st.see_proc(&key, now, active);
                    keys.insert(key);
                }
                ItemKind::Container { .. } => {
                    st.see_proc(&it.key, now, it.cpu >= 0.5 || it.rx + it.tx > 0.0);
                    keys.insert(it.key.clone());
                }
                _ => {}
            }
        }
        if self.ticks.is_multiple_of(40) {
            st.prune_procs(&|k| keys.contains(k));
            self.seen_bytes.retain(|k, _| keys.contains(k));
        }
    }

    /// One `Facts` per app, dev item and container. System items are left out entirely.
    pub fn facts(&self, col: &Collector) -> Vec<Facts> {
        let now = native::now();
        let procs = col.procs();
        let selfs = self_pids(procs);
        let gui = lock(&self.src.gui);
        let st = lock(&self.inner.state);
        let front = lock(&self.inner.front).clone();
        let pinned = lock(&self.inner.pinned);
        let last_opened = lock(&self.inner.last_opened);
        let assertions = lock(&self.inner.assertion_pids);
        let mut out = Vec::new();
        for it in &col.snapshot.items {
            if it.group == Group::System {
                continue;
            }
            let mut f = Facts {
                key: it.key.clone(),
                name: it.name.clone(),
                group: Some(it.group),
                mem: it.mem,
                cpu: it.cpu,
                net: it.rx + it.tx,
                is_self: it.procs.iter().any(|p| selfs.contains(&p.pid)),
                command: it.name.split("  :").next().unwrap_or(&it.name).to_string(),
                ..Default::default()
            };
            let oldest = it.procs.iter().map(|p| p.start_time).filter(|&s| s > 0).min().unwrap_or(now);
            f.age_secs = now.saturating_sub(oldest);
            match &it.kind {
                ItemKind::App { bundle } => {
                    let app = gui.get(bundle);
                    let id = app.and_then(|a| a.bundle_id.as_deref()).unwrap_or("");
                    f.frontmost = front.as_deref() == Some(bundle.as_str());
                    f.protected = matches!(id, "com.apple.finder" | "com.apple.dock" | "com.apple.loginwindow");
                    f.background_only = !app.is_some_and(|a| a.foreground);
                    f.pinned = pinned.contains(bundle) || (!id.is_empty() && pinned.contains(id));
                    f.holds_assertion = it.procs.iter().any(|p| assertions.contains(&p.pid));
                    f.last_opened_secs = last_opened.get(bundle).copied().flatten().map(|t| now.saturating_sub(t));
                    if let Some((unused, seen)) = st.app_unused(bundle, now) {
                        // an app launched after tracking began cannot be unused for longer than it has run
                        f.unused_secs = Some(unused.min(f.age_secs));
                        f.input_seen = seen;
                    }
                }
                ItemKind::Dev { root_pid } => {
                    let Some(root) = procs.get(root_pid) else { continue };
                    f.launchd = root.launchd_label.is_some();
                    f.service = is_known_service(root);
                    f.orphaned = root.ppid <= 1 && !f.launchd;
                    f.zombie = root.zombie;
                    f.cwd = root.cwd.clone();
                    f.has_port = it.name.contains("  :");
                    let mut last = st.proc_last_used(&State::proc_key(root.pid, root.start_time));
                    if let Some((l, seen)) = &mut last {
                        let mut newer = |t: u64| {
                            if t > *l {
                                (*l, *seen) = (t, true);
                            }
                        };
                        if !f.orphaned {
                            // typing into its terminal counts. An orphan's terminal is gone, and
                            // its old device name may belong to a new tab by now, so not for those.
                            let ttys: HashSet<u32> = it.procs.iter().filter_map(|p| nt::tty_dev(p.pid)).collect();
                            for t in ttys {
                                // a terminal that has been typed into since the process began
                                newer(nt::tty_atime(t).filter(|&a| a > root.start_time).unwrap_or(0));
                            }
                        }
                        // a tool run by an app (a language server under an editor) is in use when the app is
                        let parent_app = procs.get(&root.ppid).and_then(|pp| classify::bundle_of(&pp.exe));
                        if let Some((unused, true)) = parent_app.and_then(|b| st.app_unused(b, now)) {
                            newer(now.saturating_sub(unused));
                        }
                        *l = (*l).max(root.start_time);
                    }
                    f.unused_secs = last.map(|(l, _)| now.saturating_sub(l));
                    f.input_seen = last.is_some_and(|(_, seen)| seen);
                }
                ItemKind::Container { .. } => {
                    f.container = true;
                    let last = st.proc_last_used(&it.key);
                    f.unused_secs = last.map(|(l, _)| now.saturating_sub(l));
                    f.input_seen = last.is_some_and(|(_, seen)| seen);
                }
                ItemKind::System => continue,
            }
            out.push(f);
        }
        out
    }
}

/// parasight, every ancestor of it, and (under tmux and friends) whatever hosts the clients.
fn self_pids(procs: &HashMap<u32, ProcInfo>) -> HashSet<u32> {
    let is_mux = |p: &ProcInfo| matches!(classify::bin_name(p).as_str(), "tmux" | "screen" | "zellij");
    let mut set = HashSet::new();
    let walk = |mut pid: u32, set: &mut HashSet<u32>| {
        for _ in 0..64 {
            if pid <= 1 || !set.insert(pid) {
                break;
            }
            pid = procs.get(&pid).map(|p| p.ppid).unwrap_or(0);
        }
    };
    walk(std::process::id(), &mut set);
    if set.iter().any(|p| procs.get(p).is_some_and(is_mux)) {
        for c in procs.values().filter(|p| is_mux(p)) {
            walk(c.pid, &mut set);
        }
    }
    set
}

/// Only one process writes the state file; the others read it. Returns the held lock.
fn writer_lock(state_path: &std::path::Path) -> Option<std::fs::File> {
    let dir = state_path.parent()?;
    std::fs::create_dir_all(dir).ok()?;
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("lock")).ok()?;
    use std::os::fd::AsRawFd;
    // SAFETY: flock on a file descriptor we own; the lock is released when the file closes.
    (unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0).then_some(f)
}

/// The 1 s sampler: frontmost app + seconds since the last key, click or scroll. Both are
/// native calls (no subprocess) and neither needs a permission.
fn input_loop(src: &Shared, inner: &Inner, path: Option<PathBuf>, persist: bool, _lock_file: Option<std::fs::File>) {
    let mut sampler = InputSampler::default();
    let writer = inner.writer.load(Ordering::Relaxed);
    let mut last_sync = Instant::now();
    let mut cache: (u32, Option<String>) = (0, None);
    loop {
        if let Some(pid) = nt::front_pid() {
            if cache.0 != pid {
                cache = (pid, nt::proc_path(pid).and_then(|p| classify::bundle_of(&p).map(str::to_string)));
            }
        } else {
            cache = (0, None);
        }
        let front = cache.1.as_deref();
        {
            let mut f = lock(&inner.front);
            if f.as_deref() != front {
                *f = front.map(str::to_string);
            }
        }
        let now = nt::now_f64();
        let stamp = sampler.feed(now, front, nt::input_ages());
        {
            let mut st = lock(&inner.state);
            st.tick(now as u64);
            if let Some((app, at)) = stamp {
                st.stamp_app(app, at);
            }
        }
        if let Some(path) = &path
            && last_sync.elapsed() >= Duration::from_secs(30)
        {
            last_sync = Instant::now();
            if writer {
                let st = lock(&inner.state).clone();
                let _ = st.save(path);
            } else if persist {
                // `parasight track` (or another parasight) owns the file: pick up what it saw
                let other = State::load(path);
                lock(&inner.state).merge(&other);
            }
        }
        if !src.nap(Duration::from_secs(1)) {
            break;
        }
    }
}

/// Things that change rarely: Dock pins, power assertions, Spotlight dates of new apps.
fn slow_loop(src: &Shared, inner: &Inner) {
    let mut n: u64 = 0;
    loop {
        if n % 6 == 5 {
            *lock(&inner.pinned) = read_dock_pins();
        }
        // pmset is the one subprocess left here: the IOKit call behind it is not worth the
        // unsafe code for a 15 s poll
        if let Some(out) = run("pmset", &["-g", "assertions"]) {
            *lock(&inner.assertion_pids) = parse_assertions(&out);
        }
        let todo: Vec<String> = {
            let known = lock(&inner.last_opened);
            lock(&src.gui).keys().filter(|b| !known.contains_key(*b)).take(8).cloned().collect()
        };
        for bundle in todo {
            let date = run("mdls", &["-raw", "-name", "kMDItemLastUsedDate", &bundle]).and_then(|s| parse_mdls_date(&s));
            lock(&inner.last_opened).insert(bundle, date);
        }
        n += 1;
        if !src.nap(Duration::from_secs(15)) {
            break;
        }
    }
}

fn read_dock_pins() -> HashSet<String> {
    let path = PathBuf::from(native::home()).join("Library/Preferences/com.apple.dock.plist");
    plist::Value::from_file(path).map(|v| parse_dock(&v)).unwrap_or_default()
}

/// Bundle ids and bundle paths of the apps pinned in the Dock, from its preferences plist.
pub fn parse_dock(v: &plist::Value) -> HashSet<String> {
    let mut set = HashSet::new();
    let apps = v.as_dictionary().and_then(|d| d.get("persistent-apps")).and_then(|a| a.as_array());
    for tile in apps.into_iter().flatten() {
        let Some(data) = tile.as_dictionary().and_then(|t| t.get("tile-data")).and_then(|d| d.as_dictionary()) else { continue };
        if let Some(id) = data.get("bundle-identifier").and_then(|x| x.as_string()) {
            set.insert(id.to_string());
        }
        let url = data.get("file-data").and_then(|f| f.as_dictionary()).and_then(|f| f.get("_CFURLString")).and_then(|x| x.as_string());
        if let Some(url) = url {
            let p = url.strip_prefix("file://").unwrap_or(url);
            set.insert(percent_decode(p).trim_end_matches('/').to_string());
        }
    }
    set
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = (b[i] == b'%' && i + 2 < b.len()).then(|| s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())).flatten();
        match hex {
            Some(v) => {
                out.push(v);
                i += 3;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Pids that hold (or have held for them) an audio or prevent-sleep power assertion.
pub fn parse_assertions(text: &str) -> HashSet<u32> {
    const KINDS: &[&str] = &["PreventUserIdleSystemSleep", "PreventUserIdleDisplaySleep", "NoIdleSleepAssertion", "NoDisplaySleepAssertion", "PreventSystemSleep"];
    let mut set = HashSet::new();
    let mut holding = false;
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("pid ") {
            holding = KINDS.iter().any(|k| t.contains(&format!(" {k} named:")));
            if holding && let Some(pid) = rest.split('(').next().and_then(|p| p.trim().parse().ok()) {
                set.insert(pid);
            }
        } else if let Some(rest) = t.strip_prefix("Created for PID: ")
            && holding
            && let Ok(pid) = rest.trim_end_matches('.').trim().parse()
        {
            set.insert(pid);
        }
    }
    set
}

/// "2026-11-13 17:36:10 +0000" -> epoch seconds.
pub fn parse_mdls_date(s: &str) -> Option<u64> {
    let s = s.trim();
    let (date, rest) = s.split_once(' ')?;
    let time = rest.split(' ').next()?;
    let mut d = date.split('-').map(|x| x.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let mut t = time.split(':').map(|x| x.parse::<i64>().ok());
    let (h, mi, sec) = (t.next()??, t.next()??, t.next()??);
    // days from civil (Howard Hinnant)
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec;
    (secs > 0).then_some(secs as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dock_pins() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>persistent-apps</key><array>
  <dict><key>tile-data</key><dict>
    <key>bundle-identifier</key><string>com.spotify.client</string>
    <key>file-data</key><dict><key>_CFURLString</key><string>file:///Applications/Visual%20Studio%20Code.app/</string></dict>
  </dict></dict>
  <dict><key>tile-type</key><string>spacer-tile</string></dict>
</array></dict></plist>"#;
        let pins = parse_dock(&plist::Value::from_reader_xml(xml.as_bytes()).unwrap());
        assert!(pins.contains("com.spotify.client"));
        assert!(pins.contains("/Applications/Visual Studio Code.app"));
        assert_eq!(pins.len(), 2);
    }

    #[test]
    fn power_assertions() {
        let text = "Listed by owning process:\n   pid 756(sharingd): [0x1] 00:00:00 PreventUserIdleSystemSleep named: \"Handoff\"  \n   pid 445(coreaudiod): [0x2] 03:03:28 PreventUserIdleSystemSleep named: \"com.apple.audio.BuiltInSpeakerDevice\"  \n\tCreated for PID: 1114. \n\tResources: audio-out BuiltInSpeakerDevice \n   pid 12630(Claude): [0x3] 00:13:53 NoIdleSleepAssertion named: \"Electron\"  \n   pid 432(WindowServer): [0x4] 00:00:00 UserIsActive named: \"x\"  \n\tCreated for PID: 99. \n";
        let p = parse_assertions(text);
        assert!(p.contains(&1114) && p.contains(&12630) && p.contains(&445));
        assert!(!p.contains(&432) && !p.contains(&99));
    }

    #[test]
    fn mdls_dates() {
        assert_eq!(parse_mdls_date("2026-11-13 17:36:10 +0000"), Some(1_794_591_370));
        assert_eq!(parse_mdls_date("1970-01-02 00:00:00 +0000"), Some(86_400));
        assert_eq!(parse_mdls_date("(null)"), None);
    }

    // design test 14: a real orphan, spawned here, shows up as a candidate and exits when closed
    #[test]
    fn an_orphaned_sleep_we_spawn_is_a_candidate_and_closing_it_ends_it() {
        use crate::actions::{self, Outcome};
        let out = std::process::Command::new("/bin/sh").args(["-c", "sleep 4343 >/dev/null 2>&1 & echo $!"]).output().unwrap();
        let pid: u32 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
        // whatever happens below, the sleep we started does not outlive the test
        struct Guard(u32);
        impl Drop for Guard {
            fn drop(&mut self) {
                if crate::procs::task(self.0).is_some_and(|t| t.name == "sleep") {
                    // SAFETY: our own child, checked by name.
                    unsafe { libc::kill(self.0 as libc::pid_t, libc::SIGKILL) };
                }
            }
        }
        let _guard = Guard(pid);

        let shared = crate::sources::start(crate::sources::Options { docker: false, net_interval: 2 });
        let mut col = Collector::new(shared.clone(), crate::rules::Rules::default());
        let mut tracker = Tracker::start(shared.clone(), Options { state_path: None, persist: false });
        let key = format!("dev:{pid}");
        let mut found = None;
        for _ in 0..40 {
            col.tick();
            tracker.observe(&col);
            let facts = tracker.facts(&col);
            if let Some(f) = facts.iter().find(|f| f.key == key) {
                assert!(f.orphaned, "ppid 1 and not a launchd job");
                assert!(!f.is_self);
            }
            found = crate::cleanup::score(&facts, &[], "").into_iter().find(|c| c.key == key);
            if found.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        let cand = found.expect("the orphaned sleep should be offered");
        assert_eq!(cand.name, "sleep 4343");
        assert!(cand.reason.starts_with("orphaned"), "{}", cand.reason);
        assert!(!cand.ticked, "seconds old: listed for review, not checked");

        // "confirm": close exactly that candidate
        let target = col.target(&cand.key).unwrap();
        assert_eq!(target.pids.iter().map(|p| p.0).collect::<Vec<_>>(), [pid]);
        assert_eq!(actions::close(&target), Outcome::Closed);
        assert!(!native::alive(pid, 0));
        shared.shutdown();
    }

    #[test]
    fn services_are_recognised_by_binary_name() {
        let p = |exe: &str| ProcInfo { exe: exe.into(), ..Default::default() };
        assert!(is_known_service(&p("/opt/homebrew/opt/postgresql@16/bin/postgres")));
        assert!(is_known_service(&p("/opt/homebrew/bin/limactl")));
        assert!(!is_known_service(&p("/opt/homebrew/bin/node")));
        // an ssh with no parent is a control master or a tunnel, left running on purpose
        assert!(is_known_service(&p("/usr/bin/ssh")));
        let bazel = ProcInfo { exe: "/private/var/tmp/_bazel_me/install/abc/embedded_tools/jdk/bin/java".into(), name: "bazel(my-workspace)".into(), cmd: vec!["bazel(my-workspace)".into(), "-Xverify:none".into()], ..Default::default() };
        assert!(is_known_service(&bazel));
        let gradle = ProcInfo { exe: "/opt/jdk/bin/java".into(), cmd: vec!["java".into(), "org.gradle.launcher.daemon.bootstrap.GradleDaemon".into(), "8.5".into()], ..Default::default() };
        assert!(is_known_service(&gradle));
    }
}
