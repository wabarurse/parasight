//! Optional user overrides from `~/.config/parasight/rules.toml`.
//!
//! ```toml
//! # force a process name or app name into a group
//! [pin]
//! ollama = "dev"
//! "my daemon" = "system"
//! ```

use crate::model::Group;
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Deserialize, Default)]
struct Raw {
    #[serde(default)]
    pin: HashMap<String, String>,
}

#[derive(Clone, Debug, Default)]
pub struct Rules {
    pub pin: HashMap<String, Group>,
    /// A message for the status line when the file exists but is wrong.
    pub warning: Option<String>,
}

impl Rules {
    pub fn parse(text: &str) -> Rules {
        match toml::from_str::<Raw>(text) {
            Ok(raw) => {
                let mut r = Rules {
                    ..Default::default()
                };
                for (name, group) in raw.pin {
                    match Group::parse(&group) {
                        Some(g) => {
                            r.pin.insert(name.to_lowercase(), g);
                        }
                        None => r.warning = Some(format!("rules.toml: unknown group \"{group}\" for \"{name}\"")),
                    }
                }
                r
            }
            Err(e) => Rules { warning: Some(format!("rules.toml ignored: {}", e.message())), ..Default::default() },
        }
    }

    pub fn load() -> Rules {
        let Some(home) = std::env::var_os("HOME") else { return Rules::default() };
        let path = std::path::Path::new(&home).join(".config/parasight/rules.toml");
        match std::fs::read_to_string(path) {
            Ok(text) => Rules::parse(&text),
            Err(_) => Rules::default(),
        }
    }

    pub fn group_for(&self, name: &str) -> Option<Group> {
        if self.pin.is_empty() {
            return None;
        }
        self.pin.get(&name.to_lowercase()).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_keep_and_pin() {
        let r = Rules::parse("keep = [\"Spotify\"]\n[pin]\nollama = \"dev\"\n\"My Daemon\" = \"system\"\n");
        assert_eq!(r.group_for("Ollama"), Some(Group::Dev));
        assert_eq!(r.group_for("my daemon"), Some(Group::System));
        assert_eq!(r.group_for("other"), None);
        assert!(r.warning.is_none());
    }

    #[test]
    fn bad_file_is_ignored_with_a_warning() {
        let r = Rules::parse("pin = 3");
        assert!(r.pin.is_empty());
        assert!(r.warning.is_some());
        let r = Rules::parse("[pin]\nx = \"nowhere\"\n");
        assert!(r.pin.is_empty());
        assert!(r.warning.is_some());
    }
}
