//! Drawing. Everything is written straight into the ratatui buffer, one line of styled
//! segments at a time, so every cell is placed by hand. All text is lowercased here
//! (`s`), except key hints (`raw`).

use crate::app::{App, Focus, Row, RowKind};
use crate::model::*;
use crate::native;
use crate::util::{ftime, mem, pct, rate};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use std::collections::VecDeque;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const GREEN: Color = Color::Rgb(0x84, 0xb4, 0x8a);
pub const TEAL: Color = Color::Rgb(0x74, 0xaf, 0xb5);
pub const LAVENDER: Color = Color::Rgb(0xa9, 0x9b, 0xd0);
pub const AMBER: Color = Color::Rgb(0xcf, 0xae, 0x6b);
pub const RED: Color = Color::Rgb(0xd4, 0x82, 0x7a);
pub const TEXT: Color = Color::Rgb(0xc9, 0xd1, 0xd9);
pub const BRIGHT: Color = Color::Rgb(0xe6, 0xed, 0xf3);
pub const DIM: Color = Color::Rgb(0x8b, 0x94, 0x9e);
pub const VERY_DIM: Color = Color::Rgb(0x48, 0x4f, 0x58);
pub const BORDER: Color = Color::Rgb(0x4a, 0x52, 0x5c);
pub const BORDER_ON: Color = TEAL;
pub const SEL_BG: Color = Color::Rgb(0x23, 0x3a, 0x4a);
pub const TAB_BG: Color = Color::Rgb(0x3d, 0x5a, 0x5e);

const BLOCKS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

pub type Seg = (String, Style);

pub fn fg(c: Color) -> Style {
    Style::new().fg(c)
}
pub fn bold(c: Color) -> Style {
    Style::new().fg(c).add_modifier(Modifier::BOLD)
}
/// A lowercased segment: the whole ui is lowercase.
pub fn s(t: impl AsRef<str>, st: Style) -> Seg {
    (t.as_ref().to_lowercase(), st)
}
/// A segment that keeps its case: key hints only.
pub fn raw(t: impl Into<String>, st: Style) -> Seg {
    (t.into(), st)
}

pub fn load_color(cpu: f32) -> Color {
    if cpu >= 20.0 {
        RED
    } else if cpu >= 8.0 {
        AMBER
    } else {
        TEXT
    }
}

/// Left-aligned in `w` columns, cut with an ellipsis.
pub fn lp(t: &str, w: usize) -> String {
    let mut out = String::with_capacity(w);
    let mut n = 0;
    let total = t.width();
    for c in t.chars() {
        let cw = c.width().unwrap_or(0);
        if total > w && n + cw > w.saturating_sub(1) {
            out.push('…');
            n += 1;
            break;
        }
        out.push(c);
        n += cw;
    }
    out.extend(std::iter::repeat_n(' ', w.saturating_sub(n)));
    out
}

/// Right-aligned in `w` columns.
pub fn rp(t: &str, w: usize) -> String {
    let n = t.width();
    if n >= w { t.to_string() } else { format!("{}{t}", " ".repeat(w - n)) }
}

/// Writes segments on one line, padded to `w`; the last one that does not fit ends in "…".
pub fn put(buf: &mut Buffer, x: u16, y: u16, w: u16, segs: &[Seg], bg: Option<Color>) {
    if y >= buf.area.bottom() || x >= buf.area.right() {
        return;
    }
    let w = w.min(buf.area.right() - x) as usize;
    let total: usize = segs.iter().map(|(t, _)| t.width()).sum();
    let mut col = 0;
    for (t, st) in segs {
        if col >= w {
            break;
        }
        let st = match bg {
            Some(b) => st.bg(b),
            None => *st,
        };
        let room = w - col;
        if t.width() <= room && !(total > w && col + t.width() == w) {
            buf.set_stringn(x + col as u16, y, t, room, st);
            col += t.width();
        } else {
            buf.set_stringn(x + col as u16, y, lp(t, room), room, st);
            col = w;
        }
    }
    if col < w {
        let st = bg.map(|b| Style::new().bg(b)).unwrap_or_default();
        buf.set_stringn(x + col as u16, y, " ".repeat(w - col), w - col, st);
    }
}

