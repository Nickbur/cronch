# Cronch

A small, friendly **cross-platform task scheduler** for **Windows and macOS**.
Lives in your system tray, runs commands and scripts on a schedule, and keeps a
full history of every run.

> Made by **[burakov](https://burakov.net)** · open source · free forever.

Most schedulers are either bare-bones OS plumbing (Task Scheduler XML, `launchd`
plists, raw `cron`) or headless server daemons. Cronch is a clean desktop GUI
that sits in the tray and just works.

## Features

- **Tray-resident daemon** — a single background process fires your jobs; close
  the window and it keeps running in the tray.
- **Three schedule types** — fixed **interval**, **cron** expression, or a
  one-shot **once** at a specific date/time.
- **Runs any command** through the shell of your choice — Cronch auto-detects the
  shells installed on the machine (PowerShell, `pwsh`, `cmd`, Git Bash, WSL on
  Windows; `zsh`/`bash`/`sh`/`fish` on macOS). You can also point at a **custom
  shell binary** or run a command **directly** with no shell.
- **Catch-up** — if the machine was off or asleep when a job was due, Cronch runs
  it once on the next start (coalesced — no backlog storms). A one-shot that was
  missed still runs.
- **Overlap policy** — per rule: **Skip**, **Queue**, or run in **Parallel** when
  a previous run is still going.
- **Full run history + log viewer** — captured stdout/stderr, exit code, and
  status for every run, with configurable retention.
- **Correct environment** — jobs inherit your real login environment (on macOS,
  Cronch resolves the login-shell `PATH` so `node`/`python`/`git` are found even
  when launched at login). Optional per-rule environment variables and working
  directory.
- **Launch at login** (opt-out) so your schedules keep running.
- **Enable/disable** rules individually, or **Pause all** from the tray.
- **Import / export** your rules as JSON.

## Install / build

Cronch is written in pure Rust (UI: [Slint](https://slint.dev)).

```bash
# Debug run
cargo run

# Optimized build
cargo build --release
# binary at target/release/cronch(.exe)
```

Requires a recent stable Rust toolchain. On Windows the MSVC toolchain is used;
on macOS the standard Apple toolchain.

**New to Rust or setting up a fresh machine?** See [BUILD.md](BUILD.md) for a
detailed step-by-step guide (install the toolchain, build, and run) for Windows
and macOS.

## How it works

- One tray process owns a small SQLite database (the single source of truth for
  rules, runtime state, and history) and a scheduling engine.
- The engine re-evaluates at least every 30 seconds, so clock jumps and
  sleep/wake are handled without OS power events.
- Because it is a tray daemon, **jobs only fire while Cronch is running** — which
  is why launch-at-login is on by default. You can turn it off in Settings.

## Notes

- Cronch runs arbitrary commands on a schedule and starts at login — the same
  shape as some malware — so on Windows an unsigned build may trigger SmartScreen
  or antivirus warnings. Build it yourself or sign the binary to avoid this.
- Linux is intentionally out of scope.

## License

Dual-licensed under either of

- MIT license ([LICENSE-MIT](LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

at your option.
