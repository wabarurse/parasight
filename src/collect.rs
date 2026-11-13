//! Turns raw process data into the items the ui shows, once per tick.

use crate::actions::{Target, TargetKind};
use crate::classify::{self, Ctx, Kind};
use crate::model::*;
use crate::native;
use crate::procs;
use crate::rules::Rules;
use crate::sources::{Shared, lock};
use crate::util::{base, clip, tilde};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;
use sysinfo::System;

#[derive(Default)]
pub struct Machine {
    pub cpu: f32,
    pub cpu_hist: std::collections::VecDeque<f32>,
    pub mem_used: u64,
    pub mem_total: u64,
    pub rx: f64,
    pub tx: f64,
}

pub struct Collector {
    sys: System,
    pub shared: Arc<Shared>,
    pub rules: Rules,
    pub home: String,
    procs: HashMap<u32, ProcInfo>,
    /// pid -> cpu time at the last refresh, to turn into a percentage
    cpu_ns: HashMap<u32, u64>,
    last_refresh: Option<Instant>,
    round: u64,
    users: HashMap<u32, String>,
    pub snapshot: Snapshot,
    pub hist: HashMap<String, Hist>,
    pub machine: Machine,
    ticks: u64,
}

impl Collector {
    pub fn new(shared: Arc<Shared>, rules: Rules) -> Collector {
        Collector {
            sys: System::new().unwrap_or_else(|_| panic!("sysinfo could not start")),
            shared,
            rules,
            home: native::home(),
            procs: HashMap::new(),
            cpu_ns: HashMap::new(),
            last_refresh: None,
            round: 0,
            users: HashMap::new(),
            snapshot: Snapshot::default(),
            hist: HashMap::new(),
            machine: Machine::default(),
            ticks: 0,
        }
    }

    fn refresh_procs(&mut self) {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();

        let now = Instant::now();
        let dt_ns = self.last_refresh.map(|t| now.duration_since(t).as_nanos() as f64).filter(|&d| d > 1e6);
        self.last_refresh = Some(now);
        // SAFETY: getuid cannot fail.
        let me = unsafe { libc::getuid() };
        // Path, command line and cwd cost extra syscalls, so they are read when a process first
        // appears and again now and then (a shell that execs keeps its pid but becomes
        // something else; a shell that cds changes its cwd).
        let recheck = self.ticks % 8 == 7;
        let net = lock(&self.shared.net);
        let launchd = lock(&self.shared.launchd);
        let foreign = lock(&self.shared.foreign);
        self.round = self.round.wrapping_add(1);
        let generation = self.round;

        for pid in native::all_pids() {
            let known = self.procs.get(&pid);
            // one syscall for a process of ours; for anyone else's, the short form
            let mine = known.is_none_or(|e| e.uid == me);
            let task = if mine { procs::task(pid) } else { None };
            let e = match &task {
                Some(t) => {
                    let fresh = known.is_none_or(|e| e.start_time != t.start_time);
                    let e = self.procs.entry(pid).or_default();
                    if fresh {
                        *e = ProcInfo { pid, start_time: t.start_time, uid: t.uid, name: t.name.clone(), ..Default::default() };
                        self.cpu_ns.remove(&pid);
                    }
                    if fresh || recheck {
                        let exe = procs::exe(pid).unwrap_or_default();
                        // an exec replaces the program under the same pid: read its arguments again
                        if fresh || exe != e.exe {
                            e.cmd = procs::cmdline(pid);
                            // the kernel's process name is cut at 32 characters; the file name is not
                            e.name = if exe.is_empty() { t.name.clone() } else { base(&exe).to_string() };
                            e.exe = exe;
                        }
                        e.cwd = procs::cwd(pid).unwrap_or_default();
                    }
                    e.ppid = t.ppid;
                    e.threads = t.threads;
                    e.zombie = false;
                    e.mem = procs::footprint(pid).unwrap_or(t.resident);
                    e.cpu_time_ms = t.cpu_ns / 1_000_000;
                    let before = self.cpu_ns.insert(pid, t.cpu_ns);
                    e.cpu = match (before, dt_ns) {
                        (Some(b), Some(dt)) => (t.cpu_ns.saturating_sub(b) as f64 / dt * 100.0) as f32,
                        _ => 0.0,
                    };
                    e
                }
                None => {
                    // not ours (or a zombie of ours): the kernel still says whose it is, and
                    // `ps` supplies the numbers a few seconds apart
                    let Some(sh) = procs::short(pid) else { continue };
                    let e = self.procs.entry(pid).or_default();
                    // (the short name is cut at 16 characters, so compare only that much)
                    if e.pid != pid || e.uid != sh.uid || !e.name.starts_with(&sh.name) || e.start_time != 0 {
                        *e = ProcInfo { pid, uid: sh.uid, name: sh.name, ..Default::default() };
                    }
                    e.ppid = sh.ppid;
                    e.zombie = sh.zombie;
                    match foreign.get(&pid) {
                        Some(f) => {
                            if e.exe.is_empty() && f.path.starts_with('/') {
                                e.exe = f.path.clone();
                                e.name = base(&f.path).to_string();
                            }
                            e.mem = f.rss;
                            e.cpu = f.cpu;
                            e.cpu_time_ms = f.cpu_ms;
                        }
                        None => (e.mem, e.cpu) = (0, 0.0),
                    }
                    e
                }
            };
            e.pid = pid;
            e.seen = generation;
            (e.rx, e.tx) = net.rates.get(&pid).copied().unwrap_or((0.0, 0.0));
            let label = launchd.get(&pid);
            if e.launchd_label.as_ref() != label {
                e.launchd_label = label.cloned();
            }
        }
        self.procs.retain(|_, e| e.seen == generation);
        if recheck {
            let procs = &self.procs;
            self.cpu_ns.retain(|pid, _| procs.contains_key(pid));
        }
    }

