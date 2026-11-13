//! Clean-up scoring: plain facts in, ranked candidates out. Nothing here touches the machine.

use crate::model::Group;
use crate::util::dur;

pub const MIN: u64 = 60;
pub const HOUR: u64 = 3600;
pub const DAY: u64 = 86_400;

pub const ORPHAN_UNUSED: u64 = 10 * MIN;
/// An orphan using this much of a core is doing something: listed, but never pre-checked.
pub const BUSY_CPU: f32 = 5.0;
pub const DEV_UNUSED: u64 = 2 * HOUR;
pub const SERVICE_IDLE: u64 = DAY;
pub const CONTAINER_IDLE: u64 = 6 * HOUR;
pub const APP_UNUSED: u64 = 2 * HOUR;
pub const APP_UNUSED_LONG: u64 = DAY;
pub const APP_OPENED_LONG_AGO: u64 = 3 * DAY;

/// Everything the scorer knows about one item.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    pub key: String,
    pub name: String,
    pub group: Option<Group>,
    pub mem: u64,
    /// Current cpu (percent of one core) and network (bytes per second, down + up).
    pub cpu: f32,
    pub net: f64,

    // never offered
    /// parasight, its shell, or the terminal app it runs in.
    pub is_self: bool,
    pub frontmost: bool,
    /// Finder, Dock, loginwindow.
    pub protected: bool,

    /// Time since the user last interacted with it. Apps: keyboard, click or scroll while it
    /// was frontmost. Dev: typing into its terminal, or network traffic. Containers: cpu or
    /// network. `None` when nothing is known.
    pub unused_secs: Option<u64>,
    /// A use was actually recorded. When false, `unused_secs` is only how long tracking has
    /// watched it without seeing any.
    pub input_seen: bool,

    // dev
    /// Reparented to launchd (ppid 1) without being a launchd job.
    pub orphaned: bool,
    pub launchd: bool,
    /// A known background service (postgres, redis, tmux...) even when not run by launchd.
    pub service: bool,
    pub container: bool,
    pub zombie: bool,
    pub age_secs: u64,
    pub cwd: String,
    /// The command without its port, used to spot duplicates.
    pub command: String,
    /// It listens on a port: only servers are compared for duplicates.
    pub has_port: bool,

    // apps
    pub pinned: bool,
    /// Menu-bar or agent app with no Dock icon.
    pub background_only: bool,
    /// Spotlight's last-opened date, as seconds ago.
    pub last_opened_secs: Option<u64>,
    /// Holds an audio or prevent-sleep power assertion (playing music, on a call).
    pub holds_assertion: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    Low,
    Medium,
    High,
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub key: String,
    pub name: String,
    pub group: Group,
    pub reason: String,
    pub mem: u64,
    pub cpu: f32,
    pub net: f64,
    pub confidence: Confidence,
    pub pinned: bool,
    /// Checked: will be closed on confirm. Only high confidence starts checked.
    pub ticked: bool,
}

/// The popup shows exactly two groups.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    /// High confidence, checked for the user.
    Recommended,
    /// Medium and low together, unchecked.
    ToReview,
}

impl Candidate {
    pub fn section(&self) -> Section {
        if self.confidence == Confidence::High { Section::Recommended } else { Section::ToReview }
    }
}

/// What closing the checked rows would save.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Savings {
    pub count: usize,
    pub cpu: f32,
    pub mem: u64,
    pub net: f64,
}

pub fn savings(cands: &[Candidate]) -> Savings {
    let mut s = Savings::default();
    for c in cands.iter().filter(|c| c.ticked) {
        s.count += 1;
        s.cpu += c.cpu;
        s.mem += c.mem;
        s.net += c.net;
    }
    s
}

fn kept(f: &Facts, keep: &[String]) -> bool {
    let name = f.name.to_lowercase();
    let cmd = f.command.to_lowercase();
    keep.iter().any(|k| {
        let k = k.to_lowercase();
        k == name || (!cmd.is_empty() && k == cmd) || name.split("  :").next() == Some(k.as_str())
    })
}

