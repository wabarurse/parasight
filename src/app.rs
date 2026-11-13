//! Ui state and key handling. The keys follow vim; the README lists them.

use crate::actions::{self, Outcome, Target, TargetKind};
use crate::ask::{self, Ask, Line};
use crate::cleanup_view::{self, CleanView};
use crate::collect::Collector;
use crate::model::*;
use crate::tracker::Tracker;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use std::collections::HashSet;
use std::sync::mpsc::{Receiver, Sender, channel};

#[derive(Clone, Debug, PartialEq)]
pub enum RowKind {
    Group(Group),
    /// Index into the snapshot's items.
    Item(usize),
}

#[derive(Clone, Debug)]
pub struct Row {
    pub id: String,
    pub kind: RowKind,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Focus {
    List,
    Table,
}

pub struct Confirm {
    pub text: String,
    pub targets: Vec<(String, Target)>,
    pub force: bool,
}

pub struct ActionResult {
    pub key: String,
    pub target: Target,
    pub outcome: Outcome,
    pub forced: bool,
}

pub struct App {
    pub col: Collector,
    pub tracker: Tracker,
    pub metric: Metric,
    pub open: HashSet<Group>,
    pub sel: String,
    pub off: usize,
    pub focus: Focus,
    pub tsel: usize,
    pub toff: usize,
    pub filter: String,
    pub filtering: bool,
    pub pending: Option<char>,
    pub flash: String,
    pub confirm: Option<Confirm>,
    pub clean: Option<CleanView>,
    pub ask: Ask,
    /// item key -> the ask cache key of what was last asked about it (the item, or one process)
    ask_keys: std::collections::HashMap<String, (String, String)>,
    /// True from `K` until the next key: the bottom bar shows the answer meanwhile.
    pub ask_shown: bool,
    pub quit: bool,
    /// Until the first key the selection rides on the top item, whatever that becomes.
    pub moved: bool,
    /// Set by the last draw: visible rows of the list and of the process table, and where they are.
    pub list_h: usize,
    pub table_h: usize,
    pub list_area: (u16, u16, u16, u16),
    in_flight: usize,
    batch: Vec<ActionResult>,
    tx: Sender<ActionResult>,
    rx: Receiver<ActionResult>,
}

impl App {
    pub fn new(
        col: Collector,
        tracker: Tracker,
    ) -> App {
        let (tx, rx) = channel();
        let flash = col.rules.warning.clone().unwrap_or_default();
        App {
            col,
            tracker,
            metric: Metric::Cpu,
            // system starts folded: it is mostly noise. zR or l opens it.
            open: Group::ALL.into_iter().filter(|g| *g != Group::System).collect(),
            sel: String::new(),
            off: 0,
            focus: Focus::List,
            tsel: 0,
            toff: 0,
            filter: String::new(),
            filtering: false,
            pending: None,
            flash,
            confirm: None,
            clean: None,
            ask: Ask::default(),
            ask_keys: std::collections::HashMap::new(),
            ask_shown: false,
            quit: false,
            moved: false,
            list_h: 20,
            table_h: 10,
            list_area: (0, 0, 0, 0),
            in_flight: 0,
            batch: Vec::new(),
            tx,
            rx,
        }
    }

    pub fn snap(&self) -> &Snapshot {
        &self.col.snapshot
    }

    /// The visible tree: three groups, each a flat list sorted by the active metric.
    pub fn rows(&self) -> Vec<Row> {
        let q = self.filter.to_lowercase();
        let items = &self.snap().items;
        let mut rows = Vec::new();
        for g in Group::ALL {
            let mut idx: Vec<usize> = (0..items.len()).filter(|&i| items[i].group == g && items[i].matches(&q)).collect();
            if !q.is_empty() && idx.is_empty() {
                continue;
            }
            rows.push(Row { id: g.id(), kind: RowKind::Group(g) });
            // a filter looks inside folded groups too, so a system process can still be found
            if q.is_empty() && !self.open.contains(&g) {
                continue;
            }
            idx.sort_by(|&a, &b| items[b].metric(self.metric).total_cmp(&items[a].metric(self.metric)).then_with(|| items[a].name.cmp(&items[b].name)));
            rows.extend(idx.into_iter().map(|i| Row { id: items[i].key.clone(), kind: RowKind::Item(i) }));
        }
        rows
    }

