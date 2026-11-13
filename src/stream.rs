//! Parses the `--output-format stream-json` output of the Claude Code CLI: one JSON object per
//! line. Pure and defensive: bytes in, events out; partial lines wait for their rest, and
//! anything unrecognised is skipped.

use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The session is up. `tools` is everything the model can call.
    Init { tools: Vec<String>, model: String },
    /// A piece of the answer.
    Text(String),
    /// The model is calling a tool.
    ToolUse(String),
    /// The final result line.
    Done { text: String, is_error: bool },
}

#[derive(Default)]
pub struct Parser {
    buf: Vec<u8>,
    /// Text arrived as deltas, so the full assistant message that follows repeats it.
    saw_delta: bool,
}

impl Parser {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        self.buf.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            if let Ok(v) = serde_json::from_slice::<Value>(&line) {
                self.line(&v, &mut events);
            }
        }
        // a line that never ends is not something to keep buffering
        if self.buf.len() > 4 * 1024 * 1024 {
            self.buf.clear();
        }
        events
    }

    fn line(&mut self, v: &Value, out: &mut Vec<Event>) {
        match v["type"].as_str() {
            Some("system") if v["subtype"] == "init" => {
                let tools = v["tools"].as_array().map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect()).unwrap_or_default();
                out.push(Event::Init { tools, model: v["model"].as_str().unwrap_or("").to_string() });
            }
            Some("stream_event") => {
                let ev = &v["event"];
                match ev["type"].as_str() {
                    Some("content_block_delta") if ev["delta"]["type"] == "text_delta" => {
                        if let Some(t) = ev["delta"]["text"].as_str().filter(|t| !t.is_empty()) {
                            self.saw_delta = true;
                            out.push(Event::Text(t.to_string()));
                        }
                    }
                    Some("content_block_start") if ev["content_block"]["type"] == "tool_use" => {
                        if let Some(name) = ev["content_block"]["name"].as_str() {
                            self.saw_delta = true;
                            out.push(Event::ToolUse(name.to_string()));
                        }
                    }
                    _ => {}
                }
            }
            // without partial messages, whole assistant messages are all there is
            Some("assistant") if !self.saw_delta => {
                for block in v["message"]["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text") => out.extend(block["text"].as_str().filter(|t| !t.is_empty()).map(|t| Event::Text(t.to_string()))),
                        Some("tool_use") => out.extend(block["name"].as_str().map(|n| Event::ToolUse(n.to_string()))),
                        _ => {}
                    }
                }
            }
            Some("assistant") => self.saw_delta = false, // the next turn may again come either way
            Some("result") => {
                let text = v["result"].as_str().unwrap_or("").to_string();
                let is_error = v["is_error"].as_bool().unwrap_or(false) || v["subtype"].as_str().is_some_and(|s| s != "success");
                out.push(Event::Done { text, is_error });
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    //! The fixtures are real runs of the installed cli, trimmed to what matters (ids, paths
    //! and account details removed).
    use super::*;
    use crate::ask::{Line, Step, apply};

    const PLAIN: &str = include_str!("fixtures/plain.jsonl");
    const TOOL_USE: &str = include_str!("fixtures/tool_use.jsonl");
    const AUTH_ERROR: &str = include_str!("fixtures/auth_error.jsonl");

    fn events(text: &str) -> Vec<Event> {
        Parser::default().feed(text.as_bytes())
    }

    fn text_of(evs: &[Event]) -> String {
        evs.iter().filter_map(|e| if let Event::Text(t) = e { Some(t.as_str()) } else { None }).collect()
    }

    #[test]
    fn a_plain_answer() {
        let evs = events(PLAIN);
        assert!(matches!(&evs[0], Event::Init { tools, model } if tools.len() == 4 && tools.iter().all(|t| t.starts_with("mcp__parasight__")) && model == "claude-haiku-5-5"));
        assert_eq!(text_of(&evs), "this is macos launchd, the system process that starts and manages all other processes.");
        assert_eq!(evs.last(), Some(&Event::Done { text: "this is macos launchd, the system process that starts and manages all other processes.".into(), is_error: false }));
        // the full assistant message repeats the deltas: it must not be counted twice
        assert_eq!(evs.iter().filter(|e| matches!(e, Event::Text(_))).count(), 8);
    }

    #[test]
    fn a_turn_with_tool_calls() {
        let evs = events(TOOL_USE);
        let tools: Vec<&str> = evs.iter().filter_map(|e| if let Event::ToolUse(n) = e { Some(n.as_str()) } else { None }).collect();
        // three calls were made in that run (two were rejected by input validation); each is seen once
        assert_eq!(tools, ["mcp__parasight__get_item"; 3]);
        assert!(matches!(evs.last(), Some(Event::Done { text, is_error: false }) if text.starts_with("Finder and launchd")));
        // thinking blocks, tool results and status lines produce nothing
        assert_eq!(evs.len(), 1 + 3 + evs.iter().filter(|e| matches!(e, Event::Text(_))).count() + 1);
    }

    #[test]
    fn events_split_across_chunks_come_out_the_same() {
        let whole = events(TOOL_USE);
        for size in [1, 7, 64, 1000] {
            let mut p = Parser::default();
            let mut got = Vec::new();
            for chunk in TOOL_USE.as_bytes().chunks(size) {
                got.extend(p.feed(chunk));
            }
            assert_eq!(got, whole, "chunk size {size}");
        }
        // a multi-byte character cut in half is still one character
        let line = "{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"café…\"}}}\n";
        let mut p = Parser::default();
        let cut = line.find('é').unwrap() + 1;
        assert!(p.feed(&line.as_bytes()[..cut]).is_empty());
        assert_eq!(p.feed(&line.as_bytes()[cut..]), [Event::Text("café…".into())]);
    }

    #[test]
    fn a_failed_sign_in_is_an_error_result_not_an_answer() {
        let evs = events(AUTH_ERROR);
        assert!(matches!(evs.last(), Some(Event::Done { is_error: true, .. })));
        // through the same steps the ui takes, it ends as the one-line hint
        let (mut text, mut rounds, mut end) = (String::new(), 0, None);
        for ev in evs {
            if let Step::End(l) = apply(&mut text, &mut rounds, ev) {
                end = Some(l);
            }
        }
        assert_eq!(end, Some(Line::Failed(crate::ask::NEEDS_CLAUDE.into())));
    }

    #[test]
    fn unknown_events_and_garbage_are_ignored() {
        let input = concat!(
            "not json at all\n",
            "{\"type\":\"brand_new_event\",\"payload\":[1,2,3]}\n",
            "{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\"}}}\n",
            "{\"type\":\"stream_event\",\"event\":{\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}}\n",
            "[1,2,3]\n",
            "\n",
            "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":false,\"result\":\"\"}\n",
            "{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"no newline yet\"}}}",
        );
        let evs = events(input);
        // only the result line means something, and a non-success subtype is an error even without the flag
        assert_eq!(evs, [Event::Done { text: String::new(), is_error: true }]);
    }

    #[test]
    fn whole_messages_are_used_when_there_are_no_partial_ones() {
        let input = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"mcp__parasight__search\",\"input\":{\"query\":\"x\"}}]}}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"it's a helper\"}]}}\n",
        );
        assert_eq!(events(input), [Event::ToolUse("mcp__parasight__search".into()), Event::Text("it's a helper".into())]);
    }
}