/// A box with an optional title on the left of the top border and text on the right.
pub fn frame(buf: &mut Buffer, r: Rect, border: Color, title: &[Seg], right: &[Seg]) {
    if r.width < 2 || r.height < 2 {
        return;
    }
    let b = fg(border);
    let w = r.width as usize;
    let mut top: Vec<Seg> = vec![raw(if title.is_empty() { "┌" } else { "┌─" }, b)];
    top.extend(title.iter().cloned());
    let used: usize = top.iter().map(|(t, _)| t.width()).sum();
    let right_w: usize = right.iter().map(|(t, _)| t.width()).sum::<usize>() + if right.is_empty() { 1 } else { 2 };
    let right_fits = used + right_w < w;
    let fill = w.saturating_sub(used + if right_fits { right_w } else { 1 });
    top.push(raw("─".repeat(fill), b));
    if right_fits {
        top.extend(right.iter().cloned());
        top.push(raw(if right.is_empty() { "┐" } else { "─┐" }, b));
    } else {
        top.push(raw("┐", b));
    }
    put(buf, r.x, r.y, r.width, &top, None);
    for y in r.y + 1..r.bottom() - 1 {
        buf.set_string(r.x, y, "│", b);
        buf.set_string(r.right() - 1, y, "│", b);
    }
    put(buf, r.x, r.bottom() - 1, r.width, &[raw(format!("└{}┘", "─".repeat(w - 2)), b)], None);
}

pub fn separator(buf: &mut Buffer, r: Rect, y: u16, border: Color) {
    put(buf, r.x, y, r.width, &[raw(format!("├{}┤", "─".repeat(r.width as usize - 2)), fg(border))], None);
}

/// Key hints: the key in teal bold with its real case, the description dim.
pub fn keys(pairs: &[(&str, &str)]) -> Vec<Seg> {
    pairs.iter().flat_map(|(k, d)| [raw(format!(" {k}"), bold(TEAL)), s(format!(" {d} "), fg(DIM))]).collect()
}

/// Stretches the history over `w` columns so the graph always spans the full two minutes.
fn resample(h: &VecDeque<f32>, w: usize, out: &mut Vec<f32>) {
    out.clear();
    let missing = HIST_CAP - h.len().min(HIST_CAP);
    for c in 0..w {
        let a = c * HIST_CAP / w.max(1);
        let b = (((c + 1) * HIST_CAP).div_ceil(w.max(1))).max(a + 1).min(HIST_CAP);
        let mut v: f32 = 0.0;
        for i in a..b {
            if i >= missing {
                v = v.max(h[i - missing]);
            }
        }
        out.push(v);
    }
}

fn graph(buf: &mut Buffer, r: Rect, vals: &[f32], max: f32, color: Color) {
    let (x, y, w, h) = (r.x, r.y, r.width as usize, r.height as usize);
    for (c, &v) in vals.iter().enumerate().take(w) {
        let level = v / max * h as f32;
        for r in 0..h {
            let lvl = level - (h - 1 - r) as f32;
            let ch = if lvl >= 1.0 { BLOCKS[8] } else if lvl <= 0.0 { " " } else { BLOCKS[((lvl * 8.0).round() as usize).clamp(1, 8)] };
            buf.set_string(x + c as u16, y + r as u16, ch, fg(color));
        }
    }
}

