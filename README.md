# parasight

A terminal Activity Monitor for macOS. It shows apps rather than raw processes: helper
processes roll up under the app, dev server, or system service they belong to, in three
groups (`apps`, `dev`, `system`), with cpu, memory and per-process network.

```
┌─ parasight ────────────────────────────────────────────────────────────────────────────── 14:25 ─┐
│ cpu         ▁▂▂▂  26.5%     mem ███████████░░░░░ 71%  33.9/48g     net ↓261 kb/s ↑441 kb/s       │
└──────────────────────────────────────────────────────────────────────────────────────────────────┘
┌── cpu ─ mem ─ net ───────────────┐┌─ next dev  :3000 ────────────────────────────────── 3 procs ─┐
│▾ apps                      20.0% ││ cpu 4.2%                                                     │
│    google chrome           12.9% ││ ▂▃▂▂▃▅▃▂▂▃▂▂▂▃▃▂▂▂▃▂▂▃▂▂▃▅▃▂▂▃▂▂▂▃▃▂▂▂▃▂▂▃▂▂▃▅▃▂▂▃▂▂▂▃▃▂▂▂▃▂ │
│    slack                    1.2% ││ memory 1010 mb                                               │
│▾ dev                       11.5% ││ ▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅ │
│    next dev  :3000          4.2% ││ network ↓ 40 kb/s  ↑ 300 kb/s                                │
│    postgres  :5432          0.4% │├──────────────────────────────────────────────────────────────┤
│▾ system                     9.8% ││     pid  process                     cpu    memory      net↓ │
│    windowserver             4.8% ││   41203  node next-server           4.2%    620 mb     40 kb │
└──────────────────────────────────┘└──────────────────────────────────────────────────────────────┘
 j/k move  h/l fold  gg/G top/end  ^d/^u page  1/2/3 metric  / filter  dd kill  q quit
```

## Install and run

Needs macOS and a Rust toolchain.

```sh
cargo install --path .     # or: cargo build --release && ./target/release/parasight
parasight
```

Other ways to run it:

| Command | What it does |
|---|---|
| `parasight --snapshot [--width W --height H]` | Gathers data for a few seconds, prints one frame as text and exits. Add `--select NAME`, `--metric mem`, `--filter TEXT` or `--color`. |
| `parasight --list` | Prints the groups and items as plain text. |
| `parasight --no-docker` | Does not look for containers. |

## Keys

| Key | Action |
|---|---|
| `j` / `k`, arrows | Move |
| `h` / `l` | Fold / unfold a group. On an item, `l` steps into its process table and `h` jumps to its group |
| `gg` / `G` | Top / bottom |
| `ctrl-d` / `ctrl-u` | Half a page down / up |
| `zo` `zc` `za` | Open / close / toggle |
| `zR` / `zM` | Open all groups / fold to the three groups |
| `1` `2` `3` | Sort and chart by cpu / memory / network |
| `/` | Filter by app or process name as you type. Enter keeps it, Esc clears it |
| `dd` | Kill the selected app, or the selected process when you are in the process table. Asks `y / n` first |
| `q` | Quit |

A pending prefix (`g`, `z`, `d`) shows at the right of the bottom line. The mouse also works:
click a row to select it, click `cpu`, `mem` or `net` in the list's border, scroll to move.

`dd` is polite. An app is asked to quit (so it can still prompt to save), a dev item gets
`SIGTERM` (the root first, then any children still alive), and a container gets `docker stop`.
parasight waits up to five seconds and tells you whether it closed. If a process is still
running it asks separately before sending `SIGKILL`; it never does that by itself, and never
for apps.

## How grouping is decided

Each process is classified by the first rule that matches.

1. **system**: it runs as a system user (uid below 500), or its executable is under
   `/System`, `/usr/libexec`, `/usr/sbin`, `/sbin` or `/Library/Apple`, or it is a
   launchd-managed tool from `/usr/bin` or `/bin`. Related processes become one item
   (`spotlight`, `webkit`, `coreaudio`, `bluetooth`, `icloud`, `siri`).
2. **apps**: its executable is inside a `.app` bundle. Everything inside the same top-level
   bundle, helper bundles included, is one item named after the app. Apps in
   `/System/Applications` count, and so does anything with a Dock icon (Finder).
   Interactive shells belong to the terminal app they run in.
3. **dev**: everything else you started. A process tree is one item under its root command,
   and containers are listed here too.

Two exceptions keep this useful:

- Language servers and similar tools (`rust-analyzer`, `tsserver`, `gopls`, `pyright`,
  `clangd` and others on a built-in list) go to **dev** even when an editor started them.
- Interpreters that ship as a bundle (Homebrew's `Python.app`) are not apps.

Dev items are named from the command, not the binary: `next dev  :3000`, `postgres  :5432`,
`cargo build`, `seed.py`. Wrappers such as `npm run dev` or `sh -c` are named after what they
started, and a listening port is added when there is one. The path line shows the working
directory and the command.

To override a decision, create `~/.config/parasight/rules.toml`:

```toml
[pin]
ollama = "dev"          # process or app name -> apps | dev | system
"my daemon" = "system"
```

## Where the numbers come from

| What | Source |
|---|---|
| Processes, cpu, memory | `libproc`, read directly. Memory is the physical footprint, the figure Activity Monitor shows |
| Other users' processes (root, system accounts) | `ps`, every 4 s. The kernel does not give their cpu or memory to an unprivileged program, and `ps` is setuid |
| Per-process network | One long-running `nettop`, read as a stream. macOS has no public API for this |
| Listening ports | `libproc`, every 6 s |
| Containers | The Docker socket (`/var/run/docker.sock`, colima, OrbStack, Rancher Desktop, podman, or `DOCKER_HOST`) |
| Which apps have a Dock icon, bundle ids | `lsappinfo list`, every 12 s |
| Which processes launchd manages | `launchctl list`, every 12 s |

The screen refreshes every 2 seconds and keeps 2 minutes of history. Nothing blocks the
interface: every subprocess and socket runs on a background thread.

## Known limits

- **macOS only.**
- **Other users' processes** (root's, `WindowServer`) get cpu and memory from `ps` every 4 s,
  have no network figures unless `nettop` reports them, and cannot be killed without `sudo`.
  `kernel_task` is not shown.
- **Network** lags by about two seconds, and `nettop` only knows processes with open sockets.
- **cpu** is a percentage of one core, as in Activity Monitor, so a group can exceed 100%.
- **Containers** have no process list, and their memory is also counted inside the VM that
  runs them (Docker Desktop, colima), so the two overlap.
- **Web content of Safari** runs as system-owned WebKit services and shows under `system`
  as `webkit`, not under Safari.
- **Helper cost.** parasight itself uses about half a percent of one core. The `nettop` it
  keeps running uses about 1.5%, and the `ps` poll about 0.4%.
