//! Plain data shared by the collector, the classifier, the clean-up scorer and the ui.

use std::collections::VecDeque;

/// History length: 60 samples at 2 s is two minutes.
pub const HIST_CAP: usize = 60;
pub const TICK_MS: u64 = 2000;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum Group {
    Apps,
    Dev,
    System,
}

impl Group {
    pub const ALL: [Group; 3] = [Group::Apps, Group::Dev, Group::System];
    pub fn name(self) -> &'static str {
        match self {
            Group::Apps => "apps",
            Group::Dev => "dev",
            Group::System => "system",
        }
    }
    pub fn id(self) -> String {
        format!("g:{}", self.name())
    }
    pub fn parse(s: &str) -> Option<Group> {
        match s.trim().to_lowercase().as_str() {
            "apps" | "app" => Some(Group::Apps),
            "dev" => Some(Group::Dev),
            "system" | "sys" => Some(Group::System),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Metric {
    Cpu,
    Mem,
    Net,
}

impl Metric {
    pub const ALL: [Metric; 3] = [Metric::Cpu, Metric::Mem, Metric::Net];
    pub fn name(self) -> &'static str {
        match self {
            Metric::Cpu => "cpu",
            Metric::Mem => "mem",
            Metric::Net => "net",
        }
    }
}

/// One process as the classifier sees it. Everything is plain data so tests can build fixtures.
#[derive(Clone, Debug, Default)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub uid: u32,
    pub name: String,
    /// Full executable path, empty when it could not be read.
    pub exe: String,
    pub cmd: Vec<String>,
    pub cwd: String,
    /// Seconds since the epoch.
    pub start_time: u64,
    /// Percent of one core.
    pub cpu: f32,
    pub mem: u64,
    pub cpu_time_ms: u64,
    pub zombie: bool,
    pub threads: u32,
    /// Bytes per second.
    pub rx: f64,
    pub tx: f64,
    /// Set when launchd manages this pid (it shows up in `launchctl list`).
    pub launchd_label: Option<String>,
    /// Bookkeeping for the collector: the refresh that last saw this process.
    pub seen: u64,
}

#[derive(Clone, Debug, Default)]
pub struct ProcRow {
    pub pid: u32,
    pub start_time: u64,
    pub name: String,
    pub user: String,
    pub cpu: f32,
    pub mem: u64,
    pub rx: f64,
    pub tx: f64,
    pub threads: u32,
    pub cpu_time_ms: u64,
    pub zombie: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ItemKind {
    App { bundle: String },
    Dev { root_pid: u32 },
    Container { id: String },
    System,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub key: String,
    pub name: String,
    pub group: Group,
    /// A short description of the item, shown with its path in the detail pane.
    pub path: String,
    pub kind: ItemKind,
    pub procs: Vec<ProcRow>,
    pub cpu: f32,
    pub mem: u64,
    pub rx: f64,
    pub tx: f64,
}

impl Item {
    pub fn metric(&self, m: Metric) -> f64 {
        match m {
            Metric::Cpu => self.cpu as f64,
            Metric::Mem => self.mem as f64,
            Metric::Net => self.rx + self.tx,
        }
    }
    pub fn matches(&self, q: &str) -> bool {
        q.is_empty()
            || self.name.to_lowercase().contains(q)
            || self.procs.iter().any(|p| p.name.to_lowercase().contains(q))
    }
}

#[derive(Clone, Debug, Default)]
pub struct Totals {
    pub cpu: f32,
    pub mem: u64,
    pub rx: f64,
    pub tx: f64,
    pub procs: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Hist {
    pub cpu: VecDeque<f32>,
    pub mem: VecDeque<f32>,
    pub rx: VecDeque<f32>,
    pub tx: VecDeque<f32>,
}

impl Hist {
    pub fn push(&mut self, cpu: f32, mem: u64, rx: f64, tx: f64) {
        fn p(q: &mut VecDeque<f32>, v: f32) {
            if q.len() >= HIST_CAP {
                q.pop_front();
            }
            q.push_back(v);
        }
        p(&mut self.cpu, cpu);
        p(&mut self.mem, mem as f32);
        p(&mut self.rx, rx as f32);
        p(&mut self.tx, tx as f32);
    }
}

/// One refresh worth of data.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub items: Vec<Item>,
}

impl Snapshot {
    pub fn group_items(&self, g: Group) -> impl Iterator<Item = &Item> {
        self.items.iter().filter(move |i| i.group == g)
    }
    pub fn totals(&self, g: Group) -> Totals {
        let mut t = Totals::default();
        for i in self.group_items(g) {
            t.cpu += i.cpu;
            t.mem += i.mem;
            t.rx += i.rx;
            t.tx += i.tx;
            t.procs += i.procs.len();
        }
        t
    }
    pub fn item(&self, key: &str) -> Option<&Item> {
        self.items.iter().find(|i| i.key == key)
    }
}
