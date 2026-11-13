//! Read-only views of what parasight knows, as JSON, for the ask feature and the mcp server.
//! Everything works on a `World`, a plain copy of the current data, so it can be tested with
//! fixtures. Every string that leaves here has been through the redactor.

use crate::classify;
use crate::collect::Collector;
use crate::interact::State;
use crate::model::*;
use crate::redact::{hide_home, redact_args, redact_text};
use crate::sources::{Container, lock};
use crate::util::{base, clip, dur};
use serde_json::{Value, json};
use std::collections::HashMap;

#[derive(Default)]
pub struct World {
    pub items: Vec<Item>,
    pub procs: HashMap<u32, ProcInfo>,
    pub ports: HashMap<u32, Vec<u16>>,
    /// bundle path -> bundle id
    pub bundle_ids: HashMap<String, String>,
    pub containers: Vec<Container>,
    pub home: String,
    /// The current user's account name.
    pub user: String,
    pub now: u64,
    pub cpu: f32,
    pub cores: usize,
    pub mem_used: u64,
    pub mem_total: u64,
    pub rx: f64,
    pub tx: f64,
    pub uptime: u64,
    /// Last-interacted stamps, when there are any.
    pub state: Option<State>,
}

impl World {
    pub fn from_collector(col: &Collector, state: Option<State>) -> World {
        let m = &col.machine;
        World {
            items: col.snapshot.items.clone(),
            procs: col.procs().clone(),
            ports: lock(&col.shared.ports).clone(),
            bundle_ids: lock(&col.shared.gui).values().filter_map(|a| Some((a.bundle.clone(), a.bundle_id.clone()?))).collect(),
            containers: lock(&col.shared.docker).clone(),
            home: col.home.clone(),
            // SAFETY: getuid cannot fail.
            user: crate::native::user_name(unsafe { libc::getuid() }),
            now: crate::native::now(),
            cpu: m.cpu,
            cores: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
            mem_used: m.mem_used,
            mem_total: m.mem_total,
            rx: m.rx,
            tx: m.tx,
            uptime: crate::native::now().saturating_sub(crate::native_track::boot_time()),
            state,
        }
    }

    fn clean(&self, text: &str) -> String {
        redact_text(&hide_home(text, &self.home))
    }

    fn command(&self, p: &ProcInfo) -> String {
        if p.cmd.is_empty() {
            return self.clean(&p.name);
        }
        clip(&hide_home(&redact_args(&p.cmd).join(" "), &self.home), 300)
    }

