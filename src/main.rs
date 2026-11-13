mod actions;
mod app;
mod ask;
mod classify;
mod cleanup;
mod cleanup_view;
mod collect;
mod inspect;
mod interact;
mod mcp;
mod model;
mod native;
mod procs;
mod redact;
mod native_track;
mod rules;
mod sources;
mod stream;
mod track;
mod tracker;
mod ui;
mod util;

use app::App;
use collect::Collector;
use model::{Group, Metric, TICK_MS};
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use ratatui::crossterm::execute;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const HELP: &str = concat!(
    "parasight: a terminal activity monitor for macOS\n",
    "\n",
    "usage:\n",
    "  parasight                 run the monitor\n",
    "  parasight track           record what you interact with, without the ui (for clean-up)\n",
    "  parasight track --install start `track` at login (--uninstall removes it)\n",
    "  parasight --snapshot      print one frame as text and exit\n",
    "      --width W --height H  frame size (default 100x32)\n",
    "      --view cleanup        show the clean-up popup\n",
    "      --view ask            show a canned answer where K puts its answer\n",
    "      --metric cpu|mem|net  sort and chart by this\n",
    "      --select NAME         select the first item whose name contains NAME\n",
    "      --filter TEXT         apply a filter\n",
    "      --ticks N             refreshes to gather first (default 3)\n",
    "      --color               keep colours (ansi escapes)\n",
    "  parasight --list          print groups and items as plain text\n",
    "  parasight ask NAME        ask claude code what NAME is and print the one-line answer\n",
    "  parasight mcp             read-only mcp server over stdio (what ask gives claude code)\n",
    "  --no-docker               do not look for containers\n",
    "\n",
    "keys: j/k move · h/l fold (l on an item: its processes) · gg/G top/end\n",
    "      ctrl-d/ctrl-u half page · zo zc za zR zM folds · 1/2/3 cpu/mem/net\n",
    "      / filter · dd kill (asks first) · q quit\n",
    "      c clean up: what looks safe to close, checked or not, asks before closing\n",
    "      K what is this: one line from claude code about the selected app or process\n",
);

#[derive(Default)]
struct Args {
    snapshot: bool,
    list: bool,
    width: u16,
    height: u16,
    view: String,
    metric: Option<Metric>,
    select: Option<String>,
    filter: Option<String>,
    ticks: u32,
    color: bool,
    no_docker: bool,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut a = Args { width: 100, height: 32, ticks: 3, ..Default::default() };
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--snapshot" => a.snapshot = true,
            "--list" => a.list = true,
            "--color" => a.color = true,
            "--no-docker" => a.no_docker = true,
            "--width" => a.width = value("--width")?.parse().map_err(|_| "--width needs a number")?,
            "--height" => a.height = value("--height")?.parse().map_err(|_| "--height needs a number")?,
            "--ticks" => a.ticks = value("--ticks")?.parse().map_err(|_| "--ticks needs a number")?,
            "--view" => a.view = value("--view")?,
            "--select" => a.select = Some(value("--select")?),
            "--filter" => a.filter = Some(value("--filter")?),
            "--metric" => {
                let v = value("--metric")?;
                a.metric = Some(Metric::ALL.into_iter().find(|m| m.name() == v).ok_or("--metric is cpu, mem or net")?);
            }
            other => return Err(format!("unknown option {other} (try --help)")),
        }
    }
    Ok(a)
}

fn build(
    args: &Args,
    headless: bool,
) -> App {
    let shared = sources::start(sources::Options { docker: !args.no_docker, net_interval: 2 });
    let col = Collector::new(shared.clone(), rules::Rules::load());
    // a snapshot reads the saved history but never writes it
    let tracker = tracker::Tracker::start(shared, tracker::Options { state_path: Some(interact::State::default_path()), persist: !headless });
    let mut app = App::new(
        col,
        tracker,
    );
    if let Some(m) = args.metric {
        app.metric = m;
    }
    if let Some(f) = &args.filter {
        app.filter = f.clone();
    }
    app
}

fn tick(app: &mut App) {
    app.col.tick();
    app.tracker.observe(&app.col);
}

fn headless(args: &Args) {
    let mut app = build(
        args,
        true,
    );
    tick(&mut app);
    for _ in 0..args.ticks {
        std::thread::sleep(Duration::from_millis(TICK_MS));
        tick(&mut app);
    }
    if let Some(q) = &args.select {
        let q = q.to_lowercase();
        let found = Group::ALL.into_iter().find(|g| g.name() == q).map(|g| g.id()).or_else(|| app.snap().items.iter().find(|i| i.name.to_lowercase().contains(&q)).map(|i| i.key.clone()));
        if let Some(id) = found {
            app.sel = id;
            app.moved = true;
        }
    }
    if args.list {
        for g in Group::ALL {
            let t = app.snap().totals(g);
            println!("{}  cpu {:.1}%  mem {}  ({} processes)", g.name(), t.cpu, util::mem(t.mem), t.procs);
            let mut items: Vec<_> = app.snap().group_items(g).collect();
            items.sort_by_key(|i| std::cmp::Reverse(i.mem));
            for it in items {
                println!("  {:<34} {:>6.1}% {:>9} {:>3}p  {}", util::clip(&it.name, 34), it.cpu, util::mem(it.mem), it.procs.len(), util::clip(&it.path, 90));
            }
        }
    } else {
        if args.view == "cleanup" {
            cleanup_view::open(&mut app);
        }
        if args.view == "ask" {
            ui::snapshot(&mut app, args.width, args.height, false); // settles the selection
            app.set_canned_answer("it's a web browser, with one process per tab (canned answer)");
        }
        print!("{}", ui::snapshot(&mut app, args.width, args.height, args.color));
    }
    app.col.shared.shutdown();
}

