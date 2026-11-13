//! `parasight mcp`: a small MCP server over stdio with four read-only tools. It is what the
//! ask feature gives Claude Code to look around with, and it can be added to any Claude Code
//! setup by hand. The protocol is hand-rolled: three JSON-RPC methods over newline-delimited
//! JSON are not worth an async runtime (the maintained crate, rmcp, needs tokio).
//!
//! There is deliberately no tool here that kills, signals, writes, runs a command or reaches
//! the network.

use crate::collect::Collector;
use crate::inspect::World;
use crate::interact::State;
use crate::model::{Group, Metric};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

pub const TOOLS: [&str; 4] = ["get_item", "list_items", "search", "system_summary"];

/// The tool list. Kept byte-stable: it is part of what the model caches.
pub fn tool_definitions() -> Value {
    json!([
        {
            "name": "get_item",
            "description": "Details of one app, dev process tree, container or system service running on this mac: group, executable, command line, working directory and project, listening ports, parent chain, bundle id, and its processes with pid, user, cpu, memory and network.",
            "inputSchema": {
                "type": "object",
                "properties": { "name_or_pid": { "type": "string", "description": "An item name (or part of one) as shown by list_items, or a process id." } },
                "required": ["name_or_pid"],
                "additionalProperties": false
            }
        },
        {
            "name": "list_items",
            "description": "What is running, as parasight groups it, with cpu, memory, network and process count per item.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "group": { "type": "string", "enum": ["apps", "dev", "system"], "description": "Only this group." },
                    "sort_by": { "type": "string", "enum": ["cpu", "memory", "network"], "description": "Default: memory." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50, "description": "Default: 15." }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "search",
            "description": "Find items whose name, path or command line contains the text.",
            "inputSchema": {
                "type": "object",
                "properties": { "query": { "type": "string", "description": "Text to look for, case-insensitive." } },
                "required": ["query"],
                "additionalProperties": false
            }
        },
        {
            "name": "system_summary",
            "description": "Totals for the whole machine: cpu, cores, memory, network, uptime, and counts per group.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }
    ])
}

#[derive(Debug, PartialEq)]
pub enum Call {
    GetItem(String),
    ListItems { group: Option<Group>, sort_by: Metric, limit: usize },
    Search(String),
    SystemSummary,
}

/// Checks a tool call against its schema before anything runs: an object, no unknown
/// properties, the right types, enums and ranges.
pub fn validate(name: &str, args: &Value) -> Result<Call, String> {
    let empty = serde_json::Map::new();
    let obj = match args {
        Value::Null => &empty,
        Value::Object(o) => o,
        _ => return Err("arguments must be an object".into()),
    };
    let allowed: &[&str] = match name {
        "get_item" => &["name_or_pid"],
        "list_items" => &["group", "sort_by", "limit"],
        "search" => &["query"],
        "system_summary" => &[],
        _ => return Err(format!("unknown tool \"{name}\"")),
    };
    if let Some(k) = obj.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(format!("unknown argument \"{k}\""));
    }
    let text = |key: &str| -> Result<String, String> {
        match obj.get(key) {
            Some(Value::String(s)) if !s.trim().is_empty() && s.len() <= 200 => Ok(s.clone()),
            Some(Value::String(_)) => Err(format!("\"{key}\" must be 1 to 200 characters")),
            Some(_) => Err(format!("\"{key}\" must be a string")),
            None => Err(format!("\"{key}\" is required")),
        }
    };
    match name {
        "get_item" => Ok(Call::GetItem(text("name_or_pid")?)),
        "search" => Ok(Call::Search(text("query")?)),
        "system_summary" => Ok(Call::SystemSummary),
        _ => {
            let group = match obj.get("group") {
                None => None,
                Some(Value::String(s)) => Some(match s.as_str() {
                    "apps" => Group::Apps,
                    "dev" => Group::Dev,
                    "system" => Group::System,
                    _ => return Err("\"group\" must be apps, dev or system".into()),
                }),
                Some(_) => return Err("\"group\" must be a string".into()),
            };
            let sort_by = match obj.get("sort_by") {
                None => Metric::Mem,
                Some(Value::String(s)) => match s.as_str() {
                    "cpu" => Metric::Cpu,
                    "memory" => Metric::Mem,
                    "network" => Metric::Net,
                    _ => return Err("\"sort_by\" must be cpu, memory or network".into()),
                },
                Some(_) => return Err("\"sort_by\" must be a string".into()),
            };
            let limit = match obj.get("limit") {
                None => 15,
                Some(v) => match v.as_u64() {
                    Some(n) if (1..=50).contains(&n) => n as usize,
                    _ => return Err("\"limit\" must be a whole number from 1 to 50".into()),
                },
            };
            Ok(Call::ListItems { group, sort_by, limit })
        }
    }
}

