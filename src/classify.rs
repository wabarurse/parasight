//! Decides which group a process belongs to and which item it rolls up under.
//! Pure: it only looks at the plain data it is given.

use crate::model::{Group, ProcInfo};
use crate::rules::Rules;
use crate::util::base;
use std::collections::{HashMap, HashSet};

pub struct Ctx<'a> {
    pub procs: &'a HashMap<u32, ProcInfo>,
    /// Bundle paths of apps that have a Dock icon right now (from lsappinfo).
    pub gui_bundles: &'a HashSet<String>,
    pub rules: &'a Rules,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    App { bundle: String },
    Dev { root_pid: u32 },
    System,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Class {
    pub group: Group,
    /// Processes with the same key become one item.
    pub key: String,
    pub kind: Kind,
}

const SHELLS: &[&str] = &["zsh", "bash", "sh", "fish", "tcsh", "csh", "ksh", "dash", "nu", "pwsh", "xonsh"];

/// Language servers and similar tools that editors spawn. They go to dev instead of
/// rolling up into the editor.
const DEVTOOLS: &[&str] = &[
    "rust-analyzer", "rust-analyzer-proc-macro-srv", "tsserver", "typescript-language-server", "vtsls", "gopls",
    "pyright", "pyright-langserver", "basedpyright", "basedpyright-langserver", "pylsp", "jedi-language-server",
    "ruff", "ruff-lsp", "clangd", "sourcekit-lsp", "lua-language-server", "eslint_d", "eslintserver", "jdtls",
    "solargraph", "ruby-lsp", "zls", "taplo", "yaml-language-server", "vscode-json-language-server",
    "vscode-css-language-server", "vscode-html-language-server", "vscode-eslint-language-server",
    "tailwindcss-language-server", "biome", "prettierd", "copilot-language-server", "marksman", "texlab",
    "omnisharp", "elixir-ls", "haskell-language-server", "svelteserver", "astro-ls", "intelephense",
    "bash-language-server", "docker-langserver", "terraform-ls", "dmypy", "watchman", "esbuild", "deno-lsp",
    "metals", "kotlin-language-server", "nil", "nixd", "ty",
];

const WRAPPERS: &[&str] = &["npm", "pnpm", "yarn", "npx", "pnpx", "bunx", "corepack", "env", "nohup", "sudo", "time", "caffeinate", "pnpm-native", "uv", "uvx", "poetry", "pipenv", "bundle", "doppler", "op", "dotenv", "direnv", "mise", "volta-shim", "script"];

const INTERPRETERS: &[&str] = &["node", "nodejs", "bun", "deno", "python", "ruby", "perl", "php", "tsx", "ts-node", "lua", "java", "bash", "sh", "zsh", "fish", "dash", "osascript", "rscript"];

const GENERIC_STEMS: &[&str] = &["index", "main", "cli", "bin", "start", "server", "app", "run", "__main__", "entry", "worker", "dev", "manage"];

fn lower_base(path: &str) -> String {
    base(path).trim_start_matches('-').to_lowercase()
}

fn stem(file: &str) -> &str {
    match file.rsplit_once('.') {
        Some((s, ext)) if !s.is_empty() && matches!(ext, "js" | "mjs" | "cjs" | "ts" | "mts" | "py" | "rb" | "sh" | "pl" | "php" | "lua" | "jar") => s,
        _ => file,
    }
}

/// The short binary name of a process: the executable's file name, or the process name.
pub fn bin_name(p: &ProcInfo) -> String {
    if !p.exe.is_empty() {
        lower_base(&p.exe)
    } else {
        lower_base(&p.name)
    }
}

fn is_interpreter(b: &str) -> bool {
    INTERPRETERS.contains(&b) || b.starts_with("python") || b.starts_with("ruby") || b.starts_with("node")
}

/// The top-level `.app` bundle a path sits in, if any.
pub fn bundle_of(exe: &str) -> Option<&str> {
    exe.find(".app/").map(|i| &exe[..i + 4])
}

