//! "what is this?": asks the user's own Claude Code about the selected item and shows the
//! one-line answer. The child process can only use parasight's read-only mcp tools, runs in an
//! empty directory, and gets the item's details with secrets already redacted. Nothing is sent
//! until `K` is pressed.

use crate::stream::{Event, Parser};
use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

/// Kept byte-for-byte stable so the model's prompt cache can be reused between questions.
pub const SYSTEM_PROMPT: &str = "you explain what one process or app running on the user's mac is. reply with exactly one short sentence: lowercase, under 12 words, no preamble, no markdown, no hedging. say what it is and what it belongs to, like \"this is your api server for the shop app\" or \"it's a google chrome browser instance\". if you cannot tell, say so plainly, like \"unknown helper process started by <parent>\". the details you are given are usually enough; use the parasight tools only when they are not. you can only look: never claim to have done anything.";

pub const NEEDS_CLAUDE: &str = "ask needs claude code: install it and run `claude` once to sign in";
const TOOL_PREFIX: &str = "mcp__parasight__";
const MAX_TOOL_ROUNDS: u32 = 5;
const TIMEOUT: Duration = Duration::from_secs(60);

/// The model to ask for. One line does not need a big model, so the fast one is the default.
pub fn model() -> String {
    std::env::var("PARASIGHT_ASK_MODEL").ok().filter(|m| !m.trim().is_empty()).unwrap_or_else(|| "haiku".to_string())
}

pub const QUESTION: &str = "what is this?";

/// The whole user message: the question, then what parasight knows about the item (redacted).
pub fn prompt_for(question: &str, details: &serde_json::Value) -> String {
    format!("{question}\n\n{details}")
}

/// The arguments for `claude`. Every one of these was checked against the installed cli.
pub fn command_args(exe: &str, prompt: &str, model: &str) -> Vec<String> {
    let mcp = serde_json::json!({ "mcpServers": { "parasight": { "command": exe, "args": ["mcp"] } } }).to_string();
    let allowed: Vec<String> = crate::mcp::TOOLS.iter().map(|t| format!("{TOOL_PREFIX}{t}")).collect();
    let mut a: Vec<String> = ["-p", prompt, "--output-format", "stream-json", "--verbose", "--include-partial-messages"].iter().map(|s| s.to_string()).collect();
    // only our mcp server, and none of the user's
    a.extend(["--mcp-config".to_string(), mcp, "--strict-mcp-config".to_string()]);
    // no built-in tools at all (no bash, no file edits, no web); our four may run unprompted,
    // and anything else is refused rather than asked about
    a.extend(["--tools".to_string(), String::new(), "--allowedTools".to_string(), allowed.join(","), "--permission-mode".to_string(), "dontAsk".to_string()]);
    // no settings files, so no hooks, plugins or project instructions; no skills or commands
    a.extend(["--setting-sources".to_string(), String::new(), "--disable-slash-commands".to_string(), "--no-session-persistence".to_string()]);
    a.extend(["--system-prompt".to_string(), SYSTEM_PROMPT.to_string(), "--model".to_string(), model.to_string()]);
    a
}

/// Squeezes an answer onto one plain lowercase line.
pub fn one_line(text: &str) -> String {
    let cleaned: String = text.chars().filter(|c| !matches!(c, '*' | '`' | '#')).collect();
    let joined = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    joined.trim_matches(|c| c == '"' || c == '\'').to_lowercase()
}