    /// Index of the selected row; fixes the selection when its row has gone.
    pub fn sel_index(&mut self, rows: &[Row]) -> usize {
        if rows.is_empty() {
            return 0;
        }
        if let Some(i) = rows.iter().position(|r| r.id == self.sel).filter(|_| self.moved) {
            return i;
        }
        // at first: land on the first item rather than the group header
        let i = if !self.moved { rows.iter().position(|r| matches!(r.kind, RowKind::Item(_))).unwrap_or(0) } else { self.off.min(rows.len() - 1) };
        self.sel = rows[i].id.clone();
        self.tsel = 0;
        self.toff = 0;
        i
    }

    pub fn selected_item(&self) -> Option<&Item> {
        self.snap().item(&self.sel)
    }

    pub fn selected_group(&self) -> Option<Group> {
        Group::ALL.into_iter().find(|g| g.id() == self.sel)
    }

    /// The processes of the selected item, sorted by the active metric (what the table shows).
    pub fn table_procs(&self) -> Vec<&ProcRow> {
        let Some(it) = self.selected_item() else { return Vec::new() };
        let mut v: Vec<&ProcRow> = it.procs.iter().collect();
        let k = |p: &ProcRow| match self.metric {
            Metric::Cpu => p.cpu as f64,
            Metric::Mem => p.mem as f64,
            Metric::Net => p.rx + p.tx,
        };
        v.sort_by(|a, b| k(b).total_cmp(&k(a)).then(a.pid.cmp(&b.pid)));
        v
    }

    fn select(&mut self, rows: &[Row], i: usize) {
        if let Some(r) = rows.get(i) {
            if r.id != self.sel {
                self.tsel = 0;
                self.toff = 0;
            }
            self.sel = r.id.clone();
        }
    }

    fn move_by(&mut self, delta: isize) {
        if self.focus == Focus::Table {
            let n = self.table_procs().len();
            if n > 0 {
                self.tsel = (self.tsel as isize + delta).clamp(0, n as isize - 1) as usize;
            }
            return;
        }
        let rows = self.rows();
        if rows.is_empty() {
            return;
        }
        let i = self.sel_index(&rows) as isize;
        self.select(&rows, (i + delta).clamp(0, rows.len() as isize - 1) as usize);
    }

    fn fold(&mut self, open: Option<bool>) {
        let rows = self.rows();
        let i = self.sel_index(&rows);
        match rows.get(i).map(|r| r.kind.clone()) {
            Some(RowKind::Group(g)) => {
                let now_open = self.open.contains(&g);
                if open.unwrap_or(!now_open) {
                    self.open.insert(g);
                } else {
                    self.open.remove(&g);
                }
            }
            Some(RowKind::Item(idx)) => match open {
                // l / zo on an item steps into its process table
                Some(true) | None => {
                    self.focus = Focus::Table;
                    self.tsel = 0;
                    self.toff = 0;
                }
                // h on an item jumps to its group
                Some(false) => self.sel = self.snap().items[idx].group.id(),
            },
            None => {}
        }
    }