/// Download (teal) with upload (lavender) stacked on top.
fn net_graph(buf: &mut Buffer, r: Rect, rx: &[f32], tx: &[f32], max: f32) {
    let (x, y, w, h) = (r.x, r.y, r.width as usize, r.height as usize);
    for c in 0..w.min(rx.len()) {
        let down = rx[c] / max * h as f32;
        let total = (rx[c] + tx[c]) / max * h as f32;
        for r in 0..h {
            let b = (h - 1 - r) as f32; // this cell covers [b, b + 1)
            let part = |lvl: f32| BLOCKS[((lvl * 8.0).round() as usize).clamp(1, 8)];
            let (ch, st) = if down >= b + 1.0 {
                (BLOCKS[8], fg(TEAL))
            } else if total <= b {
                (" ", Style::new())
            } else if down <= b {
                (if total - b >= 1.0 { BLOCKS[8] } else { part(total - b) }, fg(LAVENDER))
            } else if total >= b + 1.0 {
                (part(down - b), fg(TEAL).bg(LAVENDER))
            } else {
                (part(total - b), fg(TEAL))
            };
            buf.set_string(x + c as u16, y + r as u16, ch, st);
        }
    }
}

fn spark(h: &VecDeque<f32>, w: usize, max: f32) -> String {
    let skip = h.len().saturating_sub(w);
    let mut out: String = " ".repeat(w.saturating_sub(h.len()));
    for v in h.iter().skip(skip) {
        out.push_str(BLOCKS[((v / max * 8.0).round() as usize).clamp(1, 8)]);
    }
    out
}

fn metric_value(m: Metric, cpu: f32, memory: u64, rx: f64, tx: f64) -> String {
    match m {
        Metric::Cpu => pct(cpu),
        Metric::Mem => mem(memory),
        Metric::Net => format!("{}/s", rate(rx + tx)),
    }
}

fn strip(buf: &mut Buffer, r: Rect, app: &App) {
    let m = &app.col.machine;
    frame(buf, r, BORDER, &[], &[]);
    let frac = if m.mem_total > 0 { m.mem_used as f64 / m.mem_total as f64 } else { 0.0 };
    let bar = if r.width >= 90 { 16 } else { 6 };
    let filled = (frac.clamp(0.0, 1.0) * bar as f64).round() as usize;
    let gb = |b: u64| b as f64 / 1_073_741_824.0;
    let gap = if r.width >= 100 { "     " } else { "  " };
    let line = [
        s(" cpu ", bold(BRIGHT)),
        raw(spark(&m.cpu_hist, 12, 100.0), fg(GREEN)),
        s(format!(" {}", rp(&pct(m.cpu), 6)), fg(TEXT)),
        s(format!("{gap}mem "), bold(BRIGHT)),
        raw("█".repeat(filled), fg(if frac > 0.85 { AMBER } else { GREEN })),
        raw("░".repeat(bar - filled), fg(VERY_DIM)),
        s(format!(" {:.1} gb / {:.0} gb", gb(m.mem_used), gb(m.mem_total)), fg(TEXT)),
        s(format!("{gap}net "), bold(BRIGHT)),
        s(format!("↓{}/s ", rate(m.rx)), fg(TEAL)),
        s(format!("↑{}/s", rate(m.tx)), fg(LAVENDER)),
    ];
    put(buf, r.x + 1, r.y + 1, r.width - 2, &line, None);
}

fn metric_tabs(app: &App) -> Vec<Seg> {
    let mut v = vec![raw("─", fg(BORDER))];
    for m in Metric::ALL {
        let st = if m == app.metric { bold(BRIGHT).bg(TAB_BG) } else { fg(DIM) };
        v.push(s(format!(" {} ", m.name()), st));
        v.push(raw("─", fg(BORDER)));
    }
    v
}