/// `parasight ask NAME`: the same question K asks, without the ui. Prints the one-line answer
/// to stdout and what happened on the way (with timings) to stderr.
fn ask_once(argv: &[String]) -> i32 {
    let Some(name) = argv.first() else {
        eprintln!("usage: parasight ask NAME   (an item name, or part of one)");
        return 2;
    };
    let args = Args { ticks: 1, no_docker: false, ..Default::default() };
    let mut app = build(&args, true);
    tick(&mut app);
    std::thread::sleep(Duration::from_millis(500));
    tick(&mut app);
    let world = inspect::World::from_collector(&app.col, None);
    let Some(key) = world.find(name).map(|i| i.key.clone()) else {
        eprintln!("parasight: nothing running matches \"{name}\"");
        app.col.shared.shutdown();
        return 1;
    };
    app.sel = key;
    app.moved = true;
    // a testing hook: another question, to check what the locked-down child can and cannot do
    let question = std::env::var("PARASIGHT_ASK_QUESTION").unwrap_or_else(|_| ask::QUESTION.to_string());
    let t0 = Instant::now();
    app.ask_selected(&question);
    let mut last = String::new();
    while app.ask.busy() {
        if app.ask.poll()
            && let Some((_, line)) = app.ask_line()
        {
            let now = format!("{line:?}");
            if now != last {
                eprintln!("[{:5.2}s] {now}", t0.elapsed().as_secs_f64());
                last = now;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let code = match app.ask_line() {
        Some((_, ask::Line::Answer(a))) => {
            println!("{a}");
            0
        }
        Some((_, ask::Line::Failed(why))) => {
            eprintln!("parasight: {why}");
            1
        }
        _ => 1,
    };
    app.col.shared.shutdown();
    code
}

fn run_tui(args: &Args) -> std::io::Result<()> {
    let quit = native::quit_flag();
    let mut app = build(
        args,
        false,
    );
    tick(&mut app);
    let mut term = ratatui::init();
    execute!(std::io::stdout(), EnableMouseCapture)?;
    // cpu percentages need two samples, so the second refresh comes early
    let mut next_tick = Instant::now() + Duration::from_millis(400);
    let mut dirty = true;
    let result = loop {
        if app.quit || quit.load(Ordering::Relaxed) {
            break Ok(());
        }
        if dirty {
            if let Err(e) = term.draw(|f| ui::draw(f, &mut app)) {
                break Err(e);
            }
            dirty = false;
        }
        // sleep until the next refresh, a key, or (while something is closing) a short while
        let wait = next_tick.saturating_duration_since(Instant::now());
        let wait = if app.waiting() { wait.min(Duration::from_millis(80)) } else { wait };
        match event::poll(wait) {
            Ok(true) => {
                // take what is waiting (a burst of keys, a paste) before drawing once; bounded,
                // and never past a quit signal: a terminal that went away reads as endless input
                let mut budget = 256;
                while budget > 0 && !quit.load(Ordering::Relaxed) && matches!(event::poll(Duration::ZERO), Ok(true)) {
                    budget -= 1;
                    match event::read() {
                        Ok(Event::Key(k)) if k.kind != KeyEventKind::Release => app.on_key(k),
                        Ok(Event::Mouse(m)) => app.on_mouse(m),
                        Ok(_) => {}
                        Err(_) => {
                            app.quit = true;
                            break;
                        }
                    }
                    dirty = true;
                }
            }
            Ok(false) => {}
            Err(e) => break Err(e),
        }
        // while a question is out, keep redrawing so the loader turns
        if app.poll_actions() | app.ask.poll() | app.ask.busy() {
            dirty = true;
        }
        if Instant::now() >= next_tick {
            tick(&mut app);
            next_tick = Instant::now() + Duration::from_millis(TICK_MS);
            dirty = true;
        }
        if app.quit || quit.load(Ordering::Relaxed) {
            break Ok(());
        }
    };
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    app.ask.cancel(); // stops the claude child, if one is still answering
    app.tracker.save();
    app.col.shared.shutdown();
    result
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.iter().any(|a| a == "--help" || a == "-h") {
        print!("{HELP}");
        return;
    }
    if argv.iter().any(|a| a == "--version" || a == "-V") {
        println!("parasight {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if argv.first().map(String::as_str) == Some("track") {
        std::process::exit(track::run(&argv[1..]));
    }
    if argv.first().map(String::as_str) == Some("mcp") {
        std::process::exit(mcp::serve());
    }
    if argv.first().map(String::as_str) == Some("ask") {
        std::process::exit(ask_once(&argv[1..]));
    }
    let args = match parse_args(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("parasight: {e}");
            std::process::exit(2);
        }
    };
    if args.snapshot || args.list {
        headless(&args);
        return;
    }
    if let Err(e) = run_tui(&args) {
        eprintln!("parasight: {e}");
        std::process::exit(1);
    }
}
