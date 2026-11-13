//! Background threads for the sources that are slow or blocking, so the render loop never
//! waits: one long-lived `nettop` stream (per-process network has no public api), the Docker
//! socket, and slow polls of listening ports (libproc), `launchctl list` and `lsappinfo list`.
//! The parsers are plain functions over text.

use crate::native;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct NetData {
    /// pid -> (bytes in per second, bytes out per second)
    pub rates: HashMap<u32, (f64, f64)>,
    /// pid -> bytes in + out since nettop first saw its sockets
    pub totals: HashMap<u32, u64>,
    pub ready: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GuiApp {
    pub asn: String,
    pub name: String,
    pub bundle_id: Option<String>,
    pub bundle: String,
    pub pid: u32,
    /// Has a Dock icon (lsappinfo type "Foreground"); otherwise a menu-bar or agent app.
    pub foreground: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    pub project: String,
    pub status: String,
    pub port: Option<u16>,
    pub cpu: f32,
    pub mem: u64,
    pub rx: f64,
    pub tx: f64,
}

/// Numbers for a process that is not ours, as `ps` reports them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Foreign {
    pub rss: u64,
    pub cpu_ms: u64,
    /// Percent of one core over the last poll interval.
    pub cpu: f32,
    pub path: String,
}

#[derive(Default)]
pub struct Shared {
    pub stop: AtomicBool,
    stop_mx: Mutex<()>,
    stop_cv: std::sync::Condvar,
    pub nettop_pid: AtomicU32,
    pub net: Mutex<NetData>,
    pub ports: Mutex<HashMap<u32, Vec<u16>>>,
    pub launchd: Mutex<HashMap<u32, String>>,
    pub docker: Mutex<Vec<Container>>,
    /// pid -> numbers for other users' processes (root's, system accounts')
    pub foreign: Mutex<HashMap<u32, Foreign>>,
    /// bundle path -> app, for everything LaunchServices knows is running
    pub gui: Mutex<HashMap<String, GuiApp>>,
}

pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct Options {
    pub docker: bool,
    /// Seconds between nettop samples. Each sample costs nettop about 25 ms of cpu.
    pub net_interval: u32,
}

pub fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

impl Shared {
    /// Sleeps for `d`, or less if the program is stopping. Returns false when it is.
    pub fn nap(&self, d: Duration) -> bool {
        let guard = lock(&self.stop_mx);
        if !self.stop.load(Ordering::Relaxed) {
            let _ = self.stop_cv.wait_timeout(guard, d);
        }
        !self.stop.load(Ordering::Relaxed)
    }

    /// Stops the helper process we started. Threads end with the program.
    pub fn shutdown(&self) {
        {
            let _guard = lock(&self.stop_mx);
            self.stop.store(true, Ordering::Relaxed);
        }
        self.stop_cv.notify_all();
        let pid = self.nettop_pid.swap(0, Ordering::Relaxed);
        if pid != 0 {
            // SAFETY: this is the process group of the nettop helper this program spawned.
            unsafe { libc::killpg(pid as libc::pid_t, libc::SIGTERM) };
        }
    }
}

pub fn start(opts: Options) -> Arc<Shared> {
    let shared = Arc::new(Shared::default());
    // first answers before the first frame, so grouping is right from the start
    refresh_gui_apps(&shared);
    refresh_launchd(&shared);
    *lock(&shared.ports) = native::listening_ports();

    let s = shared.clone();
    let interval = opts.net_interval.max(1);
    std::thread::spawn(move || nettop_loop(&s, interval));
    let s = shared.clone();
    std::thread::spawn(move || slow_loop(&s));
    let s = shared.clone();
    std::thread::spawn(move || foreign_loop(&s));
    if opts.docker {
        let s = shared.clone();
        std::thread::spawn(move || docker_loop(&s));
    }
    shared
}

// ───────────────────────── slow polls ─────────────────────────

fn slow_loop(shared: &Shared) {
    let mut n: u64 = 0;
    while shared.nap(Duration::from_secs(6)) {
        n += 1;
        *lock(&shared.ports) = native::listening_ports();
        if n.is_multiple_of(2) {
            refresh_gui_apps(shared);
            refresh_launchd(shared);
        }
    }
}