    /// Names from a process up to launchd: `["node", "zsh", "login", "Terminal", "launchd"]`.
    fn parent_chain(&self, pid: u32) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = pid;
        for _ in 0..16 {
            let Some(p) = self.procs.get(&cur) else { break };
            out.push(if p.exe.is_empty() { p.name.clone() } else { base(&p.exe).to_string() });
            if p.ppid == 0 || p.ppid == cur {
                break;
            }
            cur = p.ppid;
        }
        out
    }

    fn last_interacted(&self, it: &Item) -> Option<String> {
        let st = self.state.as_ref()?;
        match &it.kind {
            ItemKind::App { bundle } => st.app_unused(bundle, self.now).map(|(secs, seen)| if seen { format!("last keyboard or mouse input {} ago", dur(secs)) } else { format!("no input seen in the {} parasight has tracked", dur(secs)) }),
            ItemKind::Dev { root_pid } => {
                let root = self.procs.get(root_pid)?;
                st.proc_last_used(&State::proc_key(root.pid, root.start_time)).map(|(t, seen)| {
                    let secs = self.now.saturating_sub(t);
                    if seen { format!("last network use {} ago", dur(secs)) } else { format!("no network use seen in the {} parasight has watched it", dur(secs)) }
                })
            }
            _ => None,
        }
    }

    fn summary(&self, it: &Item) -> Value {
        json!({
            "name": self.clean(&it.name),
            "group": it.group.name(),
            "cpu_percent": round1(it.cpu as f64),
            "memory_mb": mb(it.mem),
            "network_kb_per_s": round1((it.rx + it.tx) / 1024.0),
            "processes": it.procs.len(),
        })
    }

    fn process(&self, row: &ProcRow) -> Value {
        let p = self.procs.get(&row.pid);
        json!({
            "pid": row.pid,
            "ppid": p.map(|p| p.ppid),
            // the account name stays here: "you" says the same thing
            "user": if row.user == self.user { "you" } else { row.user.as_str() },
            "started_seconds_ago": (row.start_time > 0).then(|| self.now.saturating_sub(row.start_time)),
            "cpu_percent": round1(row.cpu as f64),
            "memory_mb": mb(row.mem),
            "network_kb_per_s": round1((row.rx + row.tx) / 1024.0),
            "command": p.map(|p| self.command(p)).unwrap_or_else(|| self.clean(&row.name)),
        })
    }

    /// Everything known about one item. `project` is the name of the project its working
    /// directory belongs to, when the caller looked one up.
    pub fn item_details(&self, it: &Item, project: Option<String>) -> Value {
        let main = it.procs.first().and_then(|r| self.procs.get(&r.pid));
        let mut ports: Vec<u16> = it.procs.iter().filter_map(|r| self.ports.get(&r.pid)).flatten().copied().collect();
        ports.sort_unstable();
        ports.dedup();
        let mut procs: Vec<&ProcRow> = it.procs.iter().collect();
        procs.sort_by_key(|r| std::cmp::Reverse(r.mem));
        let mut v = self.summary(it);
        let o = v.as_object_mut().expect("summary is an object");
        o.insert("kind".into(), json!(match it.kind {
            ItemKind::App { .. } => "app",
            ItemKind::Dev { .. } => "dev process tree",
            ItemKind::Container { .. } => "container",
            ItemKind::System => "system",
        }));
        o.insert("description_line".into(), json!(self.clean(&it.path)));
        if let Some(p) = main {
            o.insert("executable".into(), json!(self.clean(&p.exe)));
            o.insert("command_line".into(), json!(self.command(p)));
            if !p.cwd.is_empty() {
                o.insert("working_directory".into(), json!(self.clean(&p.cwd)));
            }
            o.insert("parent_chain".into(), json!(self.parent_chain(p.pid).into_iter().skip(1).collect::<Vec<_>>()));
            if let Some(l) = &p.launchd_label {
                o.insert("launchd_label".into(), json!(l));
            }
            if let Some(tool) = classify::devtool_name(p) {
                o.insert("known_dev_tool".into(), json!(tool));
            }
        }
        if let Some(project) = project {
            o.insert("project".into(), json!(self.clean(&project)));
        }
        if !ports.is_empty() {
            o.insert("listening_ports".into(), json!(ports));
        }
        match &it.kind {
            ItemKind::App { bundle } => {
                if let Some(id) = self.bundle_ids.get(bundle) {
                    o.insert("bundle_id".into(), json!(id));
                }
            }
            ItemKind::Container { id } => {
                if let Some(c) = self.containers.iter().find(|c| &c.id == id) {
                    o.insert("container".into(), json!({ "image": c.image, "compose_project": c.project, "status": c.status, "published_port": c.port }));
                }
            }
            _ => {}
        }
        if let Some(l) = self.last_interacted(it) {
            o.insert("last_interacted".into(), json!(l));
        }
        o.insert("process_list".into(), json!(procs.iter().take(8).map(|r| self.process(r)).collect::<Vec<_>>()));
        v
    }

    /// One process of an item, for when a single row of the process table is asked about.
    pub fn process_details(&self, it: &Item, pid: u32, project: Option<String>) -> Option<Value> {
        let row = it.procs.iter().find(|r| r.pid == pid)?;
        let p = self.procs.get(&pid)?;
        let mut v = self.process(row);
        let o = v.as_object_mut()?;
        o.insert("name".into(), json!(self.clean(&row.name)));
        o.insert("executable".into(), json!(self.clean(&p.exe)));
        if !p.cwd.is_empty() {
            o.insert("working_directory".into(), json!(self.clean(&p.cwd)));
        }
        if let Some(project) = project {
            o.insert("project".into(), json!(self.clean(&project)));
        }
        if let Some(ports) = self.ports.get(&pid) {
            o.insert("listening_ports".into(), json!(ports));
        }
        o.insert("parent_chain".into(), json!(self.parent_chain(pid).into_iter().skip(1).collect::<Vec<_>>()));
        o.insert("belongs_to".into(), self.summary(it));
        Some(v)
    }

    pub fn find(&self, query: &str) -> Option<&Item> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return None;
        }
        if let Ok(pid) = q.parse::<u32>() {
            return self.items.iter().find(|i| i.procs.iter().any(|p| p.pid == pid && pid != 0));
        }
        let name = |i: &Item| i.name.to_lowercase();
        self.items
            .iter()
            .find(|i| name(i) == q || name(i).split("  :").next() == Some(q.as_str()))
            .or_else(|| self.items.iter().filter(|i| name(i).contains(&q)).max_by_key(|i| i.mem))
    }

    // ───────────────────────── the four tools ─────────────────────────

    pub fn get_item(&self, name_or_pid: &str) -> Value {
        match self.find(name_or_pid) {
            Some(it) => self.item_details(it, project_name(&self.cwd_of(it), &self.home)),
            None => json!({ "error": format!("nothing matches \"{}\"", clip(name_or_pid, 60)), "hint": "use search or list_items to see what is running" }),
        }
    }

    pub fn cwd_of(&self, it: &Item) -> String {
        it.procs.first().and_then(|r| self.procs.get(&r.pid)).map(|p| p.cwd.clone()).unwrap_or_default()
    }

    pub fn list_items(&self, group: Option<Group>, sort_by: Metric, limit: usize) -> Value {
        let mut items: Vec<&Item> = self.items.iter().filter(|i| group.is_none_or(|g| i.group == g)).collect();
        items.sort_by(|a, b| b.metric(sort_by).total_cmp(&a.metric(sort_by)));
        json!({ "total": items.len(), "items": items.iter().take(limit).map(|i| self.summary(i)).collect::<Vec<_>>() })
    }

    pub fn search(&self, query: &str) -> Value {
        let q = query.trim().to_lowercase();
        let mut hits = Vec::new();
        for it in &self.items {
            let matched = if it.name.to_lowercase().contains(&q) {
                Some("name".to_string())
            } else if it.path.to_lowercase().contains(&q) {
                Some(self.clean(&it.path))
            } else {
                it.procs.iter().filter_map(|r| self.procs.get(&r.pid)).find(|p| p.exe.to_lowercase().contains(&q) || p.cmd.iter().any(|a| a.to_lowercase().contains(&q))).map(|p| self.command(p))
            };
            if let Some(m) = matched {
                let mut s = self.summary(it);
                s["matched"] = json!(clip(&m, 160));
                hits.push((it.mem, s));
            }
        }
        hits.sort_by_key(|h| std::cmp::Reverse(h.0));
        json!({ "total": hits.len(), "items": hits.into_iter().take(15).map(|h| h.1).collect::<Vec<_>>() })
    }

    pub fn system_summary(&self) -> Value {
        let group = |g: Group| {
            let items: Vec<&Item> = self.items.iter().filter(|i| i.group == g).collect();
            json!({
                "items": items.len(),
                "processes": items.iter().map(|i| i.procs.len()).sum::<usize>(),
                "cpu_percent": round1(items.iter().map(|i| i.cpu as f64).sum()),
                "memory_mb": mb(items.iter().map(|i| i.mem).sum()),
            })
        };
        json!({
            "cpu_percent_of_all_cores": round1(self.cpu as f64),
            "cores": self.cores,
            "memory_used_gb": round1(self.mem_used as f64 / 1_073_741_824.0),
            "memory_total_gb": round1(self.mem_total as f64 / 1_073_741_824.0),
            "network_down_kb_per_s": round1(self.rx / 1024.0),
            "network_up_kb_per_s": round1(self.tx / 1024.0),
            "uptime": dur(self.uptime),
            "process_count": self.procs.len(),
            "groups": { "apps": group(Group::Apps), "dev": group(Group::Dev), "system": group(Group::System) },
            "note": "cpu_percent of an item is a percentage of one core",
        })
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn mb(bytes: u64) -> u64 {
    (bytes as f64 / 1_048_576.0).round() as u64
}

/// The name of the project a directory belongs to: the nearest package manifest's name, or
/// the folder's own name. Reads at most a few small files; never writes.
pub fn project_name(cwd: &str, home: &str) -> Option<String> {
    if cwd.is_empty() || cwd == "/" || cwd == home {
        return None;
    }
    let mut dir = std::path::PathBuf::from(cwd);
    let mut repo_root = None;
    for _ in 0..4 {
        if dir.to_str() == Some(home) || dir.parent().is_none() {
            break;
        }
        let read = |f: &str| std::fs::read_to_string(dir.join(f)).ok().filter(|t| t.len() < 512 * 1024);
        if let Some(name) = read("package.json").and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["name"].as_str().map(String::from)) {
            return Some(name);
        }
        for manifest in ["Cargo.toml", "pyproject.toml"] {
            if let Some(name) = read(manifest).and_then(|t| manifest_name(&t)) {
                return Some(name);
            }
        }
        if let Some(name) = read("go.mod").and_then(|t| t.lines().find_map(|l| l.strip_prefix("module ").map(|m| base(m.trim()).to_string()))) {
            return Some(name);
        }
        if dir.join(".git").exists() {
            repo_root = Some(dir.clone());
            break;
        }
        if !dir.pop() {
            break;
        }
    }
    // no manifest: the repository's folder, or failing that the directory itself
    let d = repo_root.unwrap_or_else(|| std::path::PathBuf::from(cwd));
    d.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// `name = "x"` under `[package]` or `[project]` (or `[tool.poetry]`).
fn manifest_name(text: &str) -> Option<String> {
    let v: toml::Value = toml::from_str(text).ok()?;
    ["package", "project"].iter().find_map(|k| v.get(k)?.get("name")?.as_str().map(String::from)).or_else(|| v.get("tool")?.get("poetry")?.get("name")?.as_str().map(String::from))
}

#[cfg(test)]
pub mod tests {
    use super::*;

    fn row(pid: u32, name: &str, mem_mb: u64) -> ProcRow {
        ProcRow { pid, start_time: 900, name: name.into(), user: "me".into(), cpu: 1.5, mem: mem_mb * 1_048_576, ..Default::default() }
    }

    fn proc(pid: u32, ppid: u32, exe: &str, cmd: &[&str], cwd: &str) -> ProcInfo {
        ProcInfo { pid, ppid, uid: 501, name: base(exe).into(), exe: exe.into(), cmd: cmd.iter().map(|s| s.to_string()).collect(), cwd: cwd.into(), start_time: 900, ..Default::default() }
    }

    /// A small machine: a terminal, a dev server started from it with a secret on its command
    /// line, an editor, and a system daemon.
    pub fn world() -> World {
        let procs = [
            proc(1, 0, "/sbin/launchd", &[], ""),
            proc(100, 1, "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal", &[], ""),
            proc(102, 100, "/bin/zsh", &["-zsh"], "/Users/me"),
            proc(110, 102, "/opt/homebrew/bin/node", &["node", "/Users/me/code/shop/node_modules/.bin/next", "dev", "--token", "hunter2hunter2", "DATABASE_URL=postgres://admin:pw@db/shop"], "/Users/me/code/shop"),
            proc(111, 110, "/opt/homebrew/bin/node", &["node", "/Users/me/code/shop/node_modules/next/dist/server/lib/start-server.js"], "/Users/me/code/shop"),
            proc(200, 1, "/Applications/Visual Studio Code.app/Contents/MacOS/Electron", &[], "/"),
            proc(300, 1, "/usr/libexec/rapportd", &[], "/"),
        ];
        let item = |key: &str, name: &str, group, kind, path: &str, rows: Vec<ProcRow>| {
            let mem = rows.iter().map(|r| r.mem).sum();
            Item { key: key.into(), name: name.into(), group, path: path.into(), kind, procs: rows, cpu: 3.0, mem, rx: 2048.0, tx: 1024.0 }
        };
        World {
            items: vec![
                item("app:/System/Applications/Utilities/Terminal.app", "Terminal", Group::Apps, ItemKind::App { bundle: "/System/Applications/Utilities/Terminal.app".into() }, "/System/Applications/Utilities/Terminal.app", vec![row(100, "Terminal", 150), row(102, "zsh", 4)]),
                item("dev:110", "next dev  :3000", Group::Dev, ItemKind::Dev { root_pid: 110 }, "/Users/me/code/shop · node next dev --token hunter2hunter2", vec![row(110, "node next dev", 600), row(111, "node start-server.js", 300)]),
                item("app:/Applications/Visual Studio Code.app", "Visual Studio Code", Group::Apps, ItemKind::App { bundle: "/Applications/Visual Studio Code.app".into() }, "/Applications/Visual Studio Code.app", vec![row(200, "Electron", 800)]),
                item("sys:rapportd", "rapportd", Group::System, ItemKind::System, "/usr/libexec/rapportd", vec![row(300, "rapportd", 20)]),
            ],
            procs: procs.into_iter().map(|p| (p.pid, p)).collect(),
            ports: [(111, vec![3000])].into_iter().collect(),
            bundle_ids: [("/Applications/Visual Studio Code.app".to_string(), "com.microsoft.VSCode".to_string())].into_iter().collect(),
            home: "/Users/me".into(),
            user: "me".into(),
            now: 1000,
            cpu: 12.0,
            cores: 8,
            mem_used: 16 << 30,
            mem_total: 32 << 30,
            uptime: 7200,
            ..Default::default()
        }
    }

    #[test]
    fn item_details_carry_the_context_and_no_secrets() {
        let w = world();
        let v = w.get_item("next dev");
        assert_eq!(v["name"], "next dev  :3000");
        assert_eq!(v["group"], "dev");
        assert_eq!(v["listening_ports"], json!([3000]));
        assert_eq!(v["working_directory"], "~/code/shop");
        assert_eq!(v["project"], "shop"); // no manifest on disk: the folder name
        assert_eq!(v["parent_chain"], json!(["zsh", "Terminal", "launchd"]));
        assert_eq!(v["process_list"].as_array().unwrap().len(), 2);
        assert_eq!(v["process_list"][0]["pid"], 110);
        let text = v.to_string();
        assert!(!text.contains("hunter2") && !text.contains("admin:pw"), "{text}");
        assert!(text.contains("--token [redacted]") && text.contains("postgres://[redacted]@db/shop"), "{text}");
        assert!(!text.contains("/Users/me"), "the home directory is shortened: {text}");
        assert_eq!(v["process_list"][0]["user"], "you", "the account name is not sent");
    }

    #[test]
    fn get_item_by_pid_exact_name_or_substring() {
        let w = world();
        assert_eq!(w.get_item("111")["name"], "next dev  :3000");
        assert_eq!(w.get_item("terminal")["name"], "Terminal");
        assert_eq!(w.get_item("visual")["bundle_id"], "com.microsoft.VSCode");
        assert!(w.get_item("nope")["error"].as_str().unwrap().contains("nothing matches"));
        assert!(w.get_item("0")["error"].is_string());
    }

    #[test]
    fn list_search_and_summary() {
        let w = world();
        let l = w.list_items(Some(Group::Apps), Metric::Mem, 1);
        assert_eq!(l["total"], 2);
        assert_eq!(l["items"].as_array().unwrap().len(), 1);
        assert_eq!(l["items"][0]["name"], "Visual Studio Code");
        assert_eq!(w.list_items(None, Metric::Cpu, 50)["total"], 4);

        let s = w.search("start-server");
        assert_eq!(s["total"], 1);
        assert_eq!(s["items"][0]["name"], "next dev  :3000");
        assert_eq!(w.search("zzz")["total"], 0);
        // a search that matches a secret-bearing command line does not echo the secret
        assert!(!w.search("next").to_string().contains("hunter2"));

        let sys = w.system_summary();
        assert_eq!(sys["cores"], 8);
        assert_eq!(sys["memory_total_gb"], 32.0);
        assert_eq!(sys["groups"]["apps"]["items"], 2);
        assert_eq!(sys["uptime"], "2h");
    }

    #[test]
    fn a_single_process_can_be_described() {
        let w = world();
        let it = w.find("next dev").unwrap();
        let v = w.process_details(it, 111, None).unwrap();
        assert_eq!(v["belongs_to"]["name"], "next dev  :3000");
        assert_eq!(v["parent_chain"], json!(["node", "zsh", "Terminal", "launchd"]));
        assert!(w.process_details(it, 999, None).is_none());
    }

    #[test]
    fn project_names_come_from_manifests_or_the_folder() {
        let dir = std::env::temp_dir().join(format!("parasight-proj-{}", std::process::id()));
        let sub = dir.join("web/src/app");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(project_name(sub.to_str().unwrap(), "/nohome").as_deref(), Some("app"));
        std::fs::write(dir.join("web/package.json"), r#"{"name":"@acme/storefront"}"#).unwrap();
        assert_eq!(project_name(sub.to_str().unwrap(), "/nohome").as_deref(), Some("@acme/storefront"));
        std::fs::write(dir.join("web/src/Cargo.toml"), "[package]\nname = \"inner\"\n").unwrap();
        assert_eq!(project_name(sub.to_str().unwrap(), "/nohome").as_deref(), Some("inner"));
        assert_eq!(project_name("/", "/nohome"), None);
        assert_eq!(project_name("/Users/me", "/Users/me"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