fn list(buf: &mut Buffer, r: Rect, app: &mut App, rows: &[Row], sel: usize) {
    let border = if app.focus == Focus::List { BORDER_ON } else { BORDER };
    frame(buf, r, border, &metric_tabs(app), &[]);
    let (iw, ih) = (r.width.saturating_sub(2), r.height.saturating_sub(2) as usize);
    app.list_h = ih;
    app.list_area = (r.x + 1, r.y + 1, iw, ih as u16);
    if sel < app.off {
        app.off = sel;
    }
    if sel >= app.off + ih {
        app.off = sel + 1 - ih;
    }
    app.off = app.off.min(rows.len().saturating_sub(ih));
    let name_w = (iw as usize).saturating_sub(11);
    let snap = &app.col.snapshot;
    for (i, row) in rows.iter().enumerate().skip(app.off).take(ih) {
        let y = r.y + 1 + (i - app.off) as u16;
        let line = match &row.kind {
            RowKind::Group(g) => {
                let t = snap.totals(*g);
                let arrow = if app.open.contains(g) || !app.filter.is_empty() { "▾ " } else { "▸ " };
                [s(lp(&format!("{arrow}{}", g.name()), name_w), bold(BRIGHT)), s(rp(&metric_value(app.metric, t.cpu, t.mem, t.rx, t.tx), 10), bold(BRIGHT)), raw(" ", Style::new())]
            }
            RowKind::Item(idx) => {
                let it = &snap.items[*idx];
                let color = if app.metric == Metric::Cpu { load_color(it.cpu) } else { TEXT };
                [s(lp(&format!("    {}", it.name), name_w), fg(TEXT)), s(rp(&metric_value(app.metric, it.cpu, it.mem, it.rx, it.tx), 10), fg(color)), raw(" ", Style::new())]
            }
        };
        put(buf, r.x + 1, y, iw, &line, (i == sel).then_some(SEL_BG));
    }
    if rows.is_empty() {
        put(buf, r.x + 1, r.y + 1, iw, &[s("  nothing matches", fg(DIM))], None);
    }
}

struct Col {
    head: &'static str,
    w: usize,
}