fn refresh_launchd(shared: &Shared) {
    if let Some(out) = run("launchctl", &["list"]) {
        *lock(&shared.launchd) = parse_launchctl(&out);
    }
}

/// Cpu and memory of other users' processes cannot be read without privileges; `ps` has
/// them (it is setuid). One short run every few seconds, off the render thread.
fn foreign_loop(shared: &Shared) {
    // SAFETY: getuid cannot fail.
    let me = unsafe { libc::getuid() };
    let mut last = Instant::now();
    loop {
        if let Some(out) = run("ps", &["-axo", "pid=,uid=,rss=,time=,comm="]) {
            let now = Instant::now();
            let dt = now.duration_since(last).as_secs_f64().max(0.5);
            last = now;
            let mut map = parse_ps(&out, me);
            let mut cur = lock(&shared.foreign);
            for (pid, f) in map.iter_mut() {
                if let Some(old) = cur.get(pid).filter(|o| o.path == f.path && f.cpu_ms >= o.cpu_ms) {
                    f.cpu = ((f.cpu_ms - old.cpu_ms) as f64 / (dt * 10.0)) as f32;
                }
            }
            *cur = map;
        }
        if !shared.nap(Duration::from_secs(4)) {
            break;
        }
    }
}

/// Lines of `ps -axo pid=,uid=,rss=,time=,comm=`, keeping the processes that are not `me`'s.
pub fn parse_ps(text: &str, me: u32) -> HashMap<u32, Foreign> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let mut rest = line.trim_start();
        let mut next = || {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            let (tok, tail) = rest.split_at(end);
            rest = tail.trim_start();
            tok
        };
        let (Ok(pid), Ok(uid), Ok(rss_kb)) = (next().parse::<u32>(), next().parse::<u32>(), next().parse::<u64>()) else { continue };
        let Some(cpu_ms) = parse_cputime(next()) else { continue };
        if uid != me {
            map.insert(pid, Foreign { rss: rss_kb * 1024, cpu_ms, cpu: 0.0, path: rest.trim_end().to_string() });
        }
    }
    map
}

/// "1:11.57", "28:05.77", "1:02:03.04" or "2-01:02:03.04" -> milliseconds
pub fn parse_cputime(s: &str) -> Option<u64> {
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0, s),
    };
    let (hms, frac) = rest.split_once('.').unwrap_or((rest, "0"));
    let mut secs = 0u64;
    for part in hms.split(':') {
        secs = secs * 60 + part.parse::<u64>().ok()?;
    }
    let cs: u64 = frac.get(..2).unwrap_or(frac).parse().ok()?;
    Some((days * 86_400 + secs) * 1000 + cs * 10)
}

pub fn refresh_gui_apps(shared: &Shared) {
    if let Some(out) = run("lsappinfo", &["list"]) {
        *lock(&shared.gui) = parse_lsappinfo(&out).into_iter().map(|a| (a.bundle.clone(), a)).collect();
    }
}

pub fn parse_launchctl(text: &str) -> HashMap<u32, String> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let pid = f.next()?.trim().parse::<u32>().ok()?;
            let label = f.nth(1)?.trim();
            Some((pid, label.to_string()))
        })
        .collect()
}

fn quoted(s: &str) -> Option<&str> {
    let a = s.find('"')? + 1;
    let b = s[a..].find('"')? + a;
    Some(&s[a..b])
}

