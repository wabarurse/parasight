# parasight clean-up: technical design

Status: built. Where the build differs from this design, the "As built" section at the end wins.

## Goal

Press `c` to get a short, ranked list of things that are probably safe to
close, each with a plain reason and the memory it would free. Two kinds:

1. **Hanging dev processes**: left behind by a closed terminal or editor, or
   idle for a long time.
2. **Idle apps**: GUI apps not used for a while, with apps that are not pinned
   in the Dock ranked first.

## Non-goals

- No automatic or scheduled killing. Every action needs a confirmation.
- No force kill (`SIGKILL`) by default.
- Nothing in the `system` group is ever offered.
- No cleaning of files, caches or disk space.

## Flow

```
gather (impure)                score (pure)                 act (impure)
sysinfo, nettop, lsof,   →   Vec<Facts> → Vec<Candidate>  →  confirm → quit / SIGTERM
lsappinfo, dock prefs         ranked, with reasons            → report result
```

Scoring is a pure function over plain data, so it can be unit-tested with
fixtures and never touches the machine.

## Inputs (`Facts`, one per item)

| Field | Source | Notes |
|---|---|---|
| group, name, pids, memory | existing process model | |
| root ppid, launchd-managed? | sysinfo, `launchctl` label | ppid 1 and not a launchd job = orphan |
| has controlling tty | process info | no tty + ppid 1 strengthens "orphan" |
| age | process start time | |
| cpu idle for | parasight history, plus lifetime CPU time ÷ age | second part works before parasight has history |
| net idle for | nettop byte deltas | |
| listening port, cwd | lsof / libproc | used to spot duplicates |
| last interacted | see "Last interacted" below | the main signal for staging |
| last opened | Spotlight `kMDItemLastUsedDate` | weak fallback: launch date, not last use |
| pinned in Dock | `defaults read com.apple.dock persistent-apps` | |
| is frontmost, is self/ancestor | lsappinfo, pid walk | exclusions |

## Last interacted

"Used" means the user gave the item input, not that its window was in front.
An app left frontmost overnight is not being used.

**Apps.** Every second, a sampler reads two things, neither of which needs a
permission prompt:

- the frontmost app (`lsappinfo front`, or NSWorkspace)
- seconds since the last keyboard, mouse-click or scroll event, system-wide
  (`CGEventSourceSecondsSinceLastEventType`; mouse movement alone is ignored)

If the last input event is newer than the previous sample, that input went to
the frontmost app, so `last_interacted[app] = now`. Focus without input does
not count.

**Dev processes.** There is no window, so interaction means:

- keyboard input to its terminal: the access time of its tty device
  (`stat /dev/ttysNNN`) is the last time the user typed into that terminal
- being used over the network: last time it sent or received bytes (a dev
  server answering a browser request counts as used)

`last_interacted` is the newer of the two. CPU activity alone is not
interaction: a watcher spinning by itself is not being used.

**Keeping history.** Timestamps are saved to
`~/.local/state/parasight/last-interacted.json` so they survive restarts.
They are only collected while a sampler runs, so there are two modes:

- default: sampling happens while parasight is open
- opt-in: `parasight track` runs the sampler alone, with no UI, and can be
  installed as a login item (`parasight track --install`). With this, clean-up
  has real history the moment it is opened. Never installed automatically.