/// Returns (visible table rows, selected row, scroll offset) for the caller to store.
fn detail(buf: &mut Buffer, r: Rect, app: &App) -> (usize, usize, usize) {
    let snap = &app.col.snapshot;
    let item = app.selected_item();
    let group = app.selected_group();
    let (name, key, cpu, memory, rx, tx, nprocs) = match (item, group) {
        (Some(it), _) => (it.name.clone(), it.key.clone(), it.cpu, it.mem, it.rx, it.tx, it.procs.len()),
        (None, Some(g)) => {
            let t = snap.totals(g);
            (g.name().to_string(), g.id(), t.cpu, t.mem, t.rx, t.tx, t.procs)
        }
        _ => (String::new(), String::new(), 0.0, 0, 0.0, 0.0, 0),
    };
    let border = if app.focus == Focus::Table { BORDER_ON } else { BORDER };
    let title = if name.is_empty() { vec![] } else { vec![s(format!(" {name} "), bold(TEAL))] };
    frame(buf, r, border, &title, &[s(format!(" {nprocs} procs "), fg(DIM))]);
    let (x, iw, ih) = (r.x + 1, r.width.saturating_sub(2), r.height.saturating_sub(2) as usize);
    let mut table_state = (app.table_h, app.tsel, app.toff);
    if ih == 0 || iw < 20 || name.is_empty() {
        return table_state;
    }
    let info_h = if ih >= 12 { 2 } else { 0 };
    let gh = if ih < 9 { 0 } else { ((ih as i32 - 20) / 3).clamp(1, 6) as usize };
    let graphs_h = if gh > 0 { 3 * (1 + gh) } else { 0 };
    let gap = usize::from(info_h > 0 && ih >= 16);
    let table_h = ih.saturating_sub(graphs_h + 1 + info_h + gap);
    let mut y = r.y + 1;

    // three stacked graphs: cpu, memory, network
    if gh > 0 {
        let gw = (iw as usize).saturating_sub(2);
        let empty = Hist::default();
        let hist = app.col.hist.get(&key).unwrap_or(&empty);
        let mut vals = Vec::with_capacity(gw);
        let mut vals2 = Vec::with_capacity(gw);

        put(buf, x, y, iw, &[s(" cpu ", bold(BRIGHT)), s(pct(cpu), fg(load_color(cpu)))], None);
        resample(&hist.cpu, gw, &mut vals);
        let max = vals.iter().copied().fold(1.0f32, f32::max);
        graph(buf, Rect::new(x + 1, y + 1, gw as u16, gh as u16), &vals, max, GREEN);
        y += 1 + gh as u16;

        put(buf, x, y, iw, &[s(" memory ", bold(BRIGHT)), s(mem(memory), fg(LAVENDER))], None);
        resample(&hist.mem, gw, &mut vals);
        let max = vals.iter().copied().fold(1.0f32, f32::max) * 1.3;
        graph(buf, Rect::new(x + 1, y + 1, gw as u16, gh as u16), &vals, max, LAVENDER);
        y += 1 + gh as u16;

        put(buf, x, y, iw, &[s(" network ", bold(BRIGHT)), s(format!("↓ {}/s", rate(rx)), fg(TEAL)), s(format!("  ↑ {}/s", rate(tx)), fg(LAVENDER))], None);
        resample(&hist.rx, gw, &mut vals);
        resample(&hist.tx, gw, &mut vals2);
        let max = vals.iter().zip(&vals2).map(|(a, b)| a + b).fold(1024.0f32, f32::max);
        net_graph(buf, Rect::new(x + 1, y + 1, gw as u16, gh as u16), &vals, &vals2, max);
        y += 1 + gh as u16;
    }
    separator(buf, r, y, border);
    y += 1;

    // the table: an item's processes, or a group's items
    let tw = (iw as usize).saturating_sub(2);
    let mut cols = vec![Col { head: "cpu", w: 8 }, Col { head: "memory", w: 10 }, Col { head: "net↓", w: 10 }];
    let width = |cols: &[Col]| cols.iter().map(|c| c.w).sum::<usize>();
    let roomy = |cols: &[Col], extra: usize| tw.saturating_sub(10 + width(cols) + extra) >= 30;
    if roomy(&cols, 9) {
        cols[2].head = "↓/s";
        cols.push(Col { head: "↑/s", w: 9 });
        if item.is_some() {
            for (c, at) in [(Col { head: "thr", w: 5 }, 0), (Col { head: "time", w: 10 }, 0), (Col { head: "user", w: 12 }, 0)] {
                if roomy(&cols, c.w) {
                    cols.insert(at, c);
                }
            }
        }
    }
    if tw.saturating_sub(9 + width(&cols)) < 18 {
        cols.pop(); // narrow terminal: the name matters more than the network column
    }
    let name_w = tw.saturating_sub(9 + width(&cols));
    let hd = bold(BRIGHT);
    let mut head = vec![s(rp(if item.is_some() { "pid" } else { "" }, 7), hd), raw("  ", hd), s(lp(if item.is_some() { "process" } else { "item" }, name_w), hd)];
    head.extend(cols.iter().map(|c| s(rp(c.head, c.w), hd)));
    if table_h > 0 {
        put(buf, x + 1, y, iw - 1, &head, None);
    }
    let body_h = table_h.saturating_sub(1);
    table_state.0 = body_h;
    let cell = |c: &Col, cpu: f32, memory: u64, rx: f64, tx: f64, p: Option<&ProcRow>| -> Seg {
        let (t, color) = match c.head {
            "cpu" => (pct(cpu), load_color(cpu)),
            "memory" => (mem(memory), TEXT),
            "net↓" | "↓/s" => (rate(rx), TEAL),
            "↑/s" => (rate(tx), LAVENDER),
            "thr" => (p.map(|p| p.threads.to_string()).unwrap_or_default(), DIM),
            "time" => (p.map(|p| ftime(p.cpu_time_ms / 1000)).unwrap_or_default(), DIM),
            _ => (p.map(|p| p.user.clone()).unwrap_or_default(), DIM),
        };
        s(rp(&t, c.w), fg(color))
    };
    let total;
    if let Some(it) = item {
        let procs = app.table_procs();
        total = procs.len();
        let focused = app.focus == Focus::Table;
        let tsel = app.tsel.min(total.saturating_sub(1));
        let mut off = app.toff;
        if focused {
            if tsel < off {
                off = tsel;
            }
            if tsel >= off + body_h && body_h > 0 {
                off = tsel + 1 - body_h;
            }
        } else {
            off = 0;
        }
        // when not everything fits, the last line says how many more there are
        let shown = if total > body_h && !focused { body_h.saturating_sub(1) } else { body_h };
        for (i, p) in procs.iter().enumerate().skip(off).take(shown) {
            let pid = if p.pid == 0 && matches!(it.kind, ItemKind::Container { .. }) { String::new() } else { p.pid.to_string() };
            let mut line = vec![s(rp(&pid, 7), fg(LAVENDER)), raw("  ", Style::new()), s(lp(&p.name, name_w), fg(if p.zombie { DIM } else { TEXT }))];
            line.extend(cols.iter().map(|c| cell(c, p.cpu, p.mem, p.rx, p.tx, Some(p))));
            put(buf, x + 1, y + 1 + (i - off) as u16, iw - 1, &line, (focused && i == tsel).then_some(SEL_BG));
        }
        if total > shown && !focused && body_h > 0 {
            put(buf, x + 1, y + body_h as u16, iw - 1, &[s(format!("         … {} more", total - shown), fg(DIM))], None);
        }
        (table_state.1, table_state.2) = (tsel, off);
    } else if let Some(g) = group {
        let mut items: Vec<&Item> = snap.group_items(g).collect();
        items.sort_by(|a, b| b.metric(app.metric).total_cmp(&a.metric(app.metric)));
        total = items.len();
        let shown = if total > body_h { body_h.saturating_sub(1) } else { body_h };
        for (i, it) in items.iter().enumerate().take(shown) {
            let mut line = vec![raw(" ".repeat(9), Style::new()), s(lp(&it.name, name_w), fg(TEXT))];
            line.extend(cols.iter().map(|c| cell(c, it.cpu, it.mem, it.rx, it.tx, None)));
            put(buf, x + 1, y + 1 + i as u16, iw - 1, &line, None);
        }
        if total > shown && body_h > 0 {
            put(buf, x + 1, y + body_h as u16, iw - 1, &[s(format!("         … {} more", total - shown), fg(DIM))], None);
        }
    }

    // path and facts at the bottom
    if info_h > 0 {
        let y = r.bottom() - 1 - info_h as u16;
        if let Some(it) = item {
            put(buf, x, y, iw, &[s("  path  ", fg(DIM)), s(&it.path, fg(TEXT))], None);
            let first = it.procs.first();
            let started = first.map(|p| p.start_time).filter(|&t| t > 0).map(|t| {
                let (_, h, m, _) = native::local_time(t);
                format!("{h:02}:{m:02}")
            });
            let user = first.map(|p| p.user.as_str()).unwrap_or("-");
            let mut line = vec![s("  user  ", fg(DIM)), s(user, fg(TEXT))];
            if let Some(st) = started {
                line.extend([s("     started ", fg(DIM)), s(st, fg(TEXT))]);
            }
            if let Some(p) = first.filter(|p| p.pid != 0) {
                line.extend([s("     pid ", fg(DIM)), s(p.pid.to_string(), fg(TEXT))]);
            }
            put(buf, x, y + 1, iw, &line, None);
        } else if let Some(g) = group {
            let n = snap.group_items(g).count();
            put(buf, x, y + 1, iw, &[s(format!("  {n} items · {nprocs} processes"), fg(DIM))], None);
        }
    }
    table_state
}