pub fn parse_lsappinfo(text: &str) -> Vec<GuiApp> {
    let mut out: Vec<GuiApp> = Vec::new();
    for line in text.lines() {
        let t = line.trim_start();
        let is_header = t.split_once(") \"").is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if is_header {
            let name = quoted(t).unwrap_or("").to_string();
            let asn = t.split_whitespace().find(|w| w.starts_with("ASN:")).unwrap_or("").to_string();
            out.push(GuiApp { name, asn, ..Default::default() });
            continue;
        }
        let Some(app) = out.last_mut() else { continue };
        if let Some(rest) = t.strip_prefix("bundleID=") {
            app.bundle_id = quoted(rest).map(str::to_string);
        } else if let Some(rest) = t.strip_prefix("bundle path=") {
            app.bundle = quoted(rest).unwrap_or("").to_string();
        } else if let Some(rest) = t.strip_prefix("pid = ") {
            app.pid = rest.split_whitespace().next().and_then(|p| p.parse().ok()).unwrap_or(0);
            app.foreground = rest.contains("type=\"Foreground\"");
        }
    }
    out.retain(|a| a.bundle.ends_with(".app") && a.pid != 0);
    out
}

// ───────────────────────── nettop ─────────────────────────

/// nettop only streams when it thinks it is writing to a terminal, so it gets a pty.
fn nettop_loop(shared: &Shared, interval: u32) {
    while !shared.stop.load(Ordering::Relaxed) {
        let (mut master, mut slave) = (0, 0);
        // SAFETY: openpty fills the two descriptors; null means default settings.
        if unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) } != 0 {
            return;
        }
        // SAFETY: both descriptors are fresh and owned here; each is wrapped exactly once.
        let (reader, out) = unsafe { (std::fs::File::from_raw_fd(master), Stdio::from_raw_fd(slave)) };
        // The small shell around nettop is a dead man's switch: it waits on a pipe from us and
        // stops nettop when the pipe closes, so nettop cannot outlive parasight even if we are
        // killed outright. Its own process group lets `shutdown` stop both at once.
        use std::os::unix::process::CommandExt;
        // nettop's stdin is the pty too (`<&1`): with /dev/null there it spins a whole core
        // re-reading end-of-file.
        let script = format!("nettop -P -n -L 0 -x -J bytes_in,bytes_out -s {interval} <&1 & n=$!; read _; kill $n 2>/dev/null");
        let child = Command::new("/bin/sh").args(["-c", &script]).process_group(0).stdin(Stdio::piped()).stdout(out).stderr(Stdio::null()).spawn();
        let Ok(mut child) = child else { return };
        let _switch = child.stdin.take();
        shared.nettop_pid.store(child.id(), Ordering::Relaxed);

        let mut prev: HashMap<u32, (u64, u64)> = HashMap::new();
        let mut cur: HashMap<u32, (u64, u64)> = HashMap::new();
        let (mut t_prev, mut t_cur) = (Instant::now(), Instant::now());
        let mut blocks = 0;
        for line in BufReader::new(reader).split(b'\n') {
            let Ok(line) = line else { break };
            let line = String::from_utf8_lossy(&line);
            let line = line.trim();
            if line.starts_with(",bytes_in") {
                blocks += 1;
                t_prev = t_cur;
                t_cur = Instant::now();
                prev = std::mem::take(&mut cur);
                let mut net = lock(&shared.net);
                if blocks >= 2 {
                    net.rates.retain(|pid, _| prev.contains_key(pid));
                    net.totals.retain(|pid, _| prev.contains_key(pid));
                    net.ready = true;
                }
                continue;
            }
            let Some((pid, bin, bout)) = parse_nettop_line(line) else { continue };
            cur.insert(pid, (bin, bout));
            let dt = t_cur.duration_since(t_prev).as_secs_f64();
            let rate = match prev.get(&pid) {
                Some(&(pin, pout)) if dt > 0.2 => (bin.saturating_sub(pin) as f64 / dt, bout.saturating_sub(pout) as f64 / dt),
                _ => (0.0, 0.0),
            };
            let mut net = lock(&shared.net);
            net.rates.insert(pid, rate);
            net.totals.insert(pid, bin + bout);
        }
        let _ = child.kill();
        let _ = child.wait();
        if !shared.nap(Duration::from_secs(5)) {
            break;
        }
    }
}