    fn user(&mut self, uid: u32) -> String {
        self.users.entry(uid).or_insert_with(|| native::user_name(uid)).clone()
    }

    fn row(&mut self, pid: u32, dev: bool) -> ProcRow {
        let p = &self.procs[&pid];
        let name = if dev && !p.cmd.is_empty() {
            let mut s = base(&p.cmd[0]).trim_start_matches('-').to_string();
            for a in &p.cmd[1..] {
                if s.len() > 90 {
                    break;
                }
                s.push(' ');
                s.push_str(&tilde(a, &self.home));
            }
            s
        } else if !p.exe.is_empty() {
            base(&p.exe).to_string()
        } else {
            p.name.clone()
        };
        let (uid, row) = (
            p.uid,
            ProcRow { pid, start_time: p.start_time, name, user: String::new(), cpu: p.cpu, mem: p.mem, rx: p.rx, tx: p.tx, threads: p.threads, cpu_time_ms: p.cpu_time_ms, zombie: p.zombie },
        );
        ProcRow { user: self.user(uid), ..row }
    }

    /// One refresh: read processes, classify, roll up into items, push history.
    pub fn tick(&mut self) {
        self.refresh_procs();
        let gui_bundles: HashSet<String> = lock(&self.shared.gui).values().filter(|a| a.foreground).map(|a| a.bundle.clone()).collect();
        let ports = lock(&self.shared.ports).clone();

        let mut members: HashMap<String, (Kind, Group, Vec<u32>)> = HashMap::new();
        {
            let ctx = Ctx { procs: &self.procs, gui_bundles: &gui_bundles, rules: &self.rules };
            for p in self.procs.values() {
                let c = classify::classify(p, &ctx);
                members.entry(c.key).or_insert_with(|| (c.kind, c.group, Vec::new())).2.push(p.pid);
            }
        }

        let mut items: Vec<Item> = Vec::with_capacity(members.len() + 8);
        for (key, (kind, group, mut pids)) in members {
            pids.sort_unstable();
            let (name, path, item_kind);
            match &kind {
                Kind::App { bundle } => {
                    name = classify::app_name(bundle).to_string();
                    path = bundle.clone();
                    item_kind = ItemKind::App { bundle: bundle.clone() };
                    // the main process first: it is the one that gets asked to quit
                    let main = lock(&self.shared.gui).get(bundle).map(|a| a.pid);
                    let direct = format!("{bundle}/Contents/MacOS/");
                    let main = main.filter(|m| pids.contains(m)).or_else(|| pids.iter().copied().find(|p| self.procs[p].exe.starts_with(&direct)));
                    if let Some(m) = main {
                        pids.retain(|&p| p != m);
                        pids.insert(0, m);
                    }
                }
                Kind::Dev { root_pid } => {
                    let root_pid = if self.procs.contains_key(root_pid) { *root_pid } else { pids[0] };
                    let root = &self.procs[&root_pid];
                    let mem: Vec<&ProcInfo> = pids.iter().map(|p| &self.procs[p]).collect();
                    let command = classify::dev_item_name(root, &mem);
                    let port = pids.iter().filter_map(|p| ports.get(p)).flatten().min();
                    name = match port {
                        Some(port) => format!("{command}  :{port}"),
                        None => command,
                    };
                    let cmdline = clip(&root.cmd.iter().map(|a| tilde(a, &self.home)).collect::<Vec<_>>().join(" "), 90);
                    let cwd = tilde(&root.cwd, &self.home);
                    path = match &root.launchd_label {
                        Some(l) if l.starts_with("homebrew.mxcl.") => format!("brew services · {}", tilde(&root.exe, &self.home)),
                        Some(l) => format!("launchd {l} · {cmdline}"),
                        None if cwd.is_empty() => cmdline,
                        None => format!("{cwd} · {cmdline}"),
                    };
                    pids.retain(|&p| p != root_pid);
                    pids.insert(0, root_pid);
                    item_kind = ItemKind::Dev { root_pid };
                }
                Kind::System => {
                    name = key.split_once(':').map(|(_, n)| n).unwrap_or(&key).to_string();
                    let big = pids.iter().map(|p| &self.procs[p]).max_by_key(|p| p.mem);
                    path = big.map(|p| if p.exe.is_empty() { "(no path: owned by another user)".to_string() } else { p.exe.clone() }).unwrap_or_default();
                    item_kind = ItemKind::System;
                }
            }
            let dev = group == Group::Dev;
            let procs: Vec<ProcRow> = pids.iter().map(|&p| self.row(p, dev)).collect();
            let mut it = Item { key, name, group, path, kind: item_kind, procs, cpu: 0.0, mem: 0, rx: 0.0, tx: 0.0 };
            for p in &it.procs {
                it.cpu += p.cpu;
                it.mem += p.mem;
                it.rx += p.rx;
                it.tx += p.tx;
            }
            items.push(it);
        }

        for c in lock(&self.shared.docker).iter() {
            let name = match c.port {
                Some(p) => format!("{}  :{p}", c.name),
                None => c.name.clone(),
            };
            let mut path = format!("container · image {}", c.image);
            if !c.project.is_empty() {
                path.push_str(&format!(" · compose {}", c.project));
            }
            path.push_str(&format!(" · {}", c.status));
            let row = ProcRow { name: format!("{} (container)", c.name), user: "docker".into(), cpu: c.cpu, mem: c.mem, rx: c.rx, tx: c.tx, ..Default::default() };
            items.push(Item { key: format!("docker:{}", c.id), name, group: Group::Dev, path, kind: ItemKind::Container { id: c.id.clone() }, procs: vec![row], cpu: c.cpu, mem: c.mem, rx: c.rx, tx: c.tx });
        }
        items.sort_by(|a, b| a.key.cmp(&b.key));

        // history
        for it in &items {
            self.hist.entry(it.key.clone()).or_default().push(it.cpu, it.mem, it.rx, it.tx);
        }
        self.snapshot = Snapshot { items };
        for g in Group::ALL {
            let t = self.snapshot.totals(g);
            self.hist.entry(g.id()).or_default().push(t.cpu, t.mem, t.rx, t.tx);
        }
        if self.ticks % 16 == 15 {
            let snap = &self.snapshot;
            self.hist.retain(|k, _| k.starts_with("g:") || snap.items.iter().any(|i| &i.key == k));
        }

        // machine totals
        let m = &mut self.machine;
        m.cpu = self.sys.global_cpu_usage();
        if m.cpu_hist.len() >= HIST_CAP {
            m.cpu_hist.pop_front();
        }
        m.cpu_hist.push_back(m.cpu);
        m.mem_used = self.sys.used_memory();
        m.mem_total = self.sys.total_memory();
        m.rx = self.procs.values().map(|p| p.rx).sum();
        m.tx = self.procs.values().map(|p| p.tx).sum();

        self.ticks += 1;
    }

    pub fn procs(&self) -> &HashMap<u32, ProcInfo> {
        &self.procs
    }

    /// How to close an item (or one process of it).
    pub fn target(&self, key: &str) -> Option<Target> {
        let it = self.snapshot.item(key)?;
        let pids: Vec<(u32, u64)> = it.procs.iter().map(|p| (p.pid, p.start_time)).collect();
        let kind = match &it.kind {
            ItemKind::App { bundle } => {
                let gui = lock(&self.shared.gui);
                TargetKind::App { bundle_id: gui.get(bundle).and_then(|a| a.bundle_id.clone()) }
            }
            ItemKind::Container { id } => TargetKind::Container { id: id.clone() },
            _ => TargetKind::Tree,
        };
        Some(Target { name: it.name.clone(), kind, pids })
    }
}
