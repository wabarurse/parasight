//! Removes secrets from command lines before anything leaves the machine (the ask feature and
//! the mcp tools). Pure: text in, text out. Process environments are never read at all, so
//! there is nothing of theirs to redact.

pub const MASK: &str = "[redacted]";

const SECRET_WORDS: &[&str] = &["password", "passwd", "passphrase", "pwd", "token", "secret", "apikey", "api-key", "api_key", "accesskey", "access-key", "access_key", "privatekey", "private-key", "private_key", "credential", "auth", "bearer", "cookie", "session", "signature"];

/// Well-known token shapes: redacted whatever their length or entropy.
const TOKEN_PREFIXES: &[&str] = &["sk-", "sk_", "pk_live", "rk_live", "ghp_", "gho_", "ghs_", "ghu_", "github_pat_", "glpat-", "xoxb-", "xoxp-", "xoxa-", "xapp-", "AKIA", "ASIA", "eyJ", "ya29.", "AIza", "npm_", "dop_v1_", "hf_"];

fn looks_secret(name: &str) -> bool {
    let n = name.to_lowercase();
    SECRET_WORDS.iter().any(|w| n.contains(w)) || n.ends_with("key") || n.ends_with("_pw") || n == "pw" || n == "pass"
}

fn entropy(s: &str) -> f64 {
    let mut counts = [0u32; 256];
    for b in s.bytes() {
        counts[b as usize] += 1;
    }
    let n = s.len() as f64;
    counts.iter().filter(|&&c| c > 0).map(|&c| c as f64 / n).map(|p| -p * p.log2()).sum()
}

/// A long random-looking run of token characters: mixed letters and digits, high entropy.
fn high_entropy(run: &str) -> bool {
    run.len() >= 24 && run.bytes().any(|b| b.is_ascii_digit()) && run.bytes().any(|b| b.is_ascii_alphabetic()) && entropy(run) >= 3.6
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '+'
}

