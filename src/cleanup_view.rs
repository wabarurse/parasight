//! The clean-up popup: two groups (recommended, to review), checkboxes, what each row would
//! save, live totals, and the y/n confirmation. Scoring is in `cleanup`; this is only the view.

use crate::actions::{Outcome, Target};
use crate::app::{ActionResult, App};
use crate::cleanup::{self, Candidate, Savings, Section};
use crate::native;
use crate::ui::*;
use crate::util::{mem, pct, rate};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq)]
pub enum RowStatus {
    Closing,
    Done(Outcome),
}

pub struct CleanView {
    pub cands: Vec<Candidate>,
    pub sel: usize,
    off: usize,
    /// The y/n question is showing.
    pub ask: bool,
    pub status: HashMap<String, RowStatus>,
    /// Where the popup's inside was drawn, and which candidate each of its lines shows.
    area: (u16, u16, u16, u16),
    lines: Vec<Option<usize>>,
}

pub fn savings_text(s: &Savings) -> String {
    format!("{} cpu · {} · {}/s", pct(s.cpu), mem(s.mem), rate(s.net))
}

/// `c`: work out the candidates now and check the recommended ones.
pub fn open(app: &mut App) {
    let facts = app.tracker.facts(&app.col);
    let cands = cleanup::score(&facts, &app.col.rules.keep, &app.col.home);
    app.clean = Some(CleanView { cands, sel: 0, off: 0, ask: false, status: HashMap::new(), area: (0, 0, 0, 0), lines: Vec::new() });
    app.flash.clear();
}

pub fn on_start(app: &mut App, key: &str) {
    if let Some(cv) = &mut app.clean {
        cv.status.insert(key.to_string(), RowStatus::Closing);
    }
}

pub fn on_result(app: &mut App, r: &ActionResult) {
    if let Some(cv) = &mut app.clean {
        cv.status.insert(r.key.clone(), RowStatus::Done(r.outcome.clone()));
    }
}

/// Closed rows leave the list; the message repeats what they saved.
pub fn on_closed(app: &mut App, closed: &[&str]) -> Option<String> {
    let cv = app.clean.as_mut()?;
    let mut saved: Vec<Candidate> = cv.cands.iter().filter(|c| closed.contains(&c.key.as_str())).cloned().collect();
    if saved.is_empty() {
        return None;
    }
    saved.iter_mut().for_each(|c| c.ticked = true);
    let saved = cleanup::savings(&saved);
    cv.cands.retain(|c| !closed.contains(&c.key.as_str()));
    cv.sel = cv.sel.min(cv.cands.len().saturating_sub(1));
    Some(format!("closed {} · saved {}", saved.count, savings_text(&saved)))
}

pub fn on_key(app: &mut App, key: KeyEvent) {
    let busy = app.busy();
    let Some(cv) = &mut app.clean else { return };
    let n = cv.cands.len();
    if cv.ask {
        cv.ask = false;
        if key.code == KeyCode::Char('y') {
            let keys: Vec<String> = cv.cands.iter().filter(|c| c.ticked).map(|c| c.key.clone()).collect();
            let targets: Vec<(String, Target)> = keys.into_iter().filter_map(|k| app.col.target(&k).map(|t| (k, t))).collect();
            app.flash = format!("closing {}…", targets.len());
            app.start(targets);
        }
        return;
    }
    app.flash.clear();
    match key.code {
        KeyCode::Char('j') | KeyCode::Down if n > 0 => cv.sel = (cv.sel + 1).min(n - 1),
        KeyCode::Char('k') | KeyCode::Up => cv.sel = cv.sel.saturating_sub(1),
        KeyCode::Char('g') | KeyCode::Home => cv.sel = 0,
        KeyCode::Char('G') | KeyCode::End => cv.sel = n.saturating_sub(1),
        KeyCode::Char(' ') | KeyCode::Char('x') if n > 0 && !busy => cv.cands[cv.sel].ticked ^= true,
        KeyCode::Char('a') if !busy => {
            let all = cv.cands.iter().all(|c| c.ticked);
            cv.cands.iter_mut().for_each(|c| c.ticked = !all);
        }
        KeyCode::Enter if !busy => {
            if cv.cands.iter().any(|c| c.ticked) {
                cv.ask = true;
            } else {
                app.flash = "nothing checked: press space on a row first".into();
            }
        }
        KeyCode::Char('r') if !busy => open(app),
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('c') => app.clean = None,
        _ => {}
    }
}

