# Codenotch for Windows

> **This is not the original project.** It is an unofficial Windows distribution of
> [vinzdg/codenotch](https://github.com/vinzdg/codenotch), a macOS app. All the credit for the
> idea, the design and the name goes to [@vinzdg](https://github.com/vinzdg); the Windows port
> itself was written by [@Im-Midi](https://github.com/Im-Midi). This repository exists for one
> reason only: **to ship a prebuilt installer**, because neither upstream repository publishes one.
>
> If you are on macOS, go to [the original repository](https://github.com/vinzdg/codenotch) instead.

The usage notch sits on the edge of your screen and answers two questions at a glance:
**how much of my AI allowance is left**, and **is Claude still working**.

Same design language as the macOS original (inverse-rounded pill, colour-graded rings, hover card
with per-window bars), rebuilt for Windows in Rust + Tauri 2 / WebView2.

## Download

Grab the installer from the [Releases page](../../releases/latest) and run it. Nothing else to
install — the WebView2 runtime ships with Windows 11.

The installer is **not code-signed**, so Windows SmartScreen will show a
*"Windows protected your PC"* dialog. Click **More info → Run anyway** if you want to proceed.
Code signing needs a paid certificate; if you would rather not trust an unsigned binary,
[build it yourself](#build-from-source) — it is two commands.

## What it shows

| Cell | Source | How it reads it |
|---|---|---|
| **Claude** | `GET https://api.anthropic.com/api/oauth/usage` with the token Claude Code keeps in `~/.claude/.credentials.json` | Session / weekly windows, 429 back-off with a persisted deadline, stale readings dimmed with their age. A thin arc spins inside the ring while a Claude session is working, and pulses amber when one is waiting on you (Claude Code hooks + transcript watcher, desktop app included). |
| **Codex** | `GET https://chatgpt.com/backend-api/wham/usage` with the session Codex keeps in `~/.codex/auth.json` (read only, never refreshed), falling back to the `rate_limits` snapshot in the newest rollout log | Live primary/secondary windows (5h + weekly on paid plans, a monthly window on free) while Codex is signed in; otherwise the last snapshot, marked stale by its own timestamp. |
| **Cursor** | The editor's own session from `state.vscdb` → `cursor.com/api/usage-summary` | Included usage / API usage / on-demand, reset at billing-cycle end. Nothing to sign into: it borrows the editor's session, so there is only ever one account. |
| **Antigravity** | The local `language_server` bridge (quota summary), then Google's Cloud Code API for licensed accounts, then a plain count of today's model turns | Honest degradation: a percentage only when one exists, a `~count` when it does not. |

Providers that are not installed simply do not get a cell. The providers the macOS app has and this
one does not — GLM, Grok, Gemini, GitHub Copilot, OpenCode, Perplexity — are not ported yet.

### The Claude cell needs the CLI signed in

The Claude reading comes from the OAuth token in `~/.claude/.credentials.json`, which the Claude
Code CLI writes when you sign in. Sign in to the desktop app only and that file stays a stub with an
empty `accessToken`, so the cell shows session activity but no percentages. Run `claude` in a
terminal and `/login` once to fill it in.

The macOS app has a second route for this — it runs `claude "/usage"` and reads the output, so it
needs no credential of its own — and that route is not ported here yet.

## Tray menu

Refresh now, reset position, open data folder (`%APPDATA%\codenotch` — logs, persisted readings,
icon overrides), start with Windows, install/uninstall Claude Code hooks.

Installing the hooks rewrites `~/.claude/settings.json` to add seven event hooks. It backs the file
up first (`settings.json.codenotch-bak-<timestamp>`) and leaves your own hooks alone, but it is an
edit to a file you may care about — the "uninstall" entry in the same menu reverses it.

## Hiding a provider

A provider that is installed still gets a cell. To leave one out, add its name to
`hidden_providers` in `%APPDATA%\codenotch\config.json` and restart the app:

```json
{ "hidden_providers": ["antigravity"] }
```

Accepted names: `codex`, `cursor`, `antigravity`. A hidden provider is reported as `absent` — the
same state the notch already uses for a provider that is not installed — so no cell is drawn and
its poller never runs.

## Build from source

Prerequisites: Rust (MSVC toolchain), Visual Studio Build Tools with the C++ workload, and the
WebView2 runtime (already present on Windows 11).

```powershell
cargo build --release
.\target\release\codenotch.exe          # pill appears on the right edge of the primary monitor
.\target\release\codenotch.exe doctor   # self-diagnosis: credentials, data sources, icons, hooks
```

To build the installer instead of a bare exe:

```powershell
cargo install tauri-cli --version "^2" --locked
cargo build --release -p codenotch-hook
copy target\release\codenotch-hook.exe target\release\codenotch-hook-x86_64-pc-windows-msvc.exe
cd codenotch
cargo tauri build
```

The hook is built first on purpose. `bundle.externalBin` in `codenotch/tauri.conf.json` asks for it
under its target-triple name — that is what makes the installer drop `codenotch-hook.exe` next to
`codenotch.exe`, exactly where `hooks_install.rs` looks for it — and `tauri-build` resolves that
path while compiling the app, so building the whole workspace in one go fails on a clean tree.

The first build takes a few minutes: `rusqlite` is compiled with the `bundled` feature, which
builds the SQLite C sources from scratch.

### Icons

Provider marks are the SVGs from [`@lobehub/icons-static-svg`](https://github.com/lobehub/lobe-icons)
(MIT), embedded unmodified — see `codenotch/glyphs/NOTICE.md`. Drop your own
`claude|codex|cursor|gemini.svg` (or `.png`) into `%APPDATA%\codenotch\glyphs\` to override.
The marks remain the trademarks of their owners.

## Layout

```
.
├── codenotch/          Tauri 2 app: window, tray, providers (usage.rs, codex.rs, cursor.rs, antigravity.rs),
│   ├── src/            session engine (watcher.rs, state.rs, focus.rs), glyphs.rs, doctor.rs
│   ├── ui/notch.html   the pill + hover card (single file, no framework)
│   └── glyphs/         provider marks (+ NOTICE.md)
├── codenotch-hook/     <5 ms hook messenger Claude Code calls; forwards events to the app
└── docs/specs/         the upstream design spec this port follows
```

## Credits and relationship to upstream

- **[vinzdg/codenotch](https://github.com/vinzdg/codenotch)** — the original macOS app, the design,
  and the name. This repository keeps its full git history, so every upstream commit stays attributed.
- **[Im-Midi/codenotch-windows](https://github.com/Im-Midi/codenotch-windows)** — the Rust/Tauri port,
  contributed upstream as its `windows/` tree. No code was copied from the Swift app; the providers
  were reimplemented from their documented behaviour and the wire formats. The session-detection
  engine originated in [Im-Midi/Pac-Man](https://github.com/Im-Midi/Pac-Man) (MIT).
- **This repository** — the same code with the macOS tree removed, plus CI that builds and publishes
  the installer. No functional changes to the port.

Bug reports about the app itself are better filed upstream. Issues with the installer or the build
belong here.

## License

MIT. `LICENSE` covers the Windows port (Im-Midi and contributors); `LICENSE-UPSTREAM` covers the
material inherited from the original project, including the app icon. Third-party notices are in
both files and in `codenotch/glyphs/NOTICE.md`.