/// Masks token-shaped pieces inside one word, keeping the rest (paths, hosts) readable.
fn mask_tokens(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if !run.is_empty() {
            let known = TOKEN_PREFIXES.iter().any(|p| run.starts_with(p)) && run.len() >= 16;
            out.push_str(if known || high_entropy(run) { MASK } else { run });
            run.clear();
        }
    };
    for c in word.chars() {
        if is_token_char(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// `scheme://user:pass@host` -> `scheme://[redacted]@host`
fn mask_url_credentials(word: &str) -> String {
    let Some(scheme_end) = word.find("://") else { return word.to_string() };
    let rest = &word[scheme_end + 3..];
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{}://{MASK}{}", &word[..scheme_end], &rest[at..]),
        None => word.to_string(),
    }
}

fn clean_word(word: &str) -> String {
    mask_tokens(&mask_url_credentials(word))
}

/// Redacts one argument vector. Knows that the word after `--token` is the token.
pub fn redact_args(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut mask_next = false;
    for a in args {
        if mask_next {
            mask_next = false;
            // a following flag means the secret flag took no value
            if !a.starts_with('-') {
                out.push(MASK.to_string());
                continue;
            }
        }
        if let Some(flag) = a.strip_prefix("--").or_else(|| a.strip_prefix('-')).filter(|f| !f.is_empty()) {
            if let Some((name, _value)) = flag.split_once('=') {
                if looks_secret(name) {
                    out.push(format!("{}={MASK}", &a[..a.len() - flag.len() + name.len()]));
                    continue;
                }
            } else if looks_secret(flag) || a == "-p" {
                // `-p` is a password for mysql, psql and friends; a port is not worth the risk
                mask_next = true;
                out.push(a.clone());
                continue;
            } else if a.len() > 2 && a.starts_with("-p") && !a.starts_with("--") && !a[2..].chars().all(|c| c.is_ascii_digit()) && a[2..].chars().any(|c| c.is_ascii_digit()) {
                // mysql -pS3cret (attached password)
                out.push(format!("-p{MASK}"));
                continue;
            }
        } else if let Some((key, _value)) = a.split_once('=')
            && !key.contains('/')
            && looks_secret(key)
        {
            out.push(format!("{key}={MASK}"));
            continue;
        }
        out.push(clean_word(a));
    }
    out
}

/// Redacts free text (a command line as one string, a path line).
pub fn redact_text(text: &str) -> String {
    let words: Vec<String> = text.split(' ').map(String::from).collect();
    redact_args(&words).join(" ")
}

/// Shortens the home directory to `~`, which also keeps the account name out of what is sent.
pub fn hide_home(text: &str, home: &str) -> String {
    if home.len() < 2 { text.to_string() } else { text.replace(home, "~") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> String {
        redact_args(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>()).join(" ")
    }

    #[test]
    fn values_after_secret_flags() {
        assert_eq!(args(&["tool", "--password", "hunter2", "--port", "3000"]), "tool --password [redacted] --port 3000");
        assert_eq!(args(&["tool", "--token=abc123", "--api-key", "k", "--secret", "s"]), "tool --token=[redacted] --api-key [redacted] --secret [redacted]");
        assert_eq!(args(&["curl", "--oauth2-bearer", "tok", "https://example.com"]), "curl --oauth2-bearer [redacted] https://example.com");
        assert_eq!(args(&["mysql", "-u", "root", "-p", "s3cret", "db"]), "mysql -u root -p [redacted] db");
        assert_eq!(args(&["mysql", "-pS3cret9"]), "mysql -p[redacted]");
        // a flag with no value does not swallow the next flag
        assert_eq!(args(&["tool", "--password", "--verbose"]), "tool --password --verbose");
        // ordinary flags are left alone
        assert_eq!(args(&["next", "dev", "--turbo", "-H", "0.0.0.0"]), "next dev --turbo -H 0.0.0.0");
    }

    #[test]
    fn key_value_pairs_whose_key_looks_secret() {
        assert_eq!(args(&["env", "DATABASE_PASSWORD=hunter2", "NODE_ENV=production", "node", "server.js"]), "env DATABASE_PASSWORD=[redacted] NODE_ENV=production node server.js");
        assert_eq!(args(&["AWS_SECRET_ACCESS_KEY=abc", "STRIPE_KEY=sk", "API_TOKEN=x", "PORT=3000"]), "AWS_SECRET_ACCESS_KEY=[redacted] STRIPE_KEY=[redacted] API_TOKEN=[redacted] PORT=3000");
    }

    #[test]
    fn urls_with_embedded_credentials() {
        assert_eq!(args(&["psql", "postgres://admin:hunter2@db.internal:5432/app"]), "psql postgres://[redacted]@db.internal:5432/app");
        assert_eq!(args(&["redis-cli", "-u", "redis://:pw@localhost:6379/0"]), "redis-cli -u redis://[redacted]@localhost:6379/0");
        // no credentials: untouched, including an @ in the path
        assert_eq!(args(&["curl", "https://example.com/users/@me?x=1"]), "curl https://example.com/users/@me?x=1");
    }

    #[test]
    fn long_high_entropy_strings_and_known_token_shapes() {
        assert_eq!(args(&["run", "x9Kp2LmQ8vR4tY7wZ1aB3cD5eF6gH0jN"]), "run [redacted]");
        assert_eq!(args(&["gh", "auth", "ghp_abcdefghijklmnopqrstuvwxyz0123456789"]), "gh auth [redacted]");
        assert_eq!(args(&["curl", "-H", "Authorization:", "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abc"]), "curl -H Authorization: [redacted].[redacted].abc");
        // inside a path only the token part goes
        assert_eq!(args(&["node", "/srv/x9Kp2LmQ8vR4tY7wZ1aB3cD5eF6gH0jN/app.js"]), "node /srv/[redacted]/app.js");
        // long but ordinary words and paths stay readable
        let plain = "/Applications/Visual Studio Code.app/Contents/Frameworks/Code Helper (Renderer).app/Contents/MacOS/Code";
        assert_eq!(redact_text(plain), plain);
        assert_eq!(args(&["node", "node_modules/typescript-language-server/lib/cli.mjs", "--stdio"]), "node node_modules/typescript-language-server/lib/cli.mjs --stdio");
        assert_eq!(args(&["--user-data-dir=/Users/me/Library/Application-Support/Google/Chrome"]), "--user-data-dir=/Users/me/Library/Application-Support/Google/Chrome");
    }

    #[test]
    fn text_and_home() {
        assert_eq!(redact_text("psql --password hunter2 -h db"), "psql --password [redacted] -h db");
        assert_eq!(hide_home("/Users/someone/code/web · npm run dev", "/Users/someone"), "~/code/web · npm run dev");
        assert_eq!(hide_home("/x", ""), "/x");
    }
}