fn score_dev(f: &Facts, newer_twin: bool, home: &str) -> Option<(Confidence, String)> {
    let mut best: Option<(Confidence, String)> = None;
    let mut add = |c: Confidence, r: String| {
        if best.as_ref().is_none_or(|(bc, _)| c > *bc) {
            best = Some((c, r));
        }
    };
    if f.zombie {
        // The design doc rates this high, but a zombie has already exited: a signal cannot
        // remove it and it holds no memory. It is shown for information only.
        add(Confidence::Low, "zombie · already exited, only its parent can clear it".into());
        return best;
    }
    if f.container {
        if let Some(u) = f.unused_secs.filter(|&u| u >= CONTAINER_IDLE) {
            add(Confidence::Low, format!("idle container · no cpu or network for {}", dur(u)));
        }
        return best;
    }
    if newer_twin {
        let cwd = crate::util::tilde(&f.cwd, home);
        add(Confidence::High, format!("duplicate of newer {} in {}", f.command, cwd));
    }
    if f.launchd || f.service {
        if let Some(u) = f.unused_secs.filter(|&u| u >= SERVICE_IDLE) {
            add(Confidence::Low, format!("background service · unused for {}", dur(u)));
        }
    } else if f.orphaned {
        match f.unused_secs {
            // cpu is not interaction, but something working hard is not "left hanging" either
            Some(u) if u >= ORPHAN_UNUSED && f.cpu >= BUSY_CPU => add(Confidence::Medium, format!("orphaned · busy ({:.0}% cpu), no use seen for {}", f.cpu, dur(u))),
            Some(u) if u >= ORPHAN_UNUSED && f.input_seen => add(Confidence::High, format!("orphaned · unused for {}", dur(u))),
            Some(u) if u >= ORPHAN_UNUSED => add(Confidence::High, format!("orphaned · no use seen for {}", dur(u))),
            Some(u) if f.input_seen => add(Confidence::Medium, format!("orphaned · last used {} ago", dur(u))),
            Some(u) => add(Confidence::Medium, format!("orphaned · watched {}, no use seen", dur(u))),
            None => add(Confidence::Medium, "orphaned · parent closed".into()),
        }
    } else if let Some(u) = f.unused_secs.filter(|&u| u >= DEV_UNUSED) {
        add(Confidence::Medium, format!("no terminal input or network for {}", dur(u)));
    }
    best
}

fn score_app(f: &Facts) -> Option<(Confidence, String)> {
    use Confidence::*;
    let opened_long_ago = f.last_opened_secs.filter(|&o| o >= APP_OPENED_LONG_AGO);
    let unused = f.unused_secs.unwrap_or(0);
    let tracked = f.unused_secs.is_some();
    let mut conf = if tracked && unused >= APP_UNUSED_LONG {
        // without a recorded input the evidence is only "nothing seen": capped at medium
        // unless Spotlight agrees the app has not been opened for days
        match (f.pinned, f.input_seen || opened_long_ago.is_some()) {
            (false, true) => High,
            _ => Medium,
        }
    } else if tracked && unused >= APP_UNUSED {
        match (f.pinned, opened_long_ago.is_some() && !f.input_seen) {
            (false, true) => High,
            (false, false) => Medium,
            (true, _) => Low,
        }
    } else if opened_long_ago.is_some() && !f.input_seen {
        // Spotlight's date alone, before tracking has two hours of history: listed, never staged
        if f.pinned { Low } else { Medium }
    } else {
        return None;
    };

    let mut reason = if f.input_seen {
        format!("no input for {}", dur(unused))
    } else {
        let since = if tracked { format!("no input seen since tracking began ({})", dur(unused)) } else { String::new() };
        match (f.last_opened_secs, since.is_empty()) {
            (Some(o), true) => format!("last opened {} ago", dur(o)),
            (Some(o), false) if opened_long_ago.is_some() => format!("last opened {} ago · {since}", dur(o)),
            _ => since,
        }
    };
    reason.push_str(if f.pinned { " · pinned" } else { " · not in dock" });
    if f.holds_assertion {
        conf = Low;
        reason.push_str(" · playing audio or keeping the mac awake");
    } else if f.background_only {
        conf = Low;
        reason.push_str(" · background app");
    }
    Some((conf, reason))
}