fn footer(app: &App, w: u16) -> Vec<Seg> {
    if let Some(c) = &app.confirm {
        return vec![s(format!(" {} ", c.text), fg(AMBER)), raw("y", bold(TEAL)), s(" / ", fg(DIM)), raw("n", bold(TEAL))];
    }
    if let Some(f) = crate::cleanup_view::footer(app) {
        return f;
    }
    if !app.flash.is_empty() && !app.filtering {
        return vec![s(format!(" {}", app.flash), fg(DIM))];
    }
    if app.filtering || !app.filter.is_empty() {
        return vec![
            raw(" /", bold(TEAL)),
            s(&app.filter, fg(TEXT)),
            raw(if app.filtering { "█" } else { "" }, fg(DIM)),
            s(if app.filtering { "    enter keep · esc clear" } else { "    esc clear" }, fg(DIM)),
        ];
    }
    let mut hints = if app.focus == Focus::Table {
        keys(&[("j/k", "move"), ("h", "back to list"), ("gg/G", "top/end"), ("^d/^u", "page"), ("1/2/3", "metric"), ("dd", "kill process"), ("q", "quit")])
    } else {
        keys(&[
            ("j/k", "move"),
            ("h/l", "fold"),
            ("gg/G", "top/end"),
            ("^d/^u", "page"),
            ("1/2/3", "metric"),
            ("/", "filter"),
            ("dd", "kill"),
            ("c", "clean"),
            ("q", "quit"),
        ])
    };
    // the pending prefix sits at the far right, like vim's showcmd
    let cmd = app.showcmd();
    let used: usize = hints.iter().map(|(t, _)| t.width()).sum();
    let room = (w as usize).saturating_sub(cmd.width() + 1);
    if used < room {
        hints.push(raw(" ".repeat(room - used), Style::new()));
        hints.push(raw(cmd, fg(AMBER)));
    }
    hints
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let buf = f.buffer_mut();
    if area.width < 30 || area.height < 6 {
        put(buf, area.x, area.y, area.width, &[s(" parasight needs a bigger terminal", fg(DIM))], None);
        return;
    }
    let strip_h = if area.height >= 16 { 3 } else { 0 };
    if strip_h > 0 {
        strip(buf, Rect::new(area.x, area.y, area.width, strip_h), app);
    }
    let body = Rect::new(area.x, area.y + strip_h, area.width, area.height - strip_h - 1);
    let rows = app.rows();
    let sel = app.sel_index(&rows);
    let lw = if area.width >= 84 { 36 } else if area.width >= 60 { 30 } else { area.width };
    list(buf, Rect::new(body.x, body.y, lw, body.height), app, &rows, sel);
    if lw < body.width {
        (app.table_h, app.tsel, app.toff) = detail(buf, Rect::new(body.x + lw, body.y, body.width - lw, body.height), app);
    }
    if app.clean.is_some() {
        crate::cleanup_view::draw(buf, Rect::new(area.x, area.y, area.width, area.height - 1), app);
    }
    let foot = footer(app, area.width);
    put(buf, area.x, area.bottom() - 1, area.width, &foot, None);
}