pub fn run_call(call: &Call, w: &World) -> Value {
    match call {
        Call::GetItem(q) => w.get_item(q),
        Call::ListItems { group, sort_by, limit } => w.list_items(*group, *sort_by, *limit),
        Call::Search(q) => w.search(q),
        Call::SystemSummary => w.system_summary(),
    }
}

fn reply(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Answers one JSON-RPC message. `world` is only called when a tool actually runs.
/// Notifications (no id) get no answer.
pub fn handle(msg: &Value, world: &mut dyn FnMut() -> World) -> Option<Value> {
    let method = msg.get("method").and_then(Value::as_str);
    let Some(id) = msg.get("id").filter(|i| !i.is_null()) else {
        return None; // a notification, or a response to something we never asked
    };
    let Some(method) = method else { return Some(error(id, -32600, "not a request")) };
    Some(match method {
        "initialize" => {
            let version = msg["params"]["protocolVersion"].as_str().unwrap_or("2024-11-05");
            reply(id, json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "parasight", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "Read-only views of what is running on this mac, as parasight groups it. Nothing here can change anything."
            }))
        }
        "ping" => reply(id, json!({})),
        "tools/list" => reply(id, json!({ "tools": tool_definitions() })),
        "tools/call" => {
            let name = msg["params"]["name"].as_str().unwrap_or("");
            match validate(name, &msg["params"]["arguments"]) {
                Ok(call) => {
                    let out = run_call(&call, &world());
                    reply(id, json!({ "content": [{ "type": "text", "text": out.to_string() }], "isError": out.get("error").is_some() }))
                }
                Err(why) => reply(id, json!({ "content": [{ "type": "text", "text": format!("invalid call: {why}") }], "isError": true })),
            }
        }
        _ => error(id, -32601, "method not found"),
    })
}

/// A collector that is started on the first tool call and refreshed when it has gone stale.
struct Live {
    col: Option<Collector>,
    last: Instant,
}

impl Live {
    fn world(&mut self) -> World {
        if self.col.is_none() {
            let shared = crate::sources::start(crate::sources::Options { docker: true, net_interval: 2 });
            let mut col = Collector::new(shared, crate::rules::Rules::load());
            col.tick();
            // cpu percentages need two looks a moment apart
            std::thread::sleep(Duration::from_millis(400));
            col.tick();
            self.col = Some(col);
            self.last = Instant::now();
        }
        let col = self.col.as_mut().expect("just set");
        if self.last.elapsed() > Duration::from_secs(2) {
            col.tick();
            self.last = Instant::now();
        }
        // the saved last-interacted stamps, read-only; a missing file is simply no history
        let state = Some(State::load(&State::default_path())).filter(|s| s.tracking_since > 0);
        World::from_collector(col, state)
    }
}