    fn start_actions(&mut self, targets: Vec<(String, Target)>, force: bool) {
        for (key, target) in targets {
            self.in_flight += 1;
            cleanup_view::on_start(self, &key);
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let outcome = if force { actions::force(&target) } else { actions::close(&target) };
                let _ = tx.send(ActionResult { key, target, outcome, forced: force });
            });
        }
    }

    /// Collects finished actions; returns true when something changed.
    pub fn poll_actions(&mut self) -> bool {
        let mut changed = false;
        while let Ok(r) = self.rx.try_recv() {
            changed = true;
            self.in_flight = self.in_flight.saturating_sub(1);
            cleanup_view::on_result(self, &r);
            self.batch.push(r);
        }
        if changed && self.in_flight == 0 {
            self.finish_batch();
        }
        changed
    }

    fn finish_batch(&mut self) {
        let batch = std::mem::take(&mut self.batch);
        let closed: Vec<&ActionResult> = batch.iter().filter(|r| r.outcome == Outcome::Closed).collect();
        let still: Vec<&ActionResult> = batch.iter().filter(|r| r.outcome == Outcome::StillRunning).collect();
        let refused: Vec<&ActionResult> = batch.iter().filter(|r| matches!(r.outcome, Outcome::Refused(_))).collect();
        let names = |v: &[&ActionResult]| v.iter().map(|r| r.target.name.as_str()).collect::<Vec<_>>().join(", ");
        let mut parts = Vec::new();
        let closed_msg = (!closed.is_empty()).then(|| format!("closed {}", names(&closed)));
        let closed_keys: Vec<&str> = closed.iter().map(|r| r.key.as_str()).collect();
        let closed_msg = cleanup_view::on_closed(self, &closed_keys).or(closed_msg);
        parts.extend(closed_msg);
        if !refused.is_empty() {
            let why = match &refused[0].outcome {
                Outcome::Refused(w) => w.clone(),
                _ => String::new(),
            };
            parts.push(format!("refused: {} ({why})", names(&refused)));
        }
        // apps are not offered a force kill: one that is still open is probably asking to save
        let (apps, others): (Vec<&ActionResult>, Vec<&ActionResult>) = still.iter().partition(|r| matches!(r.target.kind, TargetKind::App { .. }));
        if !apps.is_empty() {
            parts.push(format!("still running: {} (it may be asking to save)", names(&apps)));
        }
        if !others.is_empty() {
            if batch.iter().any(|r| r.forced) {
                parts.push(format!("still running after force kill: {}", names(&others)));
            } else {
                let mut text = parts.join(" · ");
                if !text.is_empty() {
                    text.push_str(" · ");
                }
                text.push_str(&format!("still running: {} · force kill (sigkill)?", names(&others)));
                self.confirm = Some(Confirm { text, targets: others.iter().map(|r| (r.key.clone(), r.target.clone())).collect(), force: true });
                return;
            }
        }
        self.flash = parts.join(" · ");
    }

    fn dd(&mut self) {
        let me = std::process::id();
        let target = if self.focus == Focus::Table {
            let it_key = self.sel.clone();
            let procs = self.table_procs();
            procs.get(self.tsel).filter(|p| p.pid != 0).map(|p| (it_key, Target { name: format!("{} (pid {})", crate::util::clip(&p.name, 30), p.pid), kind: TargetKind::Tree, pids: vec![(p.pid, p.start_time)] }))
        } else if self.selected_item().is_some() {
            self.col.target(&self.sel).map(|t| (self.sel.clone(), t))
        } else {
            None
        };
        let Some((key, target)) = target else {
            self.flash = "dd kills an app or process; move onto one first".into();
            return;
        };
        if target.pids.iter().any(|&(p, _)| p == me) {
            self.flash = "that is parasight itself (or contains it); press q to quit".into();
            return;
        }
        let n = target.pids.len();
        let what = match (&target.kind, n) {
            (TargetKind::App { .. }, _) => format!("quit {}", target.name),
            (TargetKind::Container { .. }, _) => format!("stop container {}", target.name),
            (_, 1) => format!("kill {}", target.name),
            _ => format!("kill {} ({n} processes)", target.name),
        };
        self.confirm = Some(Confirm { text: format!("{what}?"), targets: vec![(key, target)], force: false });
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        self.moved = true;
        self.ask_shown = false;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if let Some(c) = self.confirm.take() {
            if key.code == KeyCode::Char('y') {
                self.flash = if c.force { "force killing…".into() } else { "closing…".into() };
                self.start_actions(c.targets, c.force);
            } else {
                self.flash.clear();
            }
            return;
        }
        if self.clean.is_some() {
            cleanup_view::on_key(self, key);
            return;
        }
        if self.filtering {
            match key.code {
                KeyCode::Enter => self.filtering = false,
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Char(c) if !ctrl => self.filter.push(c),
                _ => {}
            }
            return;
        }
        if ctrl {
            let half = (if self.focus == Focus::Table { self.table_h } else { self.list_h } / 2).max(1) as isize;
            match key.code {
                KeyCode::Char('d') => self.move_by(half),
                KeyCode::Char('u') => self.move_by(-half),
                _ => {}
            }
            return;
        }
        let pending = self.pending.take();
        self.flash.clear();
        if let Some(p) = pending {
            if let KeyCode::Char(c) = key.code {
                match (p, c) {
                    ('g', 'g') => self.move_by(-100_000),
                    ('z', 'o') => self.fold(Some(true)),
                    ('z', 'c') => self.fold_close(),
                    ('z', 'a') => self.fold(None),
                    ('z', 'R') => self.open = Group::ALL.into_iter().collect(),
                    ('z', 'M') => {
                        // fold to the three groups; the selection moves up to its group
                        if let Some(g) = self.selected_item().map(|i| i.group) {
                            self.sel = g.id();
                        }
                        self.focus = Focus::List;
                        self.open.clear();
                    }
                    ('d', 'd') => self.dd(),
                    _ => {}
                }
            }
            return;
        }
        match key.code {
            KeyCode::Char(c @ ('g' | 'z' | 'd')) => self.pending = Some(c),
            KeyCode::Char('G') | KeyCode::End => self.move_by(100_000),
            KeyCode::Home => self.move_by(-100_000),
            KeyCode::Char('/') => {
                self.filtering = true;
                self.filter.clear();
                self.focus = Focus::List;
            }
            KeyCode::Esc => {
                if self.focus == Focus::Table {
                    self.focus = Focus::List;
                } else {
                    self.filter.clear();
                }
            }
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('c') => cleanup_view::open(self),
            KeyCode::Char('K') => self.ask_selected(ask::QUESTION),
            KeyCode::Char('1') => self.metric = Metric::Cpu,
            KeyCode::Char('2') => self.metric = Metric::Mem,
            KeyCode::Char('3') => self.metric = Metric::Net,
            KeyCode::Char('j') | KeyCode::Down => self.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_by(-1),
            KeyCode::PageDown => self.move_by(self.list_h.max(2) as isize - 1),
            KeyCode::PageUp => self.move_by(1 - self.list_h.max(2) as isize),
            KeyCode::Char('l') | KeyCode::Right => {
                if self.focus == Focus::List {
                    self.fold(Some(true))
                }
            }
            KeyCode::Char('h') | KeyCode::Left => self.fold_close(),
            KeyCode::Enter => {
                if self.focus == Focus::List {
                    self.fold(None)
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                if self.focus == Focus::Table {
                    self.focus = Focus::List;
                } else if self.selected_item().is_some() {
                    self.fold(Some(true));
                }
            }
            _ => {}
        }
    }

    /// h / zc: leave the process table, close an open group, or jump from an item to its group.
    fn fold_close(&mut self) {
        if self.focus == Focus::Table {
            self.focus = Focus::List;
        } else {
            self.fold(Some(false));
        }
    }

    pub fn on_mouse(&mut self, m: MouseEvent) {
        self.moved = true;
        if self.confirm.is_some() || self.filtering {
            return;
        }
        if self.clean.is_some() {
            cleanup_view::on_mouse(self, m);
            return;
        }
        let (x, y, w, h) = self.list_area;
        let inside = m.column >= x && m.column < x + w && m.row >= y && m.row < y + h;
        match m.kind {
            MouseEventKind::ScrollDown => self.move_by(1),
            MouseEventKind::ScrollUp => self.move_by(-1),
            MouseEventKind::Down(MouseButton::Left) if m.row + 1 == y && m.column >= x && m.column < x + w => {
                // the metric switch sits in the list's top border: "── cpu ─ mem ─ net ─"
                let off = (m.column - x) as usize;
                if (2..17).contains(&off) {
                    self.metric = Metric::ALL[((off - 2) / 6).min(2)];
                }
            }
            MouseEventKind::Down(MouseButton::Left) if inside => {
                let rows = self.rows();
                let i = self.off + (m.row - y) as usize;
                if i < rows.len() {
                    self.focus = Focus::List;
                    if rows[i].id == self.sel && matches!(rows[i].kind, RowKind::Group(_)) {
                        self.fold(None);
                    } else {
                        self.select(&rows, i);
                    }
                }
            }
            _ => {}
        }
    }

    /// `K`: ask Claude Code what the selected item (or, in the process table, the selected
    /// process) is. Nothing is sent before this; what is sent has been through the redactor.
    pub fn ask_selected(&mut self, question: &str) {
        let Some(it) = self.selected_item() else {
            self.flash = "move onto an app or process first to ask what it is".into();
            return;
        };
        let world = crate::inspect::World::from_collector(&self.col, Some(self.tracker.state()));
        let project = crate::inspect::project_name(&world.cwd_of(it), &world.home);
        let pid = (self.focus == Focus::Table).then(|| self.table_procs().get(self.tsel).map(|p| p.pid)).flatten().filter(|&p| p != 0);
        let main = it.procs.first().and_then(|r| self.col.procs().get(&r.pid));
        // a key that survives restarts of the same thing: what it is, not which pid it has
        let (cache_key, label, details) = match pid.and_then(|pid| Some((pid, world.process_details(it, pid, project.clone())?))) {
            Some((pid, details)) => {
                let p = self.col.procs().get(&pid);
                let key = format!("proc\0{}\0{}", p.map(|p| p.exe.as_str()).unwrap_or(""), p.map(|p| p.cmd.join(" ")).unwrap_or_default());
                let name = self.table_procs().get(self.tsel).map(|r| crate::util::clip(&r.name, 24)).unwrap_or_default();
                (key, format!("{name}: "), details)
            }
            None => {
                let key = match &it.kind {
                    ItemKind::Dev { .. } => format!("dev\0{}\0{}\0{}", main.map(|p| p.exe.as_str()).unwrap_or(""), main.map(|p| p.cmd.join(" ")).unwrap_or_default(), main.map(|p| p.cwd.as_str()).unwrap_or("")),
                    ItemKind::App { bundle } => format!("app\0{bundle}"),
                    ItemKind::Container { .. } | ItemKind::System => format!("{}\0{}\0{}", it.group.name(), it.name, it.path),
                };
                (key, String::new(), world.item_details(it, project))
            }
        };
        let item_key = it.key.clone();
        self.ask.ask(&cache_key, ask::prompt_for(question, &details));
        self.ask_keys.insert(item_key, (cache_key, label));
        self.ask_shown = true;
    }

    /// The answer for the selected item, if it has been asked about. The bottom bar shows it.
    pub fn ask_line(&self) -> Option<(String, &Line)> {
        let (cache_key, label) = self.ask_keys.get(&self.sel)?;
        self.ask.line(cache_key).map(|l| (label.clone(), l))
    }

    /// For snapshots: show a ready-made answer on the selected item without asking anyone.
    pub fn set_canned_answer(&mut self, text: &str) {
        self.ask.set("canned", Line::Answer(text.to_string()));
        self.ask_keys.insert(self.sel.clone(), ("canned".into(), String::new()));
        self.ask_shown = true;
    }

    /// The pending multi-key prefix, shown at the far right like vim's showcmd.
    pub fn showcmd(&self) -> String {
        self.pending.map(String::from).unwrap_or_default()
    }

    /// True while a close or kill is still being carried out.
    pub fn busy(&self) -> bool {
        self.in_flight > 0
    }

    /// True while something in the background will want a redraw soon.
    pub fn waiting(&self) -> bool {
        self.in_flight > 0 || self.ask.busy()
    }

    pub fn start(&mut self, targets: Vec<(String, Target)>) {
        self.start_actions(targets, false);
    }
}