/// "Google Chrome H.1234,5678,910," -> (1234, 5678, 910)
pub fn parse_nettop_line(line: &str) -> Option<(u32, u64, u64)> {
    let mut f = line.trim_end_matches(',').rsplitn(3, ',');
    let bout = f.next()?.trim().parse().ok()?;
    let bin = f.next()?.trim().parse().ok()?;
    let pid = f.next()?.rsplit('.').next()?.trim().parse().ok()?;
    Some((pid, bin, bout))
}

// ───────────────────────── docker ─────────────────────────

/// Where a Docker-compatible daemon may be listening. The first that answers wins.
fn docker_sockets() -> Vec<String> {
    let home = native::home();
    let mut v = Vec::new();
    if let Ok(h) = std::env::var("DOCKER_HOST")
        && let Some(p) = h.strip_prefix("unix://")
    {
        v.push(p.to_string());
    }
    v.push("/var/run/docker.sock".into());
    for rel in [".docker/run/docker.sock", ".colima/default/docker.sock", ".orbstack/run/docker.sock", ".rd/docker.sock", ".lima/default/sock/docker.sock", ".local/share/containers/podman/machine/podman.sock"] {
        v.push(format!("{home}/{rel}"));
    }
    if let Ok(dir) = std::fs::read_dir(format!("{home}/.colima")) {
        for e in dir.flatten() {
            let p = e.path().join("docker.sock");
            if let Some(p) = p.to_str().filter(|p| !v.iter().any(|x| x == p)) {
                v.push(p.to_string());
            }
        }
    }
    v
}

/// One HTTP/1.0 request over the Docker socket: returns (status, body). No keep-alive, no
/// chunking, so the whole answer is simply everything until the daemon closes the socket.
pub fn docker_request(sock: &str, method: &str, path: &str, timeout: Duration) -> Option<(u16, Vec<u8>)> {
    let mut s = UnixStream::connect(sock).ok()?;
    s.set_read_timeout(Some(timeout)).ok()?;
    s.set_write_timeout(Some(timeout)).ok()?;
    write!(s, "{method} {path} HTTP/1.0\r\nHost: docker\r\nContent-Length: 0\r\n\r\n").ok()?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).ok()?;
    parse_http(&buf)
}

pub fn parse_http(buf: &[u8]) -> Option<(u16, Vec<u8>)> {
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&buf[..split]).ok()?;
    let status = head.split_whitespace().nth(1)?.parse().ok()?;
    Some((status, buf[split + 4..].to_vec()))
}

pub fn docker_socket() -> Option<String> {
    docker_sockets().into_iter().find(|p| std::path::Path::new(p).exists() && UnixStream::connect(p).is_ok())
}

fn docker_loop(shared: &Shared) {
    // id -> (cpu ns, rx bytes, tx bytes, when)
    let mut prev: HashMap<String, (u64, u64, u64, Instant)> = HashMap::new();
    let mut sock: Option<String> = None;
    loop {
        if sock.is_none() {
            sock = docker_socket();
        }
        let list = sock.as_deref().and_then(|s| docker_request(s, "GET", "/containers/json", Duration::from_secs(4))).filter(|(st, _)| *st == 200).and_then(|(_, body)| serde_json::from_slice::<serde_json::Value>(&body).ok());
        let Some(list) = list else {
            // no docker, or the daemon is down: nothing to show, look again later
            sock = None;
            lock(&shared.docker).clear();
            if !shared.nap(Duration::from_secs(20)) {
                break;
            }
            continue;
        };
        let mut out = parse_containers(&list);
        for c in &mut out {
            let path = format!("/containers/{}/stats?stream=false&one-shot=true", c.id);
            let stats = sock.as_deref().and_then(|s| docker_request(s, "GET", &path, Duration::from_secs(4))).filter(|(st, _)| *st == 200).and_then(|(_, b)| serde_json::from_slice::<serde_json::Value>(&b).ok());
            let Some(stats) = stats else { continue };
            let (cpu_ns, mem, rx, tx) = parse_container_stats(&stats);
            let now = Instant::now();
            c.mem = mem;
            if let Some(&(pcpu, prx, ptx, t)) = prev.get(&c.id) {
                let dt = now.duration_since(t).as_secs_f64().max(0.5);
                c.cpu = (cpu_ns.saturating_sub(pcpu) as f64 / (dt * 1e9) * 100.0) as f32;
                c.rx = rx.saturating_sub(prx) as f64 / dt;
                c.tx = tx.saturating_sub(ptx) as f64 / dt;
            }
            prev.insert(c.id.clone(), (cpu_ns, rx, tx, now));
        }
        prev.retain(|id, _| out.iter().any(|c| &c.id == id));
        *lock(&shared.docker) = out;
        if !shared.nap(Duration::from_secs(3)) {
            break;
        }
    }
}