/// The server loop: one JSON message per line in, one per line out, until stdin closes.
pub fn serve() -> i32 {
    let mut live = Live { col: None, last: Instant::now() };
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let answer = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(&msg, &mut || live.world()),
            Err(_) => Some(error(&Value::Null, -32700, "parse error")),
        };
        if let Some(a) = answer
            && (writeln!(out, "{a}").is_err() || out.flush().is_err())
        {
            break;
        }
    }
    if let Some(col) = &live.col {
        col.shared.shutdown();
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspect::tests::world;

    fn call(msg: Value) -> Value {
        handle(&msg, &mut world).expect("an answer")
    }

    #[test]
    fn initialize_and_list_tools() {
        let init = call(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {} } }));
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(init["result"]["serverInfo"]["name"], "parasight");
        assert!(init["result"]["capabilities"]["tools"].is_object());
        // notifications are not answered
        assert!(handle(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }), &mut world).is_none());

        let list = call(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
        let tools = list["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, TOOLS);
        for t in tools {
            assert_eq!(t["inputSchema"]["additionalProperties"], false, "{}", t["name"]);
            assert_eq!(t["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn the_tool_list_is_read_only() {
        // the whole surface is these four names; none of them can change anything
        let text = tool_definitions().to_string().to_lowercase();
        for word in ["kill", "signal", "quit", "write", "delete", "execute", "shell", "run a command"] {
            assert!(!text.contains(word), "tool definitions mention \"{word}\"");
        }
        assert!(matches!(validate("kill_process", &json!({ "pid": 1 })), Err(e) if e.contains("unknown tool")));
    }

    #[test]
    fn tools_call_returns_redacted_json_text() {
        let r = call(json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "get_item", "arguments": { "name_or_pid": "next dev" } } }));
        assert_eq!(r["result"]["isError"], false);
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        let v: Value = serde_json::from_str(text).unwrap();
        assert_eq!(v["listening_ports"], json!([3000]));
        assert!(!text.contains("hunter2"));

        let r = call(json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "system_summary" } }));
        assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("\"cores\":8"));

        let r = call(json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": { "name": "get_item", "arguments": { "name_or_pid": "zzz" } } }));
        assert_eq!(r["result"]["isError"], true);
    }

    #[test]
    fn inputs_are_validated_before_anything_runs() {
        let mut ran = false;
        let mut w = || {
            ran = true;
            world()
        };
        let bad = [
            json!({ "name": "get_item", "arguments": {} }),
            json!({ "name": "get_item", "arguments": { "name_or_pid": 42 } }),
            json!({ "name": "get_item", "arguments": { "name_or_pid": "x", "signal": 9 } }),
            json!({ "name": "get_item", "arguments": "next dev" }),
            json!({ "name": "list_items", "arguments": { "group": "everything" } }),
            json!({ "name": "list_items", "arguments": { "limit": 0 } }),
            json!({ "name": "list_items", "arguments": { "limit": 1.5 } }),
            json!({ "name": "list_items", "arguments": { "sort_by": "pid" } }),
            json!({ "name": "search", "arguments": { "query": "   " } }),
            json!({ "name": "system_summary", "arguments": { "verbose": true } }),
            json!({ "name": "run_shell", "arguments": { "cmd": "rm -rf /" } }),
        ];
        for params in bad {
            let r = handle(&json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": params }), &mut w).unwrap();
            assert_eq!(r["result"]["isError"], true, "{params}");
            assert!(r["result"]["content"][0]["text"].as_str().unwrap().starts_with("invalid call"), "{params}");
        }
        assert!(!ran, "an invalid call must not reach the data");

        assert_eq!(validate("list_items", &json!({})), Ok(Call::ListItems { group: None, sort_by: Metric::Mem, limit: 15 }));
        assert_eq!(validate("list_items", &json!({ "group": "dev", "sort_by": "cpu", "limit": 3 })), Ok(Call::ListItems { group: Some(Group::Dev), sort_by: Metric::Cpu, limit: 3 }));
        assert_eq!(validate("system_summary", &Value::Null), Ok(Call::SystemSummary));
    }

    #[test]
    fn unknown_methods_and_garbage() {
        let r = call(json!({ "jsonrpc": "2.0", "id": 7, "method": "resources/list" }));
        assert_eq!(r["error"]["code"], -32601);
        assert_eq!(call(json!({ "jsonrpc": "2.0", "id": 8, "method": "ping" }))["result"], json!({}));
        assert!(handle(&json!({ "jsonrpc": "2.0", "id": 1, "result": {} }), &mut world).is_some_and(|r| r["error"]["code"] == -32600));
    }
}
