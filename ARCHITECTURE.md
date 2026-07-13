# Architecture

This document describes klayer's crate layout, the module conventions introduced by the
2026-07 refactor, and a few decisions that look wrong out of context but are intentional —
read this before "fixing" them.

## Crate responsibilities

```
kl-core     shared types + traits (Kind, Trust, SearchBackend, Embedder, RecallHit),
            open_db() helper for libsql-backed crates
kl-store    rusqlite: general knowledge — domains, knowledge, sources, trust, ACL,
            episodes, model registry, routing rules, media metadata (always local SQLite)
kl-session  libsql: session memory journal (local file, optional Turso sync)
kl-code     libsql: codebase index + FTS5, language detection, symbol extraction
            (local file, optional Turso sync)
kl-train    libsql: fine-tuning dataset capture/gate/export (local file, optional Turso sync)
kl-ingest   fetch (HTTP or local file) → content-type dispatch → chunk
kl-search   SearchBackend trait + DuckDuckGo / Bing / Brave with auto-fallback
kl-mcp      the `klayer` binary: rmcp MCP server + axum dashboard HTTP server
```

**Storage model:** general knowledge (the governance core) stays on plain SQLite via
`rusqlite` — zero tolerance for beta instability. Codebase, session memory, and training
data run on `libsql`, which can optionally sync a local file against a remote Turso
database. See the main README's "For Developers → Architecture" section for the
user-facing version of this table; this file is the deeper, contributor-facing companion.

## Module convention (introduced in the 2026-07 refactor)

Before this refactor, `kl-mcp/src/main.rs` had grown to ~4,900 lines and
`kl-store/src/lib.rs` to ~2,300 lines, each mixing many unrelated responsibilities with no
consistent internal structure. The convention going forward:

- **Each crate's `lib.rs`/`main.rs` is a thin composition root** — type/struct definitions,
  `mod` declarations, and top-level wiring only. Domain logic lives in named modules
  (e.g. `kl-store/src/domains.rs`, `kl-code/src/lang.rs`, `kl-mcp/src/tools/knowledge.rs`).
- **Split a file once it's approaching ~500–800 lines** of a single responsibility, or once
  it visibly mixes more than one concern (e.g. "language detection" and "symbol parsing"
  are two concerns, even though both live under "codebase indexing").
- **This is a modular monolith, not a repository-pattern or microservices architecture.**
  Prefer plain `pub(crate) fn` free functions grouped by module, or plain multiple
  `impl SomeStruct { ... }` blocks split across files (Rust allows this natively). Only
  introduce a wrapper struct (e.g. a `FooRepo<'a> { conn: &'a Connection }`) when it's a
  clean, low-effort transformation — don't add trait abstractions or generics
  speculatively. Per this project's own recalled clean-code guidance: apply KISS/YAGNI
  before abstraction.
- **kl-mcp's MCP tool handlers** are grouped under `tools/` by workflow stage
  (`knowledge.rs`, `codebase.rs`, `session.rs`, `training.rs`, `media.rs`, `admin.rs`),
  mirroring the grouping used in `klayer-skill/SKILL.md`. Axum dashboard wiring lives in
  `dashboard.rs`; process bootstrap/CLI concerns live in `bootstrap.rs`.

## Do-not-"fix" list

A few things look like inconsistencies or oversights but are deliberate:

- **kl-store opens its own `rusqlite::Connection` directly**, instead of reusing
  `kl_core::open_db()` like the libsql-backed crates do. This is required: `rusqlite` here
  is aliased to `libsql-rusqlite` (see the workspace `Cargo.toml`, lines 33–41) specifically
  to avoid linking two separate bundled SQLite C builds into the `kl-mcp` binary at once
  (duplicate `sqlite3_*` symbol errors). Do not attempt to unify kl-store's connection
  handling with `kl_core::open_db()` — they are backed by genuinely different SQLite
  bindings for a documented linking reason.
- **Error handling is `anyhow`-only across the entire workspace.** `thiserror = "2"` is
  declared as a workspace dependency but is not used by any crate — it's a dead
  dependency, not a partially-adopted pattern. Either adopt it deliberately somewhere with
  a real need for typed errors, or remove it from the workspace `Cargo.toml` as a cleanup
  pass — don't assume it's wired up anywhere today.
- **`get_klayer_dir()` never falls back to the current working directory.** It tries
  `USERPROFILE`, then `HOME`, then (Windows only) `HOMEDRIVE`+`HOMEPATH`, and if none
  resolve, it exits with a loud, actionable error rather than silently defaulting to `.`.
  This used to silently create a `./.klayer/` (with its own databases) inside whatever
  folder happened to be the process's cwd when an IDE spawned the MCP server subprocess
  without `USERPROFILE`/`HOME` in its environment — which looked like klayer "duplicating"
  databases per open project folder. Do not reintroduce a `.`/cwd fallback here.
- **`dashboard.html` is not purely compile-time embedded.** `load_dashboard_html()` in
  `kl-mcp` checks, in order: the `KLAYER_DASHBOARD_HTML` env var, then a `dashboard.html`
  file next to the running executable (e.g. `target/release/dashboard.html`), and only
  falls back to the compile-time-embedded copy if neither exists. If you edit
  `crates/kl-mcp/src/dashboard.html` and are testing against an existing local build, a
  stale disk copy next to the binary will silently shadow your edits. Rebuild (which
  refreshes the file next to the binary) or delete the stale copy before verifying.