/// Ranks what could be closed. `keep` comes from rules.toml; `home` only shortens paths in reasons.
pub fn score(facts: &[Facts], keep: &[String], home: &str) -> Vec<Candidate> {
    let mut out = Vec::new();
    for f in facts {
        let Some(group) = f.group else { continue };
        if group == Group::System || f.is_self || f.frontmost || f.protected || kept(f, keep) {
            continue;
        }
        let scored = match group {
            Group::Dev => {
                // the older of two identical commands in one directory is the leftover
                // (servers only: two workers of one program are not duplicates of each other)
                let twin = !f.container && !f.launchd && !f.service && f.has_port && !f.command.is_empty() && !f.cwd.is_empty() && f.cwd != "/" && f.cwd != home
                    && facts.iter().any(|o| {
                        o.key != f.key && o.group == Some(Group::Dev) && !o.container && o.has_port && o.command == f.command && o.cwd == f.cwd
                            && (o.age_secs < f.age_secs || (o.age_secs == f.age_secs && o.key > f.key))
                    });
                score_dev(f, twin, home)
            }
            Group::Apps => score_app(f),
            Group::System => None,
        };
        if let Some((confidence, reason)) = scored {
            out.push(Candidate {
                key: f.key.clone(),
                name: f.name.clone(),
                group,
                reason,
                mem: f.mem,
                cpu: f.cpu,
                net: f.net,
                confidence,
                pinned: f.pinned,
                ticked: confidence == Confidence::High,
            });
        }
    }
    out.sort_by(|a, b| b.confidence.cmp(&a.confidence).then(a.pinned.cmp(&b.pinned)).then(b.mem.cmp(&a.mem)).then(a.name.cmp(&b.name)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use Confidence::*;

    const MB: u64 = 1_048_576;
    const HOME: &str = "/Users/me";

    fn dev(name: &str) -> Facts {
        Facts {
            key: format!("dev:{name}"),
            name: name.into(),
            command: name.split("  :").next().unwrap().into(),
            group: Some(Group::Dev),
            mem: 300 * MB,
            age_secs: 5 * HOUR,
            cwd: "/Users/me/code/web".into(),
            has_port: name.contains("  :"),
            input_seen: true,
            ..Default::default()
        }
    }

    fn app(name: &str) -> Facts {
        Facts { key: format!("app:{name}"), name: name.into(), group: Some(Group::Apps), mem: 500 * MB, input_seen: true, ..Default::default() }
    }

    fn run(facts: &[Facts]) -> Vec<Candidate> {
        score(facts, &[], HOME)
    }

    fn one(f: Facts) -> Option<Candidate> {
        run(&[f]).pop()
    }

    #[test]
    fn t01_orphaned_dev_server_idle_3h_is_high_and_ticked() {
        let c = one(Facts { orphaned: true, unused_secs: Some(3 * HOUR), ..dev("next dev  :3000") }).unwrap();
        assert_eq!(c.confidence, High);
        assert!(c.ticked);
        assert!(c.reason.contains("orphaned") && c.reason.contains("3h"), "{}", c.reason);
    }

    #[test]
    fn t02_live_parent_idle_30m_is_not_listed() {
        assert!(one(Facts { unused_secs: Some(30 * MIN), ..dev("next dev  :3000") }).is_none());
    }

    #[test]
    fn t03_live_parent_idle_3h_is_medium_not_ticked() {
        let c = one(Facts { unused_secs: Some(3 * HOUR), ..dev("next dev  :3000") }).unwrap();
        assert_eq!(c.confidence, Medium);
        assert!(!c.ticked);
    }

    #[test]
    fn t04_launchd_postgres_is_not_an_orphan_and_low_only_after_24h() {
        // ppid 1 but launchd-managed: the gatherer sets orphaned=false; even if it did not, launchd wins
        let pg = |unused| Facts { launchd: true, orphaned: true, unused_secs: Some(unused), ..dev("postgres  :5432") };
        assert!(one(pg(3 * HOUR)).is_none());
        assert!(one(pg(23 * HOUR)).is_none());
        let c = one(pg(25 * HOUR)).unwrap();
        assert_eq!(c.confidence, Low);
        assert!(!c.ticked);
        assert!(c.reason.contains("background service"));
    }

    #[test]
    fn t05_two_vite_in_one_cwd_only_the_older_is_listed_as_duplicate() {
        let old = Facts { key: "dev:1".into(), age_secs: 5 * HOUR, unused_secs: Some(20 * MIN), ..dev("vite  :5173") };
        let new = Facts { key: "dev:2".into(), age_secs: 10 * MIN, unused_secs: Some(MIN), ..dev("vite  :5174") };
        let elsewhere = Facts { key: "dev:3".into(), cwd: "/Users/me/code/other".into(), unused_secs: Some(MIN), ..dev("vite  :5175") };
        let c = run(&[new, old, elsewhere]);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].key, "dev:1");
        assert_eq!(c[0].confidence, High);
        assert_eq!(c[0].reason, "duplicate of newer vite in ~/code/web");
    }

    #[test]
    fn t06_unpinned_ranks_before_pinned_and_is_high_where_pinned_is_medium() {
        let pinned = Facts { pinned: true, unused_secs: Some(2 * DAY), ..app("spotify") };
        let unpinned = Facts { unused_secs: Some(2 * DAY), ..app("notion") };
        let c = run(&[pinned, unpinned]);
        assert_eq!(c.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["notion", "spotify"]);
        assert_eq!((c[0].confidence, c[0].ticked), (High, true));
        assert_eq!((c[1].confidence, c[1].ticked), (Medium, false));
        assert_eq!(c[0].reason, "no input for 2d · not in dock");
        assert_eq!(c[1].reason, "no input for 2d · pinned");
    }

    #[test]
    fn t07_frontmost_app_is_never_listed() {
        assert!(one(Facts { frontmost: true, unused_secs: Some(30 * DAY), last_opened_secs: Some(30 * DAY), ..app("zen") }).is_none());
    }

    #[test]
    fn t08_parasight_its_shell_and_its_terminal_are_never_listed() {
        let facts = [
            Facts { is_self: true, orphaned: true, unused_secs: Some(DAY), ..dev("parasight") },
            Facts { is_self: true, unused_secs: Some(DAY), ..dev("zsh") },
            Facts { is_self: true, unused_secs: Some(30 * DAY), ..app("terminal") },
        ];
        assert!(run(&facts).is_empty());
    }

    #[test]
    fn t09_system_items_are_never_listed() {
        let f = Facts { group: Some(Group::System), orphaned: true, zombie: true, unused_secs: Some(30 * DAY), ..dev("mds") };
        assert!(one(f).is_none());
        let protected = Facts { protected: true, unused_secs: Some(30 * DAY), ..app("finder") };
        assert!(one(protected).is_none());
    }

    #[test]
    fn t10_no_history_and_no_spotlight_date_is_not_listed() {
        assert!(one(Facts { unused_secs: None, input_seen: false, last_opened_secs: None, ..app("notes") }).is_none());
    }

    #[test]
    fn t10a_frontmost_for_30h_without_input_counts_as_unused_for_30h() {
        // it sat in front all that time, but nothing was typed or clicked; something else is in front now
        let c = one(Facts { unused_secs: Some(30 * HOUR), input_seen: true, ..app("preview") }).unwrap();
        assert_eq!(c.confidence, High);
        assert!(c.reason.starts_with("no input for 1d"), "{}", c.reason);
    }

    #[test]
    fn t10b_background_app_that_got_input_5_min_ago_is_not_listed() {
        assert!(one(Facts { unused_secs: Some(5 * MIN), last_opened_secs: Some(9 * DAY), ..app("slack") }).is_none());
    }

    #[test]
    fn t10c_dev_server_with_network_traffic_a_minute_ago_is_not_listed() {
        // tty untouched for 5 h, but the gatherer takes the newer of tty and network: 1 min
        assert!(one(Facts { unused_secs: Some(MIN), ..dev("next dev  :3000") }).is_none());
    }

    #[test]
    fn t10d_busy_watcher_nobody_interacts_with_is_medium() {
        // cpu use is not in Facts at all: only tty input and network count
        let c = one(Facts { unused_secs: Some(3 * HOUR), ..dev("tsc --watch") }).unwrap();
        assert_eq!(c.confidence, Medium);
        assert!(!c.ticked);
    }

    #[test]
    fn t11_ordering_is_confidence_then_pin_state_then_memory() {
        let facts = [
            Facts { mem: 900 * MB, pinned: true, unused_secs: Some(3 * HOUR), ..app("low-pinned-big") },
            Facts { mem: 100 * MB, unused_secs: Some(3 * HOUR), ..app("medium-unpinned-small") },
            Facts { mem: 800 * MB, pinned: true, unused_secs: Some(2 * DAY), ..app("medium-pinned-big") },
            Facts { mem: 400 * MB, unused_secs: Some(3 * HOUR), ..app("medium-unpinned-big") },
            Facts { mem: 50 * MB, orphaned: true, unused_secs: Some(HOUR), ..dev("high-small") },
            Facts { mem: 700 * MB, unused_secs: Some(2 * DAY), ..app("high-big") },
        ];
        let names: Vec<String> = run(&facts).into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["high-big", "high-small", "medium-unpinned-big", "medium-unpinned-small", "medium-pinned-big", "low-pinned-big"]);
    }

    #[test]
    fn t12_keep_list_is_never_listed() {
        let facts = [
            Facts { orphaned: true, unused_secs: Some(DAY), ..dev("next dev  :3000") },
            Facts { unused_secs: Some(9 * DAY), ..app("Spotify") },
            Facts { unused_secs: Some(9 * DAY), ..app("notion") },
        ];
        let c = score(&facts, &["spotify".into(), "next dev".into()], HOME);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].name, "notion");
    }

    #[test]
    fn t13_totals_are_the_sums_over_checked_rows_and_follow_the_checkboxes() {
        let facts = [
            Facts { mem: 700 * MB, cpu: 2.0, net: 1000.0, unused_secs: Some(2 * DAY), ..app("a") },
            Facts { mem: 50 * MB, cpu: 0.5, net: 24.0, orphaned: true, unused_secs: Some(HOUR), ..dev("b") },
            Facts { mem: 400 * MB, cpu: 4.0, net: 300.0, unused_secs: Some(3 * HOUR), ..app("c") },
        ];
        let mut c = run(&facts);
        assert_eq!(savings(&c), Savings { count: 2, cpu: 2.5, mem: 750 * MB, net: 1024.0 });
        c[2].ticked = true;
        assert_eq!(savings(&c), Savings { count: 3, cpu: 6.5, mem: 1150 * MB, net: 1324.0 });
        c[0].ticked = false;
        assert_eq!(savings(&c), Savings { count: 2, cpu: 4.5, mem: 450 * MB, net: 324.0 });
        c.iter_mut().for_each(|c| c.ticked = false);
        assert_eq!(savings(&c), Savings::default());
    }

    #[test]
    fn t13a_high_is_recommended_and_checked_medium_and_low_are_to_review_and_unchecked() {
        let facts = [
            Facts { unused_secs: Some(2 * DAY), ..app("high") },
            Facts { unused_secs: Some(3 * HOUR), ..app("medium") },
            Facts { pinned: true, unused_secs: Some(3 * HOUR), ..app("low") },
            Facts { launchd: true, unused_secs: Some(2 * DAY), ..dev("low-service") },
        ];
        let c = run(&facts);
        assert_eq!(c.len(), 4);
        for x in &c {
            match x.confidence {
                High => assert_eq!((x.section(), x.ticked), (Section::Recommended, true)),
                Medium | Low => assert_eq!((x.section(), x.ticked), (Section::ToReview, false)),
            }
        }
        assert_eq!(c.iter().filter(|x| x.section() == Section::Recommended).count(), 1);
        // recommended rows always come first, so the two groups are contiguous
        assert_eq!(c[0].section(), Section::Recommended);
    }

    #[test]
    fn orphan_that_is_still_in_use_is_medium() {
        let c = one(Facts { orphaned: true, unused_secs: Some(2 * MIN), ..dev("vite  :5173") }).unwrap();
        assert_eq!((c.confidence, c.ticked), (Medium, false));
        assert_eq!(c.reason, "orphaned · last used 2m ago");
        // an orphan that is working hard is listed but not pre-checked, however long unused
        let c = one(Facts { orphaned: true, cpu: 80.0, unused_secs: Some(3 * HOUR), ..dev("build-server") }).unwrap();
        assert_eq!((c.confidence, c.ticked), (Medium, false));
        assert_eq!(c.reason, "orphaned · busy (80% cpu), no use seen for 3h");
        // watched for two minutes without seeing any use: says so instead of claiming a time
        let c = one(Facts { orphaned: true, input_seen: false, unused_secs: Some(2 * MIN), ..dev("sleep 9999") }).unwrap();
        assert_eq!((c.confidence, c.reason.as_str()), (Medium, "orphaned · watched 2m, no use seen"));
        // two workers of one program in one directory are not duplicates: no port, no claim
        let w = |k: &str, age| Facts { key: k.into(), age_secs: age, unused_secs: Some(MIN), ..dev("python -c") };
        assert!(run(&[w("dev:1", HOUR), w("dev:2", MIN)]).is_empty());
        // known background tools started by hand (tmux, ollama) are not orphans
        assert!(one(Facts { orphaned: true, service: true, unused_secs: Some(3 * HOUR), ..dev("tmux") }).is_none());
    }

    #[test]
    fn apps_without_recorded_input_are_capped_unless_spotlight_agrees() {
        let never = |unused, opened| Facts { unused_secs: Some(unused), input_seen: false, last_opened_secs: opened, ..app("x") };
        // tracked a day and a half, nothing seen, no Spotlight date: medium
        let c = one(never(36 * HOUR, None)).unwrap();
        assert_eq!(c.confidence, Medium);
        assert_eq!(c.reason, "no input seen since tracking began (1d) · not in dock");
        // Spotlight also says it has not been opened for days: high
        assert_eq!(one(never(36 * HOUR, Some(5 * DAY))).unwrap().confidence, High);
        // tracking only just began: Spotlight's date alone lists it but never stages it
        let c = one(never(2 * MIN, Some(5 * DAY))).unwrap();
        assert_eq!((c.confidence, c.ticked), (Medium, false));
        assert_eq!(c.reason, "last opened 5d ago · no input seen since tracking began (2m) · not in dock");
        // and with a recent Spotlight date there is nothing to say
        assert!(one(never(2 * MIN, Some(HOUR))).is_none());
    }

    #[test]
    fn audio_and_background_apps_are_capped_at_low() {
        let c = one(Facts { holds_assertion: true, unused_secs: Some(2 * DAY), ..app("spotify") }).unwrap();
        assert_eq!((c.confidence, c.ticked), (Low, false));
        let c = one(Facts { background_only: true, unused_secs: Some(2 * DAY), ..app("rectangle") }).unwrap();
        assert_eq!((c.confidence, c.ticked), (Low, false));
    }

    #[test]
    fn containers_and_zombies() {
        let c = one(Facts { container: true, unused_secs: Some(7 * HOUR), ..dev("db  :5433") }).unwrap();
        assert_eq!(c.confidence, Low);
        assert!(one(Facts { container: true, unused_secs: Some(HOUR), orphaned: true, ..dev("db  :5433") }).is_none());
        let z = one(Facts { zombie: true, ..dev("defunct") }).unwrap();
        assert!(!z.ticked);
    }
}
