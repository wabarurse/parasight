//! Talks to the real `parasight mcp` binary over pipes, the way Claude Code does.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

struct Client {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl Client {
    fn start() -> Client {
        let mut child = Command::new(env!("CARGO_BIN_EXE_parasight")).arg("mcp").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Client { child, stdin, stdout, next_id: 0 }
    }

    fn notify(&mut self, method: &str) {
        writeln!(self.stdin, "{}", json!({ "jsonrpc": "2.0", "method": method })).unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        writeln!(self.stdin, "{}", json!({ "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params })).unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let v: Value = serde_json::from_str(&line).expect("one json object per line");
        assert_eq!(v["id"], self.next_id);
        v
    }

    fn tool(&mut self, name: &str, args: Value) -> (bool, String) {
        let r = self.request("tools/call", json!({ "name": name, "arguments": args }));
        (r["result"]["isError"].as_bool().unwrap(), r["result"]["content"][0]["text"].as_str().unwrap().to_string())
    }
}

#[test]
fn a_piped_client_can_initialize_list_and_call_the_tools() {
    let mut c = Client::start();
    let init = c.request("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } }));
    assert_eq!(init["result"]["serverInfo"]["name"], "parasight");
    c.notify("notifications/initialized");

    let tools = c.request("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["get_item", "list_items", "search", "system_summary"]);

    // real data from this machine: launchd is always there
    let (is_error, text) = c.tool("get_item", json!({ "name_or_pid": "1" }));
    assert!(!is_error, "{text}");
    let item: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(item["name"], "launchd");
    assert_eq!(item["group"], "system");

    let (is_error, text) = c.tool("system_summary", json!({}));
    assert!(!is_error);
    let sys: Value = serde_json::from_str(&text).unwrap();
    assert!(sys["cores"].as_u64().unwrap() >= 1 && sys["process_count"].as_u64().unwrap() > 10);

    let (is_error, text) = c.tool("list_items", json!({ "group": "system", "limit": 3 }));
    assert!(!is_error);
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["items"].as_array().unwrap().len(), 3);

    // nothing that could change the machine exists, and bad input is refused
    let (is_error, text) = c.tool("kill", json!({ "pid": 1 }));
    assert!(is_error && text.contains("unknown tool"), "{text}");
    let (is_error, _) = c.tool("get_item", json!({ "name_or_pid": "1", "signal": 15 }));
    assert!(is_error);

    // closing stdin ends the server (and the helpers it started)
    drop(c.stdin);
    let status = c.child.wait().unwrap();
    assert!(status.success());
}