fn is_system_path(path: &str) -> bool {
    ["/System/", "/usr/libexec/", "/usr/sbin/", "/sbin/", "/usr/lib/", "/Library/Apple/", "/Library/PrivilegedHelperTools/"]
        .iter()
        .any(|p| path.starts_with(p))
}

fn is_app_bundle(bundle: &str, ctx: &Ctx) -> bool {
    if ctx.gui_bundles.contains(bundle) {
        return true;
    }
    if bundle.starts_with("/System/Applications/") || bundle.starts_with("/System/Volumes/Preboot/Cryptexes/App/System/Applications/") {
        return true;
    }
    if is_system_path(bundle) {
        return false;
    }
    // interpreters ship as bundles too (homebrew's Python.app): those are not apps
    if ["/opt/homebrew/", "/usr/local/", "/nix/", "/opt/local/"].iter().any(|p| bundle.starts_with(p)) || bundle.contains(".framework/") {
        return false;
    }
    true
}

/// An interactive shell (or `login`): it belongs with whatever it runs inside.
pub fn is_transparent(p: &ProcInfo) -> bool {
    let b = bin_name(p);
    if b == "login" {
        return true;
    }
    if !SHELLS.contains(&b.as_str()) {
        return false;
    }
    !p.cmd.iter().skip(1).any(|a| {
        if let Some(flags) = a.strip_prefix('-') {
            !flags.starts_with('-') && flags.contains('c')
        } else {
            true
        }
    })
}

/// The known dev tool this process is, if it is one.
pub fn devtool_name(p: &ProcInfo) -> Option<String> {
    let b = bin_name(p);
    if DEVTOOLS.contains(&b.as_str()) {
        return Some(b);
    }
    // node/python/electron-helper running a language server script
    for a in p.cmd.iter().skip(1).take(4) {
        if a.starts_with('-') || !a.contains('/') {
            continue;
        }
        let s = stem(base(a)).to_lowercase();
        if DEVTOOLS.contains(&s.as_str()) {
            return Some(s);
        }
        if let Some(c) = a.split('/').map(str::to_lowercase).find(|c| c.len() >= 6 && DEVTOOLS.contains(&c.as_str())) {
            return Some(c);
        }
    }
    None
}

enum Hard {
    Pinned(Group),
    System,
    App(String),
    Shell,
    Dev,
}

fn hard(p: &ProcInfo, ctx: &Ctx) -> Hard {
    if let Some(g) = ctx.rules.group_for(&p.name) {
        return Hard::Pinned(g);
    }
    if bin_name(p) == "login" {
        return Hard::Shell;
    }
    if p.pid == 0 || p.uid < 500 {
        return Hard::System;
    }
    if devtool_name(p).is_some() {
        return Hard::Dev;
    }
    if let Some(b) = bundle_of(&p.exe)
        && is_app_bundle(b, ctx)
    {
        if let Some(g) = ctx.rules.group_for(app_name(b)) {
            return Hard::Pinned(g);
        }
        return Hard::App(b.to_string());
    }
    if is_system_path(&p.exe) {
        return Hard::System;
    }
    if is_transparent(p) {
        return Hard::Shell;
    }
    let base_bin = p.exe.is_empty() || p.exe.starts_with("/usr/bin/") || p.exe.starts_with("/bin/");
    if base_bin && p.ppid <= 1 && p.launchd_label.is_some() {
        return Hard::System;
    }
    Hard::Dev
}

pub fn app_name(bundle: &str) -> &str {
    let b = base(bundle);
    b.strip_suffix(".app").unwrap_or(b)
}

