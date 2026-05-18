# Claude Code — Linux GUI (v0.0.1)

A minimal, **native** (GTK4, no Electron) free/open-source desktop GUI for the
official **Claude Code** CLI on Linux.

> Not affiliated with, endorsed by, or sponsored by Anthropic. Independent
> wrapper around the official `claude` command-line tool.

## Honest status

- The **CLI integration logic** (per-turn `--session-id` / `--resume`,
  `--output-format json`, explicit `claude` path resolution) is grounded in
  empirical probes of the installed CLI.
- The **Rust/GTK4 code is UNVERIFIED**: it was written without a Rust
  toolchain or GTK4 dev libraries available to compile or type-check it.
  Expect to fix compile errors on first build. Most likely fix points are
  marked `FRAGILE:` in `src/main.rs` (the file picker API and the
  thread→UI channel are the usual version-churn spots in gtk4-rs).
- Treat v0.0.1 as a starting point to iterate against real `cargo build`
  output, not a finished app.

## v0.1 scope (deliberate)

- **File-editing tools enabled** (`Write,Edit,Read`). **No Bash / command
  execution** — riskier, and `acceptEdits` behaviour for Bash was not verified.
- **Permission handling** (grounded in CLI probes):
  - Reading files **inside** the chosen folder: allowed automatically.
  - Writing files, or reading/writing **outside** the chosen folder: the CLI
    denies and reports it; the GUI shows what Claude needs and enables an
    **Approve** button (typing does NOT grant permission).
  - **Approve** resumes the session with `--permission-mode acceptEdits`
    (covers writes/edits) **and** `--add-dir <dir>` for each denied path's
    directory (covers out-of-folder access). Approved directories accumulate
    and are re-sent on every later turn.
  - **"Auto-approve edits"** checkbox: edits proceed without prompting (still
    no out-of-folder access until something is approved).
- **No streaming** — each turn waits for the full reply (`--output-format json`).
- Reply text shown verbatim (may contain stray markup).

## Prerequisites (heavier than a web app — this is the cost of native)

- A Rust toolchain — install via [rustup](https://rustup.rs).
- GTK4 development libraries, e.g. on Debian/Ubuntu:
  `sudo apt install libgtk-4-dev build-essential`
- The official Claude Code CLI installed and authenticated (`claude`).

## Build & run

```bash
cargo run
```

First build will likely surface crate-version / API mismatches (the code is
uncompiled — see "Honest status"). Paste the errors back to iterate.

## License

**Not chosen yet (TBD).** No license file included on purpose; default
copyright applies until one is added.