pub fn on_mouse(app: &mut App, m: MouseEvent) {
    let busy = app.busy();
    let Some(cv) = &mut app.clean else { return };
    let (x, y, w, h) = cv.area;
    match m.kind {
        MouseEventKind::ScrollDown => cv.sel = (cv.sel + 1).min(cv.cands.len().saturating_sub(1)),
        MouseEventKind::ScrollUp => cv.sel = cv.sel.saturating_sub(1),
        MouseEventKind::Down(MouseButton::Left) if !cv.ask && !busy && m.column >= x && m.column < x + w && m.row >= y && m.row < y + h => {
            if let Some(Some(i)) = cv.lines.get((m.row - y) as usize).copied()
                && let Some(c) = cv.cands.get_mut(i)
            {
                cv.sel = i;
                c.ticked ^= true;
            }
        }
        _ => {}
    }
}

/// The bottom line while the popup is open.
pub fn footer(app: &App) -> Option<Vec<Seg>> {
    let cv = app.clean.as_ref()?;
    if cv.ask {
        let sv = cleanup::savings(&cv.cands);
        let text = format!(" close {} item{}, saving {}? ", sv.count, if sv.count == 1 { "" } else { "s" }, savings_text(&sv));
        return Some(vec![s(text, fg(AMBER)), raw("y", bold(TEAL)), s(" / ", fg(DIM)), raw("n", bold(TEAL))]);
    }
    if !app.flash.is_empty() {
        return Some(vec![s(format!(" {}", app.flash), fg(DIM))]);
    }
    Some(keys(&[("j/k", "move"), ("space", "check / uncheck"), ("a", "all"), ("enter", "clean"), ("r", "rescan"), ("esc", "close")]))
}

enum Line {
    Blank,
    Header,
    Section(Section),
    Row(usize),
    Empty,
}

