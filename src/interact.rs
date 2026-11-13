//! "Last interacted": who received input, and when. The sampler and the state are pure
//! (feed them samples); only `load`/`save` touch the disk.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Seconds since the last system-wide event of each kind.
#[derive(Clone, Copy, Debug)]
pub struct InputAges {
    pub key: f64,
    pub click: f64,
    pub scroll: f64,
    // Mouse movement is deliberately not here: moving the pointer across a window is not use.
}

impl InputAges {
    pub fn since_input(&self) -> f64 {
        self.key.min(self.click).min(self.scroll)
    }
}

/// Pairs the frontmost app with new input. Call `feed` about once a second.
#[derive(Default)]
pub struct InputSampler {
    prev: Option<f64>,
}

impl InputSampler {
    /// Returns the app to stamp and the time of the input, when input arrived since the last sample.
    pub fn feed<'a>(&mut self, now: f64, front: Option<&'a str>, ages: InputAges) -> Option<(&'a str, u64)> {
        let prev = self.prev.unwrap_or(now - 2.0);
        self.prev = Some(now);
        let last_input = now - ages.since_input();
        if last_input > prev { front.map(|f| (f, last_input.max(0.0) as u64)) } else { None }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ProcStamp {
    /// When tracking first saw this process.
    pub first_seen: u64,
    /// Last time it sent or received bytes (containers: used cpu or network).
    #[serde(default)]
    pub last_net: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct State {
    #[serde(default)]
    pub version: u32,
    /// Start of the current unbroken stretch of tracking. 0 means no history.
    #[serde(default)]
    pub tracking_since: u64,
    #[serde(default)]
    pub last_sample: u64,
    /// App bundle path -> last time it received input.
    #[serde(default)]
    pub apps: BTreeMap<String, u64>,
    /// "pid:start_time" or "docker:id" -> stamps.
    #[serde(default)]
    pub procs: BTreeMap<String, ProcStamp>,
}

/// A gap longer than this between samples means nobody was watching.
const MAX_GAP: u64 = 180;

impl State {
    pub fn parse(text: &str) -> State {
        match serde_json::from_str::<serde_json::Value>(text) {
            Ok(v) if v.is_object() => serde_json::from_value(v).unwrap_or_default(),
            _ => State::default(),
        }
    }

    /// A missing or corrupt file is simply "no history".
    pub fn load(path: &Path) -> State {
        std::fs::read_to_string(path).map(|t| State::parse(&t)).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self).unwrap_or_default())?;
        std::fs::rename(tmp, path)
    }

    pub fn default_path() -> PathBuf {
        crate::native::parasight_home().join("state/last-interacted.json")
    }

    /// Called once after loading. If tracking was not running for a while, what was recorded
    /// before the gap cannot prove "nothing happened since", so history starts again.
    /// Time the machine was switched off does not count as a gap.
    pub fn resume(&mut self, now: u64, boot_time: u64) {
        self.version = 1;
        let off_until = if boot_time > self.last_sample { boot_time } else { self.last_sample };
        let unbroken = self.tracking_since > 0 && self.last_sample > 0 && now.saturating_sub(off_until) <= MAX_GAP && self.last_sample <= now;
        if !unbroken {
            self.tracking_since = now;
            self.apps.clear();
            self.procs.clear();
        }
        self.last_sample = now;
    }

    /// Record a sample time. A long jump inside one run is the machine sleeping: nothing was
    /// interacted with meanwhile, so history stays valid.
    pub fn tick(&mut self, now: u64) {
        if self.tracking_since == 0 {
            self.tracking_since = now;
        }
        self.last_sample = now;
    }

    pub fn stamp_app(&mut self, bundle: &str, at: u64) {
        let e = self.apps.entry(bundle.to_string()).or_insert(0);
        *e = (*e).max(at);
    }

    /// How long an app has gone without input, and whether an input was ever recorded.
    /// `None` when there is no tracking history at all.
    pub fn app_unused(&self, bundle: &str, now: u64) -> Option<(u64, bool)> {
        if self.tracking_since == 0 {
            return None;
        }
        match self.apps.get(bundle) {
            Some(&t) if t >= self.tracking_since => Some((now.saturating_sub(t), true)),
            _ => Some((now.saturating_sub(self.tracking_since), false)),
        }
    }

    pub fn proc_key(pid: u32, start_time: u64) -> String {
        format!("{pid}:{start_time}")
    }

    /// Note that a process exists, and whether it is using the network right now.
    pub fn see_proc(&mut self, key: &str, now: u64, active: bool) {
        let e = self.procs.entry(key.to_string()).or_insert(ProcStamp { first_seen: now, last_net: None });
        if active {
            e.last_net = Some(now);
        }
    }

    /// The last time a process is known to have used the network and whether that was actually
    /// seen; when it was not, the time is when tracking first saw the process (what it did
    /// before that is unknown: nettop only counts sockets that are still open).
    pub fn proc_last_used(&self, key: &str) -> Option<(u64, bool)> {
        let s = self.procs.get(key)?;
        Some((s.last_net.unwrap_or(s.first_seen), s.last_net.is_some()))
    }

    pub fn prune_procs(&mut self, alive: &dyn Fn(&str) -> bool) {
        self.procs.retain(|k, _| alive(k));
    }

    /// Take anything newer from a state written by another sampler (`parasight track`).
    pub fn merge(&mut self, other: &State) {
        if other.tracking_since == 0 {
            return;
        }
        if self.tracking_since == 0 || other.tracking_since < self.tracking_since {
            self.tracking_since = other.tracking_since;
        }
        for (k, &t) in &other.apps {
            self.stamp_app(k, t);
        }
        for (k, o) in &other.procs {
            match self.procs.get_mut(k) {
                Some(s) => {
                    s.last_net = s.last_net.max(o.last_net);
                    s.first_seen = s.first_seen.min(o.first_seen);
                }
                None => {
                    self.procs.insert(k.clone(), o.clone());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The last argument is how long ago the pointer moved: the sampler never sees it.
    fn ages(key: f64, click: f64, scroll: f64, _moved: f64) -> InputAges {
        InputAges { key, click, scroll }
    }

    // design test 10e
    #[test]
    fn input_newer_than_the_previous_sample_stamps_only_the_frontmost_app() {
        let mut s = InputSampler::default();
        let mut st = State::default();
        st.resume(1000, 0);
        let mut feed = |s: &mut InputSampler, now: f64, front: &str, a: InputAges| {
            if let Some((app, at)) = s.feed(now, Some(front), a) {
                st.stamp_app(app, at);
            }
            st.apps.clone()
        };
        // a key press 0.3 s ago while the editor is in front
        let apps = feed(&mut s, 1001.0, "/Applications/Editor.app", ages(0.3, 50.0, 50.0, 0.1));
        assert_eq!(apps.len(), 1);
        assert_eq!(apps["/Applications/Editor.app"], 1000);
        // a second later nothing new: the same key press is now 1.3 s old
        let apps = feed(&mut s, 1002.0, "/Applications/Editor.app", ages(1.3, 51.0, 51.0, 0.1));
        assert_eq!(apps["/Applications/Editor.app"], 1000);
        // the browser comes to the front but only the pointer moves: nothing is stamped
        let apps = feed(&mut s, 1003.0, "/Applications/Browser.app", ages(2.3, 52.0, 52.0, 0.0));
        assert!(!apps.contains_key("/Applications/Browser.app"));
        // a scroll lands while the browser is in front: only the browser is stamped
        let apps = feed(&mut s, 1004.0, "/Applications/Browser.app", ages(3.3, 53.0, 0.2, 0.0));
        assert_eq!(apps["/Applications/Browser.app"], 1003);
        assert_eq!(apps["/Applications/Editor.app"], 1000);
    }

    // design test 10a, sampler side: thirty hours in front without input records nothing
    #[test]
    fn focus_without_input_does_not_count() {
        let mut s = InputSampler::default();
        let mut st = State::default();
        st.resume(100_000, 0);
        let mut now = 100_010.0;
        while now < 100_000.0 + 30.0 * 3600.0 {
            // last input was before sampling began and keeps getting older
            assert!(s.feed(now, Some("/Applications/Idle.app"), ages(now + 60.0, now + 60.0, now + 60.0, 0.5)).is_none());
            st.tick(now as u64);
            now += 600.0;
        }
        let (unused, seen) = st.app_unused("/Applications/Idle.app", 100_000 + 30 * 3600).unwrap();
        assert_eq!(unused, 30 * 3600);
        assert!(!seen);
    }

    // design test 10f
    #[test]
    fn state_round_trips_and_bad_files_mean_no_history() {
        let dir = std::env::temp_dir().join(format!("parasight-test-{}", std::process::id()));
        let path = dir.join("nested/last-interacted.json");
        let mut st = State::default();
        st.resume(500, 0);
        st.stamp_app("/Applications/A.app", 600);
        st.see_proc("12:34", 610, true);
        st.see_proc("13:35", 610, false);
        st.save(&path).unwrap();
        assert_eq!(State::load(&path), st);

        std::fs::write(&path, "{ not json").unwrap();
        let bad = State::load(&path);
        assert_eq!(bad, State::default());
        assert_eq!(bad.app_unused("/Applications/A.app", 700), None);
        assert_eq!(State::load(&dir.join("missing.json")), State::default());
        // valid json of the wrong shape is also just "no history"
        std::fs::write(&path, "[1,2,3]").unwrap();
        assert_eq!(State::load(&path), State::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_gap_in_tracking_restarts_history_but_time_switched_off_does_not() {
        let mut st = State::default();
        st.resume(1000, 0);
        st.stamp_app("a", 1100);
        st.tick(2000);
        // parasight was closed for an hour while the machine stayed on
        let mut gap = st.clone();
        gap.resume(2000 + 3600, 500);
        assert_eq!(gap.tracking_since, 5600);
        assert!(gap.apps.is_empty());
        // back within a minute: history continues
        let mut quick = st.clone();
        quick.resume(2060, 500);
        assert_eq!(quick.tracking_since, 1000);
        assert_eq!(quick.app_unused("a", 2060), Some((960, true)));
        // machine was shut down overnight and tracking started right after boot
        let mut reboot = st.clone();
        reboot.resume(50_060, 50_000);
        assert_eq!(reboot.tracking_since, 1000);
    }

    #[test]
    fn proc_last_used_is_real_traffic_or_else_first_sight() {
        let mut st = State::default();
        st.resume(1000, 0);
        st.see_proc("quiet", 1000, false);
        st.see_proc("busy", 1000, false);
        st.see_proc("busy", 1500, true);
        st.see_proc("busy", 1600, false);
        assert_eq!(st.proc_last_used("quiet"), Some((1000, false))); // nothing seen: only known since first sight
        assert_eq!(st.proc_last_used("busy"), Some((1500, true)));
        assert_eq!(st.proc_last_used("nope"), None);
    }
}