/// Related system processes that read better as one item.
fn system_name(p: &ProcInfo) -> String {
    let n = p.name.as_str();
    let roll = |prefixes: &[&str]| prefixes.iter().any(|x| n.starts_with(x));
    if roll(&["mds", "mdworker", "mdbulkimport", "mdsync", "corespotlightd", "spotlightknowledged", "Spotlight"]) {
        return "spotlight".into();
    }
    if roll(&["com.apple.WebKit"]) {
        return "webkit".into();
    }
    if roll(&["bluetooth", "BTLEServer", "BlueTool"]) {
        return "bluetooth".into();
    }
    if roll(&["cloudd", "bird", "cloudphotod", "iCloud", "cloudpaird"]) {
        return "icloud".into();
    }
    if roll(&["siri", "Siri", "assistantd", "assistant_"]) {
        return "siri".into();
    }
    if roll(&["coreaudiod", "com.apple.audio", "audiomxd", "AudioComponentRegistrar"]) {
        return "coreaudio".into();
    }
    if let Some(b) = bundle_of(&p.exe) {
        return app_name(b).to_string();
    }
    n.strip_prefix("com.apple.").unwrap_or(n).to_string()
}

fn dev_class(root: u32) -> Class {
    Class { group: Group::Dev, key: format!("dev:{root}"), kind: Kind::Dev { root_pid: root } }
}

fn dev_root(p: &ProcInfo, ctx: &Ctx) -> u32 {
    let mut cur = p;
    for _ in 0..32 {
        let Some(parent) = ctx.procs.get(&cur.ppid) else { break };
        if parent.pid <= 1 || parent.pid == cur.pid || is_transparent(parent) {
            break;
        }
        match hard(parent, ctx) {
            Hard::Dev => cur = parent,
            _ => break,
        }
    }
    cur.pid
}

pub fn classify(p: &ProcInfo, ctx: &Ctx) -> Class {
    match hard(p, ctx) {
        Hard::Pinned(group) => {
            let kind = match group {
                Group::Dev => Kind::Dev { root_pid: p.pid },
                _ => Kind::System,
            };
            Class { group, key: format!("pin:{}", p.name.to_lowercase()), kind }
        }
        Hard::System => {
            let name = system_name(p);
            Class { group: Group::System, key: format!("sys:{name}"), kind: Kind::System }
        }
        Hard::App(bundle) => Class { group: Group::Apps, key: format!("app:{bundle}"), kind: Kind::App { bundle } },
        Hard::Dev => dev_class(dev_root(p, ctx)),
        Hard::Shell => {
            // join whatever the shell runs inside: a terminal app, tmux, an agent...
            let mut cur = p;
            for _ in 0..16 {
                let Some(parent) = ctx.procs.get(&cur.ppid) else { break };
                if parent.pid <= 1 || parent.pid == cur.pid {
                    break;
                }
                match hard(parent, ctx) {
                    Hard::Shell => cur = parent,
                    Hard::App(bundle) => return Class { group: Group::Apps, key: format!("app:{bundle}"), kind: Kind::App { bundle } },
                    Hard::Dev => return dev_class(dev_root(parent, ctx)),
                    _ => break,
                }
            }
            dev_class(p.pid)
        }
    }
}

// ───────────────────────── naming dev items ─────────────────────────

fn wordlike(a: &str) -> bool {
    !a.is_empty() && a.len() <= 20 && !a.starts_with('-') && a.chars().all(|c| c.is_alphanumeric() || "-_.:@".contains(c))
}

/// Appends up to two subcommand-like words: `cargo` + `build`, `next` + `dev`, `tail` + `-f api.log`.
fn sub_words(out: &mut String, args: &[String]) {
    let filelike = |a: &str| {
        let b = base(a);
        wordlike(b) && b.rsplit_once('.').is_some_and(|(s, e)| !s.is_empty() && !e.is_empty() && e.chars().all(|c| c.is_ascii_alphabetic()))
    };
    let mut words = 0;
    let mut i = 0;
    while i < args.len() && words < 2 {
        let a = args[i].as_str();
        if a == "--watch" {
            out.push_str(" --watch");
            break;
        }
        if a.starts_with('-') {
            // keep a short flag only when it leads straight to a file: `tail -f api.log`
            if a.len() == 2 && args.get(i + 1).is_some_and(|n| filelike(n)) {
                out.push(' ');
                out.push_str(a);
                out.push(' ');
                out.push_str(base(&args[i + 1]));
            }
            break;
        }
        let word = if a.contains('/') {
            if filelike(a) { base(a) } else { break }
        } else if wordlike(a) {
            a
        } else {
            break;
        };
        out.push(' ');
        out.push_str(word);
        words += 1;
        i += 1;
    }
}