pub fn draw(buf: &mut Buffer, area: Rect, app: &mut App) {
    // dim the main view underneath
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_style(Style::new().fg(VERY_DIM).bg(ratatui::style::Color::Reset).remove_modifier(Modifier::BOLD));
        }
    }
    let since = app.tracker.tracking_since();
    let Some(cv) = &mut app.clean else { return };
    if area.width < 40 || area.height < 9 {
        return;
    }
    let pw = area.width.saturating_sub(4).min(96);
    let piw = pw as usize - 2;
    // columns: checkbox 5, name, group 5, reason, cpu 7, memory 10, network 10, 1 spare
    let name_w = if piw >= 80 { 20 } else { 14 };
    let why_w = piw.saturating_sub(5 + name_w + 5 + 28);
    let left_w = 5 + name_w + 5 + why_w;

    let mut lines = vec![Line::Blank, Line::Header];
    let mut last = None;
    for (i, c) in cv.cands.iter().enumerate() {
        if last != Some(c.section()) {
            if last.is_some() {
                lines.push(Line::Blank);
            }
            lines.push(Line::Section(c.section()));
            last = Some(c.section());
        }
        lines.push(Line::Row(i));
    }
    if cv.cands.is_empty() {
        lines.push(Line::Blank);
        lines.push(Line::Empty);
    }
    // the header stays; the rest scrolls when the terminal is short
    let max_body = (area.height as usize).saturating_sub(2 + 2 + 3).max(1);
    let body: &[Line] = &lines[2..];
    let sel_line = body.iter().position(|l| matches!(l, Line::Row(i) if *i == cv.sel)).unwrap_or(0);
    if sel_line < cv.off + 1 {
        cv.off = sel_line.saturating_sub(1);
    }
    if sel_line >= cv.off + max_body {
        cv.off = sel_line + 1 - max_body;
    }
    cv.off = cv.off.min(body.len().saturating_sub(max_body));
    let visible = body.len().min(max_body);
    let ph = (2 + visible + 3 + 2) as u16;
    let r = Rect::new(area.x + (area.width - pw) / 2, area.y + (area.height.saturating_sub(ph)) / 2, pw, ph);

    let sv = cleanup::savings(&cv.cands);
    frame(buf, r, BORDER_ON, &[s(" clean up ", bold(TEAL))], &[s(format!(" {} of {} checked ", sv.count, cv.cands.len()), fg(DIM))]);
    let (x, iw) = (r.x + 1, pw - 2);
    let hd = bold(BRIGHT);
    cv.area = (x, r.y + 1, iw, ph - 2);
    cv.lines.clear();
    let mut y = r.y + 1;
    let shown = lines[..2].iter().chain(body.iter().skip(cv.off).take(visible));
    for line in shown {
        let mut which = None;
        match line {
            Line::Blank => put(buf, x, y, iw, &[], None),
            Line::Header => put(buf, x, y, iw, &[raw(" ".repeat(left_w), hd), s(rp("cpu", 7), hd), s(rp("memory", 10), hd), s(rp("network", 10), hd)], None),
            Line::Section(sec) => {
                let (a, b) = match sec {
                    Section::Recommended => ("recommended", "checked for you"),
                    Section::ToReview => ("to review", "not checked · tick what you want closed"),
                };
                put(buf, x, y, iw, &[s(format!(" {a}"), hd), s(format!(" · {b}"), fg(DIM))], None);
            }
            Line::Empty => put(buf, x, y, iw, &[s("   nothing to clean up right now", fg(DIM))], None),
            Line::Row(i) => {
                let c = &cv.cands[*i];
                which = Some(*i);
                let on = c.ticked;
                let (why, why_style) = match cv.status.get(&c.key) {
                    Some(RowStatus::Closing) => ("closing…".to_string(), fg(AMBER)),
                    Some(RowStatus::Done(Outcome::Closed)) => ("closed".to_string(), fg(GREEN)),
                    Some(RowStatus::Done(Outcome::StillRunning)) => ("still running".to_string(), fg(RED)),
                    Some(RowStatus::Done(Outcome::Refused(w))) => (format!("refused: {w}"), fg(RED)),
                    None => (c.reason.clone(), fg(if on { TEXT } else { DIM })),
                };
                let num = |c| fg(if on { c } else { DIM });
                let row = [
                    s(if on { " [x] " } else { " [ ] " }, fg(if on { TEAL } else { DIM })),
                    s(lp(&c.name, name_w), if on { bold(BRIGHT) } else { fg(TEXT) }),
                    s(lp(c.group.name(), 5), fg(DIM)),
                    s(lp(&why, why_w), why_style),
                    s(rp(&pct(c.cpu), 7), num(GREEN)),
                    s(rp(&mem(c.mem), 10), num(LAVENDER)),
                    s(rp(&format!("{}/s", rate(c.net)), 10), num(TEAL)),
                    raw(" ", Style::new()),
                ];
                put(buf, x, y, iw, &row, (*i == cv.sel).then_some(SEL_BG));
            }
        }
        cv.lines.push(which);
        y += 1;
    }
    separator(buf, r, y, BORDER_ON);
    let totals = [
        s(lp(&format!(" saves with {} checked", sv.count), left_w), hd),
        s(rp(&pct(sv.cpu), 7), fg(GREEN)),
        s(rp(&mem(sv.mem), 10), fg(LAVENDER)),
        s(rp(&format!("{}/s", rate(sv.net)), 10), fg(TEAL)),
    ];
    put(buf, x, y + 1, iw, &totals, None);
    const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
    let (wd, h, m, _) = native::local_time(since);
    // the last line spells out the selected row's reason when the column had to cut it
    let cut = cv.cands.get(cv.sel).filter(|c| c.reason.chars().count() > why_w && !cv.status.contains_key(&c.key));
    match cut {
        Some(c) => put(buf, x, y + 2, iw, &[s(format!(" {}: {}", c.name, c.reason), fg(DIM))], None),
        None => {
            let note = format!(" input tracked since {} {h:02}:{m:02} · apps are asked to quit · dev processes get sigterm", DAYS[wd as usize % 7]);
            put(buf, x, y + 2, iw, &[s(note, fg(VERY_DIM))], None);
        }
    }
}
