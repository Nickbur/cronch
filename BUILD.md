# Building Cronch from source

This is a step-by-step guide to compiling Cronch yourself. Cronch is written in
pure Rust, so the process is the same shape on both platforms: install the Rust
toolchain + a system linker, get the source, and run one `cargo` command.

**You'll get:** a single executable — `cronch.exe` on Windows, `cronch` on
macOS — that runs the tray app.

There is nothing to configure. SQLite is compiled in (no database to install),
and the app stores its data per-user (`%APPDATA%\burakov\Cronch` on Windows,
`~/Library/Application Support/net.burakov.Cronch` on macOS).

> The first build compiles the whole dependency tree (Slint, winit, Tokio, …)
> and takes a few minutes. Every build after that is fast.

---

## Get the source (both platforms)

If you have the repository URL:

```bash
git clone https://github.com/Nickbur/cronch.git
cd cronch
```

Or download the source archive and unzip it, then `cd` into the folder that
contains `Cargo.toml`.

---

## Part 1 — Windows

### 1. Install the C++ build tools (the linker)

Rust on Windows uses the MSVC toolchain, which needs Microsoft's linker and the
Windows SDK. Install the **Visual Studio 2022 Build Tools** with the
**"Desktop development with C++"** workload:

- Download from <https://visualstudio.microsoft.com/downloads/> → "Tools for
  Visual Studio" → **Build Tools for Visual Studio 2022**, or install from a
  terminal:

  ```powershell
  winget install --id Microsoft.VisualStudio.2022.BuildTools -e
  ```

  In the installer, tick **Desktop development with C++** and install. (This
  provides `link.exe`, the MSVC compiler, and the Windows SDK.)

If you already have Visual Studio 2022 with the C++ workload, you can skip this.

### 2. Install Rust

Download and run **rustup** from <https://rustup.rs> (`rustup-init.exe`), or:

```powershell
winget install --id Rustlang.Rustup -e
```

Accept the defaults (the `stable-msvc` toolchain). Cronch uses the Rust **2024
edition**, so you need **Rust 1.85 or newer** — rustup installs the latest
stable, which is fine.

Close and reopen your terminal, then verify:

```powershell
rustc --version
cargo --version
```

### 3. Build

From the project folder (the one with `Cargo.toml`):

```powershell
# Optimized release build (recommended)
cargo build --release
```

Or, to build and run in one step while trying it out:

```powershell
cargo run --release
```

### 4. Run it

The compiled binary is at:

```
target\release\cronch.exe
```

Double-click it in File Explorer, or run it from the terminal:

```powershell
.\target\release\cronch.exe
```

A window opens and a clock icon appears in the system tray (bottom-right, near
the clock — you may need to click the "^" to show hidden icons). Closing the
window hides Cronch to the tray; quit it from the tray menu.

### Windows notes

- **SmartScreen / antivirus:** an unsigned app that runs commands on a schedule
  and starts at login looks, to Windows, a lot like malware. SmartScreen may
  show "Windows protected your PC" — click **More info → Run anyway**. Since you
  built it yourself from source, it is safe. To avoid this on a machine you
  distribute to, sign the binary with a code-signing certificate.
- **Launch at login:** on first run Cronch enables itself to start at login
  (a registry entry under `HKCU\...\Run`). You can turn this off in **Settings**
  inside the app.
- **Debug build:** `cargo build` (without `--release`) is faster to compile,
  produces `target\debug\cronch.exe`, and prints logs to the console — handy if
  something misbehaves.

---

## Part 2 — macOS

The same steps, with Apple's toolchain instead of MSVC.

### 1. Install the Xcode Command Line Tools (the linker)

This gives you `clang` and the system linker Rust needs:

```bash
xcode-select --install
```

Click through the prompt. (You do **not** need the full Xcode app — just the
Command Line Tools.)

### 2. Install Rust

Install **rustup** from <https://rustup.rs>:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Choose the default installation. Then load it into your current shell and
verify:

```bash
source "$HOME/.cargo/env"
rustc --version
cargo --version
```

(As on Windows, you need Rust 1.85+ for the 2024 edition; the latest stable is
fine. Cronch builds on both Apple Silicon and Intel Macs.)

### 3. Build

From the project folder:

```bash
cargo build --release
```

Or build and run at once:

```bash
cargo run --release
```

### 4. Run it

The compiled binary is at:

```
target/release/cronch
```

Run it from the terminal:

```bash
./target/release/cronch
```

The window opens and a clock icon appears in the **menu bar** (top-right).
Closing the window hides Cronch to the menu bar; quit it from the menu.

### macOS notes

- **Gatekeeper:** a binary you build and run locally from the terminal is **not**
  quarantined, so Gatekeeper does not block it. (Quarantine only applies to
  apps *downloaded* from the internet — those show "cannot be opened because the
  developer cannot be verified", which you'd clear with **right-click → Open**,
  or `xattr -dr com.apple.quarantine <path>`.) To ship to other Macs without
  friction you'd sign and notarize the app.
- **Launch at login:** on first run Cronch installs a **LaunchAgent** so it
  starts at login. Turn it off in **Settings** inside the app.
- **A plain binary, not a `.app`:** `cargo build` produces a Unix executable,
  not a double-clickable `.app` bundle. That's all you need to run it. To make a
  proper `.app`/`.dmg`, see "Distributable packages" below.

---

## Optional: run the tests

To confirm everything works after building:

```bash
cargo test
```

All tests should pass (this includes an end-to-end check that actually runs a
scheduled command and records the result).

## Optional: distributable packages

`cargo build --release` gives you a runnable binary, which is enough to use
Cronch. To produce installers/bundles for other people —
a Windows `.exe`/`.msi` or a macOS `.app`/`.dmg` — use a packaging tool such as
[`cargo-bundle`](https://crates.io/crates/cargo-bundle) or
[`cargo-dist`](https://crates.io/crates/cargo-dist). That's beyond this build
guide.

---

## Troubleshooting

- **Windows: `error: linker 'link.exe' not found`** — the C++ build tools aren't
  installed (or not the C++ workload). Redo Part 1, Step 1, making sure
  **Desktop development with C++** is selected.
- **Windows: linker errors when building from Git Bash / MSYS** — Git Bash ships
  its own `/usr/bin/link.exe` that can shadow MSVC's linker. Build from
  **PowerShell** or the **Developer Command Prompt** instead.
- **macOS: `xcrun: error: invalid active developer path`** — the Command Line
  Tools aren't installed. Run `xcode-select --install`.
- **The first build seems stuck** — it isn't; compiling the GUI stack the first
  time takes a few minutes. Subsequent builds are incremental and quick.