fn script_name(path: &str) -> String {
    let file = base(path);
    let st = stem(file);
    let generic = GENERIC_STEMS.contains(&st.to_lowercase().as_str());
    if let Some(i) = path.rfind("/node_modules/") {
        let rest: Vec<&str> = path[i + 14..].split('/').collect();
        if !generic {
            return st.to_string();
        }
        let pkg = match rest.as_slice() {
            [".bin", b, ..] => b,
            [scope, pkg, ..] if scope.starts_with('@') => pkg,
            [pkg, ..] => pkg,
            [] => st,
        };
        return pkg.to_string();
    }
    if generic {
        let dir = path.trim_end_matches(file).trim_end_matches('/');
        if !dir.is_empty() && base(dir) != "." {
            return format!("{}/{}", base(dir), file);
        }
    }
    file.to_string()
}

/// A readable name for a command line: `next dev`, `cargo build`, `seed.py`, `http.server`.
pub fn dev_name(cmd: &[String], fallback: &str) -> String {
    let Some(first) = cmd.first() else { return fallback.to_string() };
    let a0 = base(first).trim_start_matches('-').to_string();
    if a0.is_empty() {
        return fallback.to_string();
    }
    let lower = a0.to_lowercase();
    let args = &cmd[1..];
    let mut out;
    if is_interpreter(&lower) {
        let mut i = 0;
        let mut script = None;
        while i < args.len() {
            let a = &args[i];
            if a == "-m" && lower.starts_with("python") {
                if let Some(m) = args.get(i + 1) {
                    out = m.clone();
                    sub_words(&mut out, &args[i + 2..]);
                    return out;
                }
            } else if a == "-c" && SHELLS.contains(&lower.as_str()) {
                if let Some(s) = args.get(i + 1) {
                    let inner: Vec<String> = s.split_whitespace().take(6).map(String::from).collect();
                    return dev_name(&inner, &a0);
                }
            } else if matches!(a.as_str(), "-e" | "-c" | "--eval" | "-p" | "--print") {
                return format!("{a0} {a}");
            } else if matches!(a.as_str(), "-r" | "--require" | "--loader" | "--import" | "-W" | "-X" | "-jar" | "-cp" | "-classpath") {
                if a == "-jar"
                    && let Some(j) = args.get(i + 1)
                {
                    return base(j).to_string();
                }
                i += 1;
            } else if a == "run" && matches!(lower.as_str(), "bun" | "deno") && script.is_none() {
                // `bun run dev`: keep going, the next word is the thing
            } else if !a.starts_with('-') {
                script = Some(i);
                break;
            }
            i += 1;
        }
        match script {
            Some(i) => {
                out = script_name(&args[i]);
                sub_words(&mut out, &args[i + 1..]);
            }
            None => out = a0,
        }
    } else {
        out = a0;
        sub_words(&mut out, args);
    }
    crate::util::clip(&out, 30)
}

fn is_wrapper(p: &ProcInfo) -> bool {
    let b = bin_name(p);
    if WRAPPERS.contains(&b.as_str()) {
        return true;
    }
    // `sh -c "..."` and `node /path/to/npm run dev` only start the real thing
    if SHELLS.contains(&b.as_str()) {
        return p.cmd.iter().skip(1).any(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('c'));
    }
    if b.starts_with("node") {
        return p.cmd.get(1).map(|s| WRAPPERS.contains(&stem(base(s)).to_lowercase().as_str())).unwrap_or(false);
    }
    false
}