If an app has no recorded interaction, the reason says so ("no input seen
since tracking began, 45m ago") and confidence is capped at medium unless the
Spotlight last-opened date is also old.

## Staging

Pressing `c` computes candidates at that moment and **stages** the ones that
qualify: staged items are ticked and will be closed on confirm. Everything
else that matched a rule is listed below them, unstaged. Staging is decided
by last interacted; the Dock pin and memory only affect order.

## Rules

Each rule that matches adds a reason and a confidence. An item takes its
highest confidence.

**Dev**

| Rule | Condition | Confidence |
|---|---|---|
| orphaned and unused | ppid 1, not launchd-managed, no interaction for 10 min | high (staged) |
| orphaned | ppid 1, not launchd-managed, still active | medium |
| duplicate server | same cwd and command as a newer item | high (the older one) |
| zombie / stuck | zombie state, or uninterruptible for 5 min | high |
| unused server or watcher | has a parent, no interaction for 2 h | medium |
| idle background service | launchd-managed (postgres, redis), idle 24 h | low |
| idle container | no CPU/net for 6 h | low |

**Apps**

| Rule | Condition | Confidence |
|---|---|---|
| unused, not in Dock | no interaction for 24 h (or last opened 3 d+) and not pinned | high (staged) |
| unused, not in Dock | no interaction for 2 h and not pinned | medium |
| unused, pinned | no interaction for 24 h and pinned | medium |
| unused, pinned | no interaction for 2 h and pinned | low |
| background-only app | menu-bar or agent app (no Dock icon) | capped at low |

**Never offered**: parasight, its parent shell and terminal app, the frontmost
app, Finder, Dock, loginwindow, anything in `system`, and anything the user
has listed under `keep` in `~/.config/parasight/rules.toml`.

## Ranking and preselection

Sort by confidence (high → low), then not-pinned before pinned, then memory
freed (largest first). Only **high** items are staged (ticked).

## Actions

| Target | Action | Why |
|---|---|---|
| GUI app | ask it to quit (Apple event) | the app can still prompt to save work |
| dev process tree | `SIGTERM` to the root, then children still alive | lets servers shut down cleanly |
| container | `docker stop` | same as the user would do |

After acting, wait up to 5 s and report per item: closed, still running, or
refused. A still-running item gets a "force kill?" follow-up; never automatic.

## UI

Centred popup over the dimmed main view. Two groups only:

- **recommended**: the high-confidence items, checked for you
- **to review**: everything else that matched a rule (medium and low),
  unchecked

Each row has a checkbox, name, group, reason, and three columns for what
closing it would save: cpu (current %), memory, and network (current rate).
The user can check and uncheck any row in either group (`space`, `x` or a
click; `a` toggles all). A totals line at the bottom sums cpu, memory and
network for whatever is checked, and updates live. The confirmation repeats
those totals.

```
┌─ clean up ──────────────────────────────────────────────── 4 of 10 checked ─┐
│                                                     cpu    memory   network │
│ recommended · checked for you                                               │
│ [x] next dev :3000   dev   terminal closed · no input for 3h   5.6%   1016 mb  218 kb/s │
│ [x] spotify          apps  no input for 2d · not in dock       1.6%    388 mb  257 kb/s │
│                                                                             │
│ to review · not checked                                                     │
│ [ ] docker desktop   apps  no input for 26h · pinned           4.0%   2.49 gb  115 kb/s │
│ [ ] postgres :5432   dev   background service · unused for 1d  0.6%    217 mb   23 kb/s │
├─────────────────────────────────────────────────────────────────────────────┤
│ saves with 4 checked                                   8.8%   1.61 gb  526 kb/s │
└─────────────────────────────────────────────────────────────────────────────┘
 j/k move  space check / uncheck  a all  enter clean  esc close
```

The interactive version is section B of `mockup-pick.html` (variant
'Clean-up'). The reason always names the signal used, e.g. "no input for 2d",
"no input seen since tracking began (45m)" or "last opened 3d ago".

## Tests

Pure scoring tests with fixture `Facts`:

1. Orphaned dev server, idle 3 h → high, ticked, reason mentions both.
2. Same server with a live parent terminal, idle 30 min → not listed.
3. Same server with a live parent, idle 3 h → medium, not ticked.
4. launchd-managed postgres, ppid 1 → not treated as orphan; low only after 24 h idle.
5. Two `vite` in the same cwd → only the older one is listed, as duplicate.
6. Unpinned app idle 2 d and pinned app idle 2 d, same memory → unpinned ranks first; unpinned is high, pinned is medium.
7. Frontmost app, however idle → never listed.
8. parasight, its shell and its terminal app → never listed.
9. Any `system` item → never listed.
10. No interaction history yet and no Spotlight date → app is not listed (no guess).
10a. App frontmost for 30 h with no input → treated as unused for 30 h (focus alone does not count).
10b. App in the background that received input 5 min ago (was frontmost then) → not listed.
10c. Dev server with a live terminal, no tty input for 5 h, but network traffic 1 min ago → not listed.
10d. Watcher using CPU, no tty input and no network for 3 h, parent alive → medium (CPU alone is not interaction).
10e. Sampler: input event newer than the previous sample stamps the frontmost app only; mouse movement alone stamps nothing.
10f. Timestamps round-trip through the state file; a missing or corrupt file means "no history", not a crash.
11. Ordering: confidence, then pin state, then memory.
12. Item in the `keep` list → never listed.
13. Totals (cpu, memory, network) equal the sums over checked items and change when a row is checked or unchecked.
13a. Grouping: high → recommended and checked; medium and low → to review and unchecked.

Integration tests (real processes the test spawns itself, never the user's):

14. Spawn an orphaned `sleep` → shows as a candidate; confirm → it exits.
15. Spawn a process that ignores `SIGTERM` → reported as "still running", not force-killed.

## Risks and open questions

- **No history before tracking starts.** macOS does not record per-app
  interaction, so parasight has to sample it. Without `parasight track`
  installed, history only covers the time parasight was open; until then we
  fall back to Spotlight's last-opened date and say so in the reason.
- **Input is credited to the frontmost app.** Scrolling a background window
  or using a global hotkey is credited to whatever is in front. Rare, and it
  errs towards keeping apps open.
- **Passive use is not interaction.** Music playing or a video call with no
  typing looks unused. Apps holding a "prevent sleep" or audio assertion
  (`pmset -g assertions`) are capped at low so they are never staged.
- **tty access time** is shared by everything in that terminal tab, and some
  terminal multiplexers update it differently; needs checking under tmux.
- **Orphan false positives.** Things started on purpose with `nohup` or
  `disown` look orphaned. They are listed with the reason shown and can be
  added to `keep`.
- **Idle is not the same as unneeded.** A dev server waiting for a request is
  idle by design, which is why "idle with a live parent" is only medium and
  needs 2 h.
- **Containers are counted twice in memory** (also inside Docker Desktop's
  VM), so "frees" for a container is an estimate.
- **Unsaved work.** Quit is polite, so apps can prompt. An app that blocks on
  a save dialog is reported as "still running".

## As built

Checked on a real machine during the build; these replace the matching
points above.

**Signals**

- `lsappinfo` exposes no last-activation time, so the input sampler is the
  only real "last interacted" signal for apps.
- Spotlight's last-used date is not only the launch date, and it is unclear
  what updates it. It is shown in the reason but never enough to recommend
  an app: that needs 2 h of tracking with no input, and the app not pinned.
- `nettop` only counts open sockets, so "never used the network" cannot be
  inferred. An orphan must be watched for 10 min before it is recommended.
- tty access time updates on input, not output, and per pane under tmux. It
  is ignored for orphans (the device name may have been reused) and only
  counts if newer than the process.
- A dev tool started by an app inherits that app's input stamp.
- A tracking gap over 3 min while the Mac stays on restarts history.
- The "has controlling tty" and "lifetime CPU ÷ age" inputs are not used.

**Rules**

- Zombies are low, not high: a signal cannot remove one and it holds no
  memory.
- "Uninterruptible for 5 min" is not implemented; macOS does not expose it.
- An orphan using 5% CPU or more is capped at "to review".
- Daemons by design (tmux, ollama, limactl, an ssh mux, bazel and gradle
  daemons) are treated like launchd services.
- "Duplicate" requires both items to listen on a port. Without that, two
  workers of one program were wrongly flagged.

**Actions and UI**

- Force kill is offered for dev processes only, never for apps.
- Containers are stopped with the docker CLI; listing and stats use the
  socket.
- `r` rescans. The last line of the popup spells out the selected row's
  reason when the column cut it.

**Not yet exercised on real targets**: quitting a GUI app, stopping a
container, the force-kill follow-up, and `parasight track --install`.