/// Renders a frame without a terminal and returns it as text (`color` adds ANSI colours).
pub fn snapshot(app: &mut App, w: u16, h: u16, color: bool) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut term = Terminal::new(TestBackend::new(w, h)).expect("test backend");
    term.draw(|f| draw(f, app)).expect("draw");
    let buf = term.backend().buffer();
    let mut out = String::new();
    for y in 0..h {
        let mut line = String::new();
        let mut skip = 0;
        for x in 0..w {
            let cell = &buf[(x, y)];
            if skip > 0 {
                skip -= 1;
                continue;
            }
            skip = cell.symbol().width().saturating_sub(1);
            if color {
                let code = |c: Color, base: u8| match c {
                    Color::Rgb(r, g, b) => format!("\x1b[{base};2;{r};{g};{b}m"),
                    _ => String::new(),
                };
                line.push_str("\x1b[0m");
                line.push_str(&code(cell.fg, 38));
                line.push_str(&code(cell.bg, 48));
                if cell.modifier.contains(Modifier::BOLD) {
                    line.push_str("\x1b[1m");
                }
            }
            line.push_str(cell.symbol());
        }
        if color {
            line.push_str("\x1b[0m");
            out.push_str(&line);
        } else {
            out.push_str(line.trim_end());
        }
        out.push('\n');
    }
    out
}