/// Picks the process a dev item is named after and returns its name: wrappers such as
/// `npm run dev` or `sh -c` are skipped in favour of what they started.
pub fn dev_item_name(root: &ProcInfo, members: &[&ProcInfo]) -> String {
    if let Some(t) = devtool_name(root) {
        return t;
    }
    let mut cur = root;
    for _ in 0..4 {
        if !is_wrapper(cur) {
            break;
        }
        let child = members.iter().filter(|m| m.ppid == cur.pid && m.pid != cur.pid && !is_transparent(m)).max_by_key(|m| m.mem);
        match child {
            Some(c) => cur = c,
            None => break,
        }
    }
    dev_name(&cur.cmd, &cur.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, ppid: u32, exe: &str, cmd: &[&str]) -> ProcInfo {
        ProcInfo {
            pid,
            ppid,
            uid: 501,
            name: base(exe).to_string(),
            exe: exe.to_string(),
            cmd: cmd.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    struct Fix {
        procs: HashMap<u32, ProcInfo>,
        gui: HashSet<String>,
        rules: Rules,
    }

    impl Fix {
        fn new(list: Vec<ProcInfo>) -> Fix {
            let mut procs: HashMap<u32, ProcInfo> = list.into_iter().map(|p| (p.pid, p)).collect();
            procs.insert(1, ProcInfo { pid: 1, uid: 0, name: "launchd".into(), exe: "/sbin/launchd".into(), ..Default::default() });
            Fix { procs, gui: HashSet::new(), rules: Rules::default() }
        }
        fn class(&self, pid: u32) -> Class {
            classify(&self.procs[&pid], &Ctx { procs: &self.procs, gui_bundles: &self.gui, rules: &self.rules })
        }
    }

    fn world() -> Fix {
        let mut root_daemon = p(60, 1, "/opt/thing/daemon", &["daemon"]);
        root_daemon.uid = 0;
        let mut windowserver = p(61, 1, "/System/Library/PrivateFrameworks/SkyLight.framework/Resources/WindowServer", &[]);
        windowserver.uid = 88;
        let mut login = p(101, 100, "/usr/bin/login", &["login", "-fp", "me"]);
        login.uid = 0;
        let mut pg = p(300, 1, "/opt/homebrew/opt/postgresql@16/bin/postgres", &["postgres", "-D", "/opt/homebrew/var/postgresql@16"]);
        pg.launchd_label = Some("homebrew.mxcl.postgresql@16".into());
        let mut agent = p(310, 1, "/usr/bin/ssh-agent", &["ssh-agent", "-l"]);
        agent.launchd_label = Some("com.openssh.ssh-agent".into());
        Fix::new(vec![
            root_daemon,
            windowserver,
            p(62, 1, "/usr/libexec/rapportd", &["rapportd"]),
            p(63, 1, "/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/Metadata.framework/Versions/A/Support/mdworker_shared", &[]),
            p(64, 1, "/System/Library/Frameworks/CoreServices.framework/Frameworks/Metadata.framework/Support/mds_stores", &[]),
            p(65, 1, "/System/Library/CoreServices/Dock.app/Contents/MacOS/Dock", &[]),
            p(66, 1, "/System/Library/CoreServices/Finder.app/Contents/MacOS/Finder", &[]),
            // terminal app with a login shell running things
            p(100, 1, "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal", &[]),
            login,
            p(102, 101, "/bin/zsh", &["-zsh"]),
            p(110, 102, "/opt/homebrew/bin/node", &["node", "/opt/homebrew/bin/npm", "run", "dev"]),
            p(111, 110, "/bin/sh", &["sh", "-c", "next dev"]),
            p(112, 111, "/opt/homebrew/bin/node", &["node", "/Users/me/code/web/node_modules/.bin/next", "dev"]),
            p(113, 112, "/opt/homebrew/bin/node", &["node", "/Users/me/code/web/node_modules/next/dist/server/lib/start-server.js"]),
            p(120, 102, "/Users/me/.cargo/bin/cargo", &["cargo", "build", "--release"]),
            p(121, 120, "/usr/bin/cc", &["cc", "-o", "x"]),
            // chrome with helpers
            p(200, 1, "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome", &[]),
            p(201, 200, "/Applications/Google Chrome.app/Contents/Frameworks/Google Chrome Framework.framework/Versions/1/Helpers/Google Chrome Helper (Renderer).app/Contents/MacOS/Google Chrome Helper (Renderer)", &[]),
            // editor spawning a language server and an integrated terminal
            p(210, 1, "/Applications/Visual Studio Code.app/Contents/MacOS/Electron", &[]),
            p(211, 210, "/Applications/Visual Studio Code.app/Contents/Frameworks/Code Helper (Plugin).app/Contents/MacOS/Code Helper (Plugin)", &[]),
            p(212, 211, "/Users/me/.vscode/extensions/rust-lang.rust-analyzer/server/rust-analyzer", &["rust-analyzer"]),
            p(213, 212, "/Users/me/.rustup/toolchains/stable/libexec/rust-analyzer-proc-macro-srv", &[]),
            p(214, 211, "/Applications/Visual Studio Code.app/Contents/Frameworks/Code Helper (Plugin).app/Contents/MacOS/Code Helper (Plugin)", &["Code Helper (Plugin)", "/Applications/Visual Studio Code.app/Contents/Resources/app/extensions/node_modules/typescript/lib/tsserver.js", "--useInferredProjectPerProjectRoot"]),
            p(215, 211, "/bin/zsh", &["/bin/zsh", "-il"]),
            p(216, 215, "/opt/homebrew/Cellar/python@3.14/3.14.7/Frameworks/Python.framework/Versions/3.14/Resources/Python.app/Contents/MacOS/Python", &["python3", "scripts/seed.py"]),
            pg,
            agent,
            // orphans
            p(320, 1, "/bin/sleep", &["sleep", "9999"]),
            p(321, 1, "/opt/homebrew/bin/node", &["node", "/Users/me/code/docs/node_modules/vite/bin/vite.js"]),
        ])
    }

    #[test]
    fn system_by_uid_and_path() {
        let f = world();
        assert_eq!(f.class(60).group, Group::System); // root owned, odd path
        assert_eq!(f.class(61).group, Group::System); // _windowserver
        assert_eq!(f.class(62).group, Group::System); // user owned but /usr/libexec
        assert_eq!(f.class(65).group, Group::System); // Dock.app lives under /System/Library
        assert_eq!(f.class(65).key, "sys:Dock");
        assert_eq!(f.class(310).group, Group::System); // launchd-managed /usr/bin agent
    }

    #[test]
    fn related_system_processes_roll_up() {
        let f = world();
        assert_eq!(f.class(63).key, "sys:spotlight");
        assert_eq!(f.class(64).key, "sys:spotlight");
    }

    #[test]
    fn apps_roll_helpers_into_the_top_bundle() {
        let f = world();
        let chrome = f.class(200);
        assert_eq!(chrome.group, Group::Apps);
        assert_eq!(chrome.key, "app:/Applications/Google Chrome.app");
        assert_eq!(f.class(201), chrome);
        // apps shipped in /System/Applications are still apps
        assert_eq!(f.class(100).key, "app:/System/Applications/Utilities/Terminal.app");
    }

    #[test]
    fn finder_is_an_app_only_when_it_has_a_dock_icon() {
        let mut f = world();
        assert_eq!(f.class(66).group, Group::System);
        f.gui.insert("/System/Library/CoreServices/Finder.app".into());
        assert_eq!(f.class(66).group, Group::Apps);
    }

    #[test]
    fn interactive_shells_join_their_terminal() {
        let f = world();
        let term = f.class(100);
        assert_eq!(f.class(101), term); // login
        assert_eq!(f.class(102), term); // -zsh
        assert_eq!(f.class(215), f.class(210)); // shell in the editor's terminal
    }

    #[test]
    fn dev_trees_group_under_their_root_command() {
        let f = world();
        let root = f.class(110);
        assert_eq!(root.group, Group::Dev);
        assert_eq!(root.kind, Kind::Dev { root_pid: 110 });
        for pid in [111, 112, 113] {
            assert_eq!(f.class(pid), root);
        }
        // a second command in the same shell is its own item, and a /usr/bin child follows it
        assert_eq!(f.class(120).kind, Kind::Dev { root_pid: 120 });
        assert_eq!(f.class(121), f.class(120));
    }

    #[test]
    fn language_servers_go_to_dev_not_the_editor() {
        let f = world();
        assert_eq!(f.class(211).group, Group::Apps);
        assert_eq!(f.class(212).kind, Kind::Dev { root_pid: 212 });
        assert_eq!(f.class(213), f.class(212)); // proc-macro server follows rust-analyzer
        assert_eq!(f.class(214).kind, Kind::Dev { root_pid: 214 }); // tsserver inside the helper binary
        assert_eq!(devtool_name(&f.procs[&214]).as_deref(), Some("tsserver"));
    }

    #[test]
    fn interpreters_in_bundles_and_services_and_orphans_are_dev() {
        let f = world();
        assert_eq!(f.class(216).kind, Kind::Dev { root_pid: 216 }); // homebrew Python.app
        assert_eq!(f.class(300).kind, Kind::Dev { root_pid: 300 }); // brew services postgres
        assert_eq!(f.class(320).kind, Kind::Dev { root_pid: 320 }); // orphaned /bin/sleep
        assert_eq!(f.class(321).kind, Kind::Dev { root_pid: 321 });
    }

    #[test]
    fn rules_pin_a_name_to_a_group() {
        let mut f = world();
        f.rules = Rules::parse("[pin]\nrapportd = \"dev\"\n\"google chrome\" = \"system\"\n");
        assert_eq!(f.class(62).group, Group::Dev);
        assert_eq!(f.class(200).group, Group::System);
    }

    fn name(cmd: &[&str]) -> String {
        let v: Vec<String> = cmd.iter().map(|s| s.to_string()).collect();
        dev_name(&v, "fallback")
    }

    #[test]
    fn dev_names_come_from_the_command() {
        assert_eq!(name(&["node", "/x/web/node_modules/.bin/next", "dev"]), "next dev");
        assert_eq!(name(&["node", "/x/web/node_modules/next/dist/bin/next", "dev", "--turbo"]), "next dev");
        assert_eq!(name(&["node", "/x/docs/node_modules/vite/bin/vite.js"]), "vite");
        assert_eq!(name(&["cargo", "build", "--release"]), "cargo build");
        assert_eq!(name(&["/opt/homebrew/opt/postgresql@16/bin/postgres", "-D", "/opt/homebrew/var/postgresql@16"]), "postgres");
        assert_eq!(name(&["python3", "scripts/seed.py"]), "seed.py");
        assert_eq!(name(&["python3", "-m", "http.server", "8765"]), "http.server 8765");
        assert_eq!(name(&["node", "dist/server.js"]), "dist/server.js");
        assert_eq!(name(&["npx", "tsc", "--watch"]), "npx tsc --watch");
        assert_eq!(name(&["node", "/x/node_modules/typescript/bin/tsc", "--watch"]), "tsc --watch");
        assert_eq!(name(&["tail", "-f", "/x/logs/api.log"]), "tail -f api.log");
        assert_eq!(name(&["sleep", "9999"]), "sleep 9999");
        assert_eq!(name(&["sh", "-c", "next dev -p 3000"]), "next dev");
        assert_eq!(name(&["redis-server", "*:6379"]), "redis-server");
        assert_eq!(name(&[]), "fallback");
    }

    #[test]
    fn wrappers_are_named_after_what_they_run() {
        let f = world();
        let members: Vec<&ProcInfo> = [110, 111, 112, 113].iter().map(|p| &f.procs[p]).collect();
        assert_eq!(dev_item_name(&f.procs[&110], &members), "next dev");
        assert_eq!(dev_item_name(&f.procs[&120], &[&f.procs[&120], &f.procs[&121]]), "cargo build");
        assert_eq!(dev_item_name(&f.procs[&214], &[&f.procs[&214]]), "tsserver");
        assert_eq!(dev_item_name(&f.procs[&216], &[&f.procs[&216]]), "seed.py");
    }
}