/// The answer of `GET /containers/json`.
pub fn parse_containers(v: &serde_json::Value) -> Vec<Container> {
    let Some(list) = v.as_array() else { return Vec::new() };
    list.iter()
        .filter_map(|c| {
            let id = c["Id"].as_str()?.to_string();
            let label = |k: &str| c["Labels"][k].as_str().unwrap_or("").to_string();
            let service = label("com.docker.compose.service");
            let first_name = c["Names"][0].as_str().unwrap_or("").trim_start_matches('/').to_string();
            let port = c["Ports"].as_array().and_then(|ps| ps.iter().filter_map(|p| p["PublicPort"].as_u64()).min()).map(|p| p as u16);
            Some(Container {
                name: if service.is_empty() { first_name } else { service },
                image: c["Image"].as_str().unwrap_or("").to_string(),
                project: label("com.docker.compose.project"),
                status: c["Status"].as_str().unwrap_or("").to_string(),
                port,
                id,
                ..Default::default()
            })
        })
        .collect()
}

/// (cpu nanoseconds, memory bytes, rx bytes, tx bytes) from `GET /containers/<id>/stats`.
pub fn parse_container_stats(v: &serde_json::Value) -> (u64, u64, u64, u64) {
    let cpu = v["cpu_stats"]["cpu_usage"]["total_usage"].as_u64().unwrap_or(0);
    let m = &v["memory_stats"];
    // what `docker stats` shows: usage without the reclaimable file cache
    let cache = m["stats"]["inactive_file"].as_u64().or(m["stats"]["total_inactive_file"].as_u64()).unwrap_or(0);
    let mem = m["usage"].as_u64().unwrap_or(0).saturating_sub(cache);
    let (mut rx, mut tx) = (0, 0);
    if let Some(nets) = v["networks"].as_object() {
        for n in nets.values() {
            rx += n["rx_bytes"].as_u64().unwrap_or(0);
            tx += n["tx_bytes"].as_u64().unwrap_or(0);
        }
    }
    (cpu, mem, rx, tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nettop_lines() {
        assert_eq!(parse_nettop_line("apsd.401,2484955,754408,"), Some((401, 2484955, 754408)));
        assert_eq!(parse_nettop_line("Google Chrome H.1234,5,6,"), Some((1234, 5, 6)));
        assert_eq!(parse_nettop_line("com.apple.WebKi.77,0,0,"), Some((77, 0, 0)));
        assert_eq!(parse_nettop_line(",bytes_in,bytes_out,"), None);
    }

    #[test]
    fn ps_lines() {
        assert_eq!(parse_cputime("1:11.57"), Some(71_570));
        assert_eq!(parse_cputime("1:02:03.04"), Some(3_723_040));
        assert_eq!(parse_cputime("2-00:00:01.00"), Some(172_801_000));
        assert_eq!(parse_cputime("x"), None);
        let text = "  432    88 145744  28:05.77 /System/Library/PrivateFrameworks/SkyLight.framework/Resources/WindowServer\n    0     0   9000 120:00.00 kernel_task\n 1115   501 500000   3:00.00 /Applications/Slack.app/Contents/MacOS/Slack\n  900     0  1024   0:00.10 /Library/Some Dir/tool helper\n";
        let m = parse_ps(text, 501);
        assert_eq!(m.len(), 3);
        assert_eq!(m[&432], Foreign { rss: 145_744 * 1024, cpu_ms: 1_685_770, cpu: 0.0, path: "/System/Library/PrivateFrameworks/SkyLight.framework/Resources/WindowServer".into() });
        assert_eq!(m[&0].path, "kernel_task");
        assert_eq!(m[&900].path, "/Library/Some Dir/tool helper");
    }

    #[test]
    fn launchctl_list() {
        let m = parse_launchctl("PID\tStatus\tLabel\n-\t0\tcom.apple.thing\n812\t0\thomebrew.mxcl.postgresql@16\n");
        assert_eq!(m.len(), 1);
        assert_eq!(m[&812], "homebrew.mxcl.postgresql@16");
    }

    #[test]
    fn lsappinfo_list() {
        let text = r#" 6) "Dock" ASN:0x0-0xb00b: 
    bundleID="com.apple.dock"
    bundle path="/System/Library/CoreServices/Dock.app"
    executable path="/System/Library/CoreServices/Dock.app/Contents/MacOS/Dock"
    pid = 660 type="UIElement" flavor=3 Version="1" fileType="APPL" creator="dock" Arch=ARM64 
 3) "universalaccessd" ASN:0x0-0x8008: 
    bundleID=[ NULL ] 
    bundle path="/usr/sbin/universalaccessd"
    pid = 648 !signalled type="BackgroundOnly" flavor=3 Version=[ NULL ]
43) "Zen" ASN:0x0-0x33033: (in front) 
    bundleID="app.zen-browser.zen"
    bundle path="/Applications/Zen.app"
    executable path="/Applications/Zen.app/Contents/MacOS/zen"
    pid = 1114 type="Foreground" flavor=3 Version="126.11.6" fileType="APPL" creator="MOZB" Arch=ARM64 
    parentASN="Dock" ASN:0x0-0xb00b: 
"#;
        let apps = parse_lsappinfo(text);
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].name, "Dock");
        assert!(!apps[0].foreground);
        let zen = &apps[1];
        assert_eq!((zen.asn.as_str(), zen.pid, zen.foreground), ("ASN:0x0-0x33033:", 1114, true));
        assert_eq!(zen.bundle_id.as_deref(), Some("app.zen-browser.zen"));
        assert_eq!(zen.bundle, "/Applications/Zen.app");
    }

    #[test]
    fn docker_json() {
        let list: serde_json::Value = serde_json::from_str(
            r#"[{"Id":"415f72","Names":["/orca-api-1"],"Image":"my-api:dev","Status":"Up 2 hours (healthy)",
                 "Ports":[{"PrivatePort":8787,"PublicPort":18787,"Type":"tcp"},{"PrivatePort":8788,"PublicPort":18788,"Type":"tcp"}],
                 "Labels":{"com.docker.compose.project":"my-app","com.docker.compose.service":"api"}},
                {"Id":"782b34","Names":["/postgres"],"Image":"postgres:18","Status":"Up 2 hours","Ports":[{"PrivatePort":5432,"Type":"tcp"}],"Labels":{}}]"#,
        )
        .unwrap();
        let c = parse_containers(&list);
        assert_eq!(c.len(), 2);
        assert_eq!((c[0].name.as_str(), c[0].port, c[0].project.as_str()), ("api", Some(18787), "my-app"));
        assert_eq!((c[1].name.as_str(), c[1].port), ("postgres", None));
        let stats: serde_json::Value = serde_json::from_str(
            r#"{"cpu_stats":{"cpu_usage":{"total_usage":5000000000}},"memory_stats":{"usage":30000000,"stats":{"inactive_file":10000000}},
                "networks":{"eth0":{"rx_bytes":100,"tx_bytes":200},"eth1":{"rx_bytes":1,"tx_bytes":2}}}"#,
        )
        .unwrap();
        assert_eq!(parse_container_stats(&stats), (5_000_000_000, 20_000_000, 101, 202));
    }

    #[test]
    fn http_answers() {
        assert_eq!(parse_http(b"HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n[1]"), Some((200, b"[1]".to_vec())));
        assert_eq!(parse_http(b"HTTP/1.0 204 No Content\r\n\r\n"), Some((204, Vec::new())));
        assert_eq!(parse_http(b"garbage"), None);
    }
}
