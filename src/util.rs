//! Number and text formatting. Units are lowercase on purpose (the whole ui is).

pub fn pct(v: f32) -> String {
    format!("{v:.1}%")
}

pub fn mem(bytes: u64) -> String {
    let mb = bytes as f64 / 1_048_576.0;
    if mb >= 1024.0 {
        format!("{:.2} gb", mb / 1024.0)
    } else {
        format!("{} mb", mb.round() as u64)
    }
}

/// Bytes per second, shown as kb or mb (the caller appends "/s").
pub fn rate(bps: f64) -> String {
    let kb = bps / 1024.0;
    if kb >= 1024.0 {
        format!("{:.1} mb", kb / 1024.0)
    } else {
        format!("{} kb", kb.round() as u64)
    }
}

pub fn ftime(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// A rough duration for sentences: 45s, 12m, 3h 5m, 2d.
pub fn dur(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        let (h, m) = (secs / 3600, secs / 60 % 60);
        if m == 0 || h >= 6 {
            format!("{h}h")
        } else {
            format!("{h}h {m}m")
        }
    } else {
        format!("{}d", secs / 86_400)
    }
}

pub fn tilde(path: &str, home: &str) -> String {
    if !home.is_empty() && (path == home || path.starts_with(&format!("{home}/"))) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    }
}

pub fn base(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path)
}

pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(pct(3.17), "3.2%");
        assert_eq!(mem(300 * 1_048_576), "300 mb");
        assert_eq!(mem(1536 * 1_048_576), "1.50 gb");
        assert_eq!(rate(2048.0), "2 kb");
        assert_eq!(rate(3.0 * 1_048_576.0), "3.0 mb");
        assert_eq!(ftime(75), "1:15");
        assert_eq!(ftime(3675), "1:01:15");
        assert_eq!(dur(45), "45s");
        assert_eq!(dur(190), "3m");
        assert_eq!(dur(3 * 3600 + 300), "3h 5m");
        assert_eq!(dur(2 * 86_400 + 5), "2d");
        assert_eq!(tilde("/Users/a/code", "/Users/a"), "~/code");
        assert_eq!(tilde("/Users/ab/code", "/Users/a"), "/Users/ab/code");
        assert_eq!(base("/a/b/c.app/"), "c.app");
        assert_eq!(clip("abcdef", 4), "abc…");
    }
}