fn friendly_error(text: &str) -> String {
    let t = text.to_lowercase();
    if ["authenticate", "oauth", "/login", "api key", "not logged in", "sign in"].iter().any(|w| t.contains(w)) {
        NEEDS_CLAUDE.to_string()
    } else if t.contains("rate limit") || t.contains("429") || t.contains("usage limit") {
        "claude code is rate limited right now; try again in a bit".to_string()
    } else if t.is_empty() {
        "claude code gave no answer".to_string()
    } else {
        crate::util::clip(&one_line(text), 120)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Line {
    /// Still working; the text is what to show meanwhile ("…", "looking at get_item…", a partial answer).
    Waiting(String),
    Answer(String),
    Failed(String),
}

enum Msg {
    Event(Event),
    /// The child is gone; `Some` when it could not even be started.
    Exit(Option<std::io::ErrorKind>),
}

struct Run {
    key: String,
    rx: Receiver<Msg>,
    pgid: Arc<AtomicU32>,
    text: String,
    tool_rounds: u32,
    started: Instant,
}

pub struct Ask {
    /// Answers by a stable key (executable + command), kept for the session.
    cache: HashMap<String, Line>,
    run: Option<Run>,
    /// The Claude Code cli to run: `claude` from PATH unless PARASIGHT_CLAUDE says otherwise.
    program: String,
}

impl Default for Ask {
    fn default() -> Ask {
        let program = std::env::var("PARASIGHT_CLAUDE").ok().filter(|p| !p.trim().is_empty()).unwrap_or_else(|| "claude".to_string());
        Ask { cache: HashMap::new(), run: None, program }
    }
}

impl Ask {
    pub fn line(&self, key: &str) -> Option<&Line> {
        self.cache.get(key)
    }

    pub fn busy(&self) -> bool {
        self.run.is_some()
    }

    pub fn set(&mut self, key: &str, line: Line) {
        self.cache.insert(key.to_string(), line);
    }

    /// Starts a question unless the answer is already known. Returns false when it was.
    pub fn ask(&mut self, key: &str, prompt: String) -> bool {
        if matches!(self.cache.get(key), Some(Line::Answer(_))) || self.run.as_ref().is_some_and(|r| r.key == key) {
            return false;
        }
        self.cancel();
        let (tx, rx) = channel();
        let pgid = Arc::new(AtomicU32::new(0));
        let shared_pgid = pgid.clone();
        let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "parasight".into());
        let program = self.program.clone();
        std::thread::spawn(move || {
            let mut cmd = Command::new(program);
            cmd.args(command_args(&exe, &prompt, &model())).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
            // an empty directory of our own: no project CLAUDE.md, settings or hooks to pick up
            let dir = std::env::temp_dir().join("parasight-ask");
            if std::fs::create_dir_all(&dir).is_ok() {
                cmd.current_dir(&dir);
            }
            // When parasight itself was started from inside a Claude Code session, that
            // session's own wiring must not leak into this separate one.
            if std::env::var_os("CLAUDE_CODE_ENTRYPOINT").is_some() {
                for (k, _) in std::env::vars_os() {
                    let k = k.to_string_lossy();
                    if k.starts_with("CLAUDE_CODE_") || k.starts_with("CLAUDE_AGENT_SDK_") || k == "CLAUDECODE" || k == "ANTHROPIC_BASE_URL" {
                        cmd.env_remove(k.as_ref());
                    }
                }
            }
            // its own process group, so closing takes the mcp server it starts with it
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(Msg::Exit(Some(e.kind())));
                    return;
                }
            };
            shared_pgid.store(child.id(), Ordering::Relaxed);
            let mut parser = Parser::default();
            let mut buf = [0u8; 8192];
            // a debugging aid: keep the raw stream (this is how the test fixtures were recorded)
            let mut log = std::env::var_os("PARASIGHT_ASK_LOG").and_then(|p| std::fs::File::create(p).ok());
            if let Some(mut out) = child.stdout.take() {
                while let Ok(n) = out.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    if let Some(f) = &mut log {
                        let _ = std::io::Write::write_all(f, &buf[..n]);
                    }
                    for ev in parser.feed(&buf[..n]) {
                        if tx.send(Msg::Event(ev)).is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = child.wait();
            shared_pgid.store(0, Ordering::Relaxed);
            let _ = tx.send(Msg::Exit(None));
        });
        self.cache.insert(key.to_string(), Line::Waiting("…".into()));
        self.run = Some(Run { key: key.to_string(), rx, pgid, text: String::new(), tool_rounds: 0, started: Instant::now() });
        true
    }

    /// Stops the child we started (and the mcp server under it). A question left unanswered
    /// is forgotten, so K can ask again.
    pub fn cancel(&mut self) {
        if let Some(run) = self.run.take() {
            kill_group(&run.pgid);
            if matches!(self.cache.get(&run.key), Some(Line::Waiting(_))) {
                self.cache.remove(&run.key);
            }
        }
    }

    fn finish(&mut self, line: Line) {
        if let Some(run) = self.run.take() {
            kill_group(&run.pgid);
            self.cache.insert(run.key, line);
        }
    }

    /// Takes in whatever the child has said since last time. Returns true when the line changed.
    pub fn poll(&mut self) -> bool {
        let Some(run) = &mut self.run else { return false };
        let mut changed = false;
        let mut end = None;
        while let Ok(msg) = run.rx.try_recv() {
            changed = true;
            match msg {
                Msg::Event(ev) => match apply(&mut run.text, &mut run.tool_rounds, ev) {
                    Step::Show(line) => {
                        self.cache.insert(run.key.clone(), line);
                    }
                    Step::End(line) => {
                        end = Some(line);
                        break;
                    }
                    Step::Nothing => {}
                },
                Msg::Exit(Some(_)) => end = Some(Line::Failed(NEEDS_CLAUDE.into())),
                Msg::Exit(None) => end = Some(Line::Failed("claude code ended without an answer".into())),
            }
        }
        if end.is_none() && run.started.elapsed() > TIMEOUT {
            end = Some(Line::Failed("no answer from claude code after 60s".into()));
            changed = true;
        }
        if let Some(line) = end {
            self.finish(line);
        }
        changed
    }
}

impl Drop for Ask {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn kill_group(pgid: &AtomicU32) {
    let id = pgid.swap(0, Ordering::Relaxed);
    if id > 1 {
        // SAFETY: the process group of the claude child this program started.
        unsafe { libc::killpg(id as libc::pid_t, libc::SIGTERM) };
    }
}

#[derive(Debug, PartialEq)]
pub enum Step {
    Nothing,
    Show(Line),
    End(Line),
}

/// What one stream event does to the line. Pure, so the whole conversation can be tested
/// from recorded events.
pub fn apply(text: &mut String, tool_rounds: &mut u32, ev: Event) -> Step {
    match ev {
        Event::Init { tools, .. } => {
            // the lock: if claude code offers anything but our read-only tools, stop here
            match tools.iter().find(|t| !t.starts_with(TOOL_PREFIX)) {
                Some(t) => Step::End(Line::Failed(format!("ask stopped: claude code offered a tool it should not have ({t})"))),
                None => Step::Nothing,
            }
        }
        Event::Text(t) => {
            text.push_str(&t);
            Step::Show(Line::Waiting(format!("{}…", one_line(text))))
        }
        Event::ToolUse(name) => {
            *tool_rounds += 1;
            text.clear();
            if *tool_rounds > MAX_TOOL_ROUNDS {
                Step::End(Line::Failed("ask gave up: too many lookups".into()))
            } else if !name.starts_with(TOOL_PREFIX) {
                Step::End(Line::Failed(format!("ask stopped: claude code tried a tool it should not have ({name})")))
            } else {
                Step::Show(Line::Waiting(format!("looking at {}…", name.trim_start_matches(TOOL_PREFIX))))
            }
        }
        Event::Done { text: result, is_error } => {
            if is_error {
                Step::End(Line::Failed(friendly_error(&result)))
            } else {
                let answer = one_line(if result.trim().is_empty() { text } else { &result });
                Step::End(if answer.is_empty() { Line::Failed("claude code gave no answer".into()) } else { Line::Answer(answer) })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_child_is_locked_down_by_its_arguments() {
        let a = command_args("/usr/local/bin/parasight", "what is this?", "haiku");
        let after = |flag: &str| a[a.iter().position(|x| x == flag).unwrap_or_else(|| panic!("{flag} missing")) + 1].clone();
        assert_eq!(after("--tools"), "", "no built-in tools");
        assert_eq!(after("--allowedTools"), "mcp__parasight__get_item,mcp__parasight__list_items,mcp__parasight__search,mcp__parasight__system_summary");
        assert_eq!(after("--permission-mode"), "dontAsk");
        assert_eq!(after("--setting-sources"), "", "no user, project or local settings");
        assert!(a.contains(&"--strict-mcp-config".to_string()));
        assert!(a.contains(&"--disable-slash-commands".to_string()));
        assert!(a.contains(&"--no-session-persistence".to_string()));
        let mcp: serde_json::Value = serde_json::from_str(&after("--mcp-config")).unwrap();
        assert_eq!(mcp["mcpServers"]["parasight"]["command"], "/usr/local/bin/parasight");
        assert_eq!(mcp["mcpServers"]["parasight"]["args"], serde_json::json!(["mcp"]));
        assert_eq!(after("--system-prompt"), SYSTEM_PROMPT);
        assert_eq!(after("--model"), "haiku");
        assert!(!a.iter().any(|x| x.contains("dangerously") || x == "bypassPermissions"));
    }

    #[test]
    fn answers_become_one_plain_line() {
        assert_eq!(one_line("This is **macOS** `launchd`,\nthe system process\n\nthat starts everything."), "this is macos launchd, the system process that starts everything.");
        assert_eq!(one_line("\"it's a browser\""), "it's a browser");
    }

    #[test]
    fn errors_become_one_plain_line() {
        assert_eq!(friendly_error("Failed to authenticate: OAuth session expired and could not be refreshed"), NEEDS_CLAUDE);
        assert_eq!(friendly_error("Invalid API key · Please run /login"), NEEDS_CLAUDE);
        assert!(friendly_error("API Error: 429 rate limit").contains("rate limited"));
        assert_eq!(friendly_error(""), "claude code gave no answer");
        assert_eq!(friendly_error("Something\nelse broke"), "something else broke");
    }

    fn play(events: Vec<Event>) -> (Vec<Line>, Option<Line>) {
        let (mut text, mut rounds, mut shown) = (String::new(), 0, Vec::new());
        for ev in events {
            match apply(&mut text, &mut rounds, ev) {
                Step::Nothing => {}
                Step::Show(l) => shown.push(l),
                Step::End(l) => return (shown, Some(l)),
            }
        }
        (shown, None)
    }

    fn init(tools: &[&str]) -> Event {
        Event::Init { tools: tools.iter().map(|s| s.to_string()).collect(), model: "m".into() }
    }

    #[test]
    fn a_plain_answer_streams_in_and_settles() {
        let (shown, end) = play(vec![init(&["mcp__parasight__get_item"]), Event::Text("this is".into()), Event::Text(" Launchd.".into()), Event::Done { text: "this is Launchd.".into(), is_error: false }]);
        assert_eq!(shown, [Line::Waiting("this is…".into()), Line::Waiting("this is launchd.…".into())]);
        assert_eq!(end, Some(Line::Answer("this is launchd.".into())));
    }

    #[test]
    fn a_tool_call_shows_what_is_being_looked_at() {
        let (shown, end) = play(vec![
            init(&["mcp__parasight__get_item", "mcp__parasight__search"]),
            Event::Text("let me check".into()),
            Event::ToolUse("mcp__parasight__get_item".into()),
            Event::Text("it's a helper for slack".into()),
            Event::Done { text: "it's a helper for slack".into(), is_error: false },
        ]);
        assert_eq!(shown[1], Line::Waiting("looking at get_item…".into()));
        assert_eq!(shown[2], Line::Waiting("it's a helper for slack…".into()), "text from before the tool call is dropped");
        assert_eq!(end, Some(Line::Answer("it's a helper for slack".into())));
    }

    #[test]
    fn anything_but_our_tools_stops_the_conversation() {
        let (_, end) = play(vec![init(&["mcp__parasight__get_item", "Bash"]), Event::Text("x".into())]);
        assert!(matches!(end, Some(Line::Failed(m)) if m.contains("Bash")));
        let (_, end) = play(vec![init(&[]), Event::ToolUse("Bash".into())]);
        assert!(matches!(end, Some(Line::Failed(m)) if m.contains("should not have")));
        // and the number of lookups is capped
        let mut evs = vec![init(&["mcp__parasight__search"])];
        evs.extend((0..6).map(|_| Event::ToolUse("mcp__parasight__search".into())));
        let (shown, end) = play(evs);
        assert_eq!(shown.len(), 5);
        assert_eq!(end, Some(Line::Failed("ask gave up: too many lookups".into())));
    }

    #[test]
    fn a_missing_claude_binary_is_one_plain_hint() {
        let mut ask = Ask { cache: HashMap::new(), run: None, program: "/nonexistent/claude".into() };
        assert!(ask.ask("k", "what is this?".into()));
        assert_eq!(ask.line("k"), Some(&Line::Waiting("…".into())));
        let t0 = Instant::now();
        while ask.busy() && t0.elapsed() < Duration::from_secs(5) {
            ask.poll();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(ask.line("k"), Some(&Line::Failed(NEEDS_CLAUDE.into())));
        // a failure is not a cached answer: asking again starts over
        assert!(ask.ask("k", "what is this?".into()));
        ask.cancel();
        assert_eq!(ask.line("k"), None);
    }

    #[test]
    fn a_known_answer_is_not_asked_again() {
        let mut ask = Ask { cache: HashMap::new(), run: None, program: "/nonexistent/claude".into() };
        ask.set("k", Line::Answer("it's a browser".into()));
        assert!(!ask.ask("k", "what is this?".into()));
        assert!(!ask.busy());
    }
}
