<p align="center">
  <img src="logo.svg" alt="klayer logo" width="160" />
</p>

<p align="center">
  A domain-agnostic, <b>grounded knowledge layer</b> for LLMs — one Rust MCP server binary.
</p>

<p align="center">
  Ingest sources · recall with provenance · enforce only validated rules · keep an audit trail — no fat SKILL.md, no per-project install pain.
</p>

<p align="center">
  <a href="#-for-users">For Users</a> ·
  <a href="#-for-developers">For Developers</a> ·
  <a href="#tools-reference">Tools</a> ·
  <a href="#rest-api-reference">REST API</a> ·
  <a href="#license--attribution">License</a>
</p>

---

## Why klayer

- **No skills bloat.** Routing rules live in the MCP server's own instructions, delivered on connect — no SKILL.md to maintain per repo.
- **One binary, one config block.** No package manager, no per-project install.
- **Trust is the safety spine.** Ingested content is untrusted data, never instructions. Only `reviewed`/`user` knowledge is ever enforced — and enforced domains can gate risky actions on proof that governed knowledge was actually consulted first.
- **Shared memory across agents.** One global MCP config means Claude Code, Cursor, Claude Desktop, etc. all read/write the same store — a shared brain for your local agentic workflows.
- **Audit-ready.** Every run's knowledge usage, trust tier, and approval history exports as JSON or a formatted PDF compliance report.

```
untrusted ──(LLM extracts)──> proposed ──(you promote)──> reviewed / user - only these are ENFORCED
```

---

# 🙋 For Users

<details open>
<summary><b>1. Install (no Rust, no clone required)</b></summary>

Download the binary for your OS from the **[Releases](../../releases)** page:

| OS | File |
|---|---|
| Windows | `klayer-windows-x86_64.exe` |
| macOS (Apple Silicon) | `klayer-macos-arm64` |
| macOS (Intel) | `klayer-macos-x86_64` |
| Linux | `klayer-linux-x86_64` |

Put it in a **permanent folder** — the MCP config is global, and all your databases live next to the binary, so every workspace shares the same memory.

```bash
chmod +x klayer-macos-arm64   # macOS/Linux only
```

**Alternative — no manual download, via npx:**

```json
{
  "mcpServers": {
    "klayer": {
      "command": "npx",
      "args": ["-y", "klayer-mcp@latest"]
    }
  }
}
```

This is a thin launcher ([`npm/klayer-mcp`](npm/klayer-mcp)) that downloads the right platform binary on first run and caches it under `~/.klayer/bin/` — same binary, same databases, zero manual steps. The manual download above still works and gives you full control over binary placement; this is additive, not a replacement.

</details>

<details open>
<summary><b>2. Connect it to your MCP client</b></summary>

**Automatic (recommended):**
```bash
./klayer-windows-x86_64.exe --install                          # merges config into Claude Desktop
./klayer-windows-x86_64.exe --install --client=cursor           # merges config into Cursor (~/.cursor/mcp.json)
./klayer-windows-x86_64.exe --print-mcp-config                  # or just print the JSON to paste yourself
```

`--client` currently supports `claude` (default) and `cursor` — both write the same
`mcpServers` JSON shape to their respective config file. For any other MCP-compatible
client (Antigravity, Windsurf, etc.), use `--print-mcp-config` and paste the output into
that client's config — the shape is the same one Claude Desktop and Cursor both use, but
we haven't verified every other client's exact config file location/format, so there's no
dedicated `--client` flag for them yet.

**Manual** — add to your client's MCP config:

```json
{
  "mcpServers": {
    "klayer": {
      "command": "C:\\Users\\you\\klayer\\klayer-windows-x86_64.exe",
      "env": {
        "KLAYER_DB": "C:\\Users\\you\\klayer\\klayer.db",
        "KLAYER_CODE_DB": "C:\\Users\\you\\klayer\\klayer_code.db",
        "KLAYER_TRAIN_DB": "C:\\Users\\you\\klayer\\klayer_train.db",
        "KLAYER_SESSION_DB": "C:\\Users\\you\\klayer\\klayer_session.db"
      }
    }
  }
}
```

<sub>macOS: `command` → `/Users/you/klayer/klayer-macos-arm64`, paths use `/Users/you/klayer/...`. Linux: same pattern under `/home/you/klayer/...`.</sub>

Restart your client — klayer starts automatically from then on.

</details>

<details>
<summary><b>3. Open the dashboard</b></summary>

klayer starts a live web dashboard at **http://localhost:7474** every time it runs (the port is printed to stderr on startup). It only runs while your MCP client is open.

Switch language (EN / PT / ES / ZH / PL) and light/dark theme from **Settings** — saved in your browser.

![Dashboard overview](docs/screenshot/dashboard.png?v=1.6.0-3)

<details>
<summary>Tour the dashboard sections</summary>

| Section | What's there |
|---|---|
| **Overview** | Dashboard summary, Marketplace, your published domains (Submissions) |
| **Trust & Governance** | Trust Lifecycle (promote proposed → reviewed), Knowledge Conflicts (contradictions between ingested facts, resolved keep/accept/merge) |
| **Knowledge** | Domains, Knowledge items, Sources |
| **Codebase** | Indexed repositories + full-text code search, Session Memory (per-repo work journal) |
| **Observability** | Usage & Cost (token/cost trends, action/outcome rollups), Storage Health (per-database status incl. sync health) |
| **Agent** | Episode Log (full run audit trail), Model Routing → Model Registry & Routing Rules |
| **Training** | Captured fine-tuning examples by provenance and trust |
| **System** | Settings — language, theme, author name, connection info |

</details>

</details>

<details>
<summary><b>4. Publish a domain to the Marketplace</b></summary>

Any domain's reviewed/user knowledge can become a reusable template others can apply with one click.

1. **Domains** page → **Publish** on your domain → snapshots into a pending submission.
2. **Submissions** page → **Export** it to a JSON file.
3. Send that file to the project maintainer (PR, direct message — klayer is local-only, so this step is out-of-band).
4. The maintainer **Imports** it into their admin queue and **Approves** or **Denies** it.
5. Approved templates land in `marketplace.json` and appear for everyone under **Marketplace**.

Set your **author name** once from Settings (changeable every 14 days) — it's attached to everything you publish.

</details>

<details>
<summary><b>5. Everyday knowledge workflow</b></summary>

```
register_domain("my-domain", description, query_hint)
ingest(url_or_path, "my-domain")             # HTML, PDF, JSON, Markdown, plain text, Office docs, code…
recall("my-domain", "your question")         # model grounds its answer, cites sources
propose("my-domain", "rule", title, body)    # model extracts a candidate rule (not yet enforced)
promote(id)                                  # you validate it → now enforced
```

Housekeeping:
```
list_knowledge("my-domain")                  # everything with ids
forget(id)                                   # delete one item
clear_domain("my-domain")                    # wipe a domain entirely
clear_domain("my-domain", chunks_only=true)  # keep rules, clear ingested docs only
```

</details>

---

# 🛠 For Developers

<details open>
<summary><b>Architecture</b></summary>

```
kl-core     shared types + traits (Kind, Trust, SearchBackend, Embedder, RecallHit)
kl-store    rusqlite: general knowledge — domains, knowledge, sources, trust, ACL,
            episodes, model registry, routing rules, media metadata (always local SQLite)
kl-session  libsql: session memory journal (local file, optional Turso sync)
kl-code     libsql: codebase index + FTS5 (local file, optional Turso sync)
kl-train    libsql: fine-tuning dataset capture/gate/export (local file, optional Turso sync)
kl-ingest   fetch (HTTP or local file) → content-type dispatch → chunk
kl-search   SearchBackend trait + DuckDuckGo / Bing / Brave with auto-fallback
kl-mcp      the `klayer` binary: rmcp MCP server + axum dashboard HTTP server
```

**Storage model:** general knowledge (the governance core) stays on plain SQLite via `rusqlite` — zero tolerance for beta instability. Codebase, session memory, and training data run on `libsql`, which can optionally sync a local file against a remote Turso database (`KLAYER_TURSO_URL`/`KLAYER_TURSO_TOKEN`) — unset, it's just a local file, identical to before. Sync failures never break local reads/writes; they're counted and surfaced on the **Storage Health** dashboard page and (if configured) the notification relay.

</details>

<details>
<summary><b>Build from source</b></summary>

Requires `rustup`.

```bash
cargo build --release
KLAYER_DB=./klayer.db ./target/release/klayer
# Dashboard opens automatically at http://localhost:7474
```

**Admin vs. user build** — the `admin` Cargo feature (on by default) unlocks marketplace submission review/approval. Build the user-facing binary without it:

```bash
cargo build --release                              # admin build
cargo build --release --no-default-features        # user build (no review/approve)
```

Verify the two actually differ before shipping both:
```bash
curl http://localhost:7474/api/admin   # {"admin": true}  on the admin build
                                        # {"admin": false} on the user build
```

</details>

<details>
<summary><b>Governance: enforced domains & precondition gating</b></summary>

Any domain can be flagged `enforced` via `register_domain`. For enforced domains:

- `recall` responses prefix enforceable items with `MANDATORY RULE — violating this is a compliance failure:` — imperative framing, not neutral text.
- `execute_change(domain, run_id, action, override?)` refuses to proceed unless a `recall` against that domain already happened earlier in the same `run_id` — structural reinforcement, not just a returned suggestion. `override: true` bypasses the block but is always logged.
- The compliance report (`export_explainability`) surfaces every override and every enforced-domain action taken without a prior recall as a **gap**, so bypassed governance is visible after the fact even when it wasn't blocked in the moment.

This doesn't guarantee a model obeys returned knowledge — no MCP tool can — but it raises adherence, gates the riskiest actions structurally, and makes non-compliance auditable rather than invisible.

</details>

<details>
<summary><b>Compliance export</b></summary>

`export_explainability(format: "json" | "pdf")` joins episode → knowledge-used → trust tier/source/approval history, grouped per run. `"pdf"` renders the same content as a real downloadable document (`/api/explainability?format=pdf` on the dashboard side). Best-effort token/cost figures (`model`, `tokens_used`, `cost` — optional, self-reported on `recall`/`remember`/`ingest`) roll up into **Usage & Cost**; MCP has no standard field for this, so treat it as advisory, not metered billing.

</details>

<details>
<summary><b>Model routing & complexity estimation</b></summary>

`configure_model_registry` manages a per-harness registry of models/sub-agents and a `(domain_type, task_type, complexity_tier) → model` routing matrix — every mutating call previews the change and requires a second `confirm: true` call before it persists (a bad entry silently biases every future recommendation otherwise).

`estimate_task_complexity` recommends a model/sub-agent using codebase signals (optionally scoped to one `repo`) when code exists, or ingested domain knowledge density for greenfield projects — always advisory, reasoning always shown, harness auto-detected from the MCP `initialize` handshake when not passed explicitly. klayer cannot force a host to route accordingly; that's the host's decision.

</details>

<details>
<summary><b>Media attachments (images)</b></summary>

`ingest_media` stores an image (base64) linked to a `knowledge_id` (inherits its trust tier) or a bare `domain` (stays unpromoted/standalone until `attach_media` links it later). Bytes live on the filesystem (`KLAYER_MEDIA_DIR`, content-hash named — identical bytes dedupe automatically), only a reference lives in SQLite. `list_media` filters by domain or knowledge item. Video and object-store backends are a deliberately deferred later increment.

</details>

<details>
<summary><b>Notification relay</b></summary>

Optional outbound webhook (`KLAYER_NOTIFY_WEBHOOK_URL`, unset = fully disabled, zero overhead) fires — batched, rate-limited — on four events only: a knowledge conflict detected, a `proposed` item aging past a threshold, a Turso→local storage fallback, or a spike in domain-permission denials. It's a relay, not a blanket alert system.

</details>

<details>
<summary><b>PII redaction</b></summary>

Every domain redacts pattern-matched PII (emails, phone numbers, card numbers, ID-shaped digit sequences) before it's written by `remember`/`propose`/`ingest`/`log_work` — matches are replaced with `[REDACTED:EMAIL]`/`[REDACTED:CARD]`/`[REDACTED:PHONE]`/`[REDACTED:ID_NUMBER]`, and this runs before the trust-tier/conflict-detection pipeline, not as cleanup after the fact. Default is on; `register_domain(..., redact_enabled: false)` opts a specific domain out for the rare case where it's meant to hold structured PII under proper access control. Session memory (`log_work`) always redacts — it has no per-domain scope to opt out with.

</details>

<details>
<summary><b>Retention (TTL)</b></summary>

`register_domain(..., retention_days: N)` purges that domain's knowledge older than `N` days via an hourly background sweep (`clear_retention: true` removes the limit again); `set_knowledge_retention(id, retention_days)` overrides the domain default for a single item. Default is no expiration — nothing changes until you opt in. Every purge is logged to the Episode Log (`stage=retention_sweep`), so "why did this disappear" is always answerable. Domains created from a Marketplace template are excluded from retention by default (shared reference content other users depend on), unless the template domain itself is given an explicit `retention_days`. `KLAYER_SESSION_RETENTION_DAYS` applies the same sweep to session memory (unset = never expires). `KLAYER_MAX_RETENTION_DAYS`, if set, clamps any `retention_days` request above that ceiling at write time — a per-tenant safety cap, not a per-request rejection.

</details>

<details>
<summary><b>Server mode (VPS / remote access)</b></summary>

klayer's dashboard binds `127.0.0.1`-only by default — unauthenticated, since only you can reach it. Passing `--mode=server` (e.g. `klayer --dashboard --mode=server`) switches this to `0.0.0.0` and *requires* a bearer token on every request: set `KLAYER_SERVER_TOKEN` yourself, or let klayer generate one on first run (printed once to stderr, persisted at `~/.klayer/server_token.txt` so restarts don't rotate it). This is strictly opt-in — without the flag, behavior is byte-for-byte unchanged.

klayer has no built-in TLS termination by design. `--mode=server` without `KLAYER_TLS_TERMINATED=1` prints a loud startup warning that the connection is unencrypted — put a reverse proxy (nginx, Caddy) in front of it for TLS, then set that env var to silence the warning once it's in place.

```bash
curl http://your-vps:7474/api/stats                                    # 401 unauthorized
curl -H "Authorization: Bearer <token>" http://your-vps:7474/api/stats # 200
```

</details>

<details>
<summary><b>Terminal UI (headless / SSH-only access)</b></summary>

For a pure-SSH session with no browser: `klayer status` prints a one-shot plain-text summary (proposed-item count, recent Episode Log entries, per-database Storage Health) and exits — safe to script or pipe. `klayer tui` opens an interactive, read-only terminal view of the same three panels using `ratatui`; navigate with arrow keys, quit with `q`. No write actions (promote, resolve) — those stay web-only; the TUI's job is "check on things from a terminal," not replacing the dashboard.

</details>

<details>
<summary><b>Ingest sources</b></summary>

`ingest` accepts HTTP/HTTPS URLs, absolute local paths, and `file://` URIs. Content-type auto-detected: HTML, PDF, JSON, Markdown, plain text, Office docs (`.docx`/`.xlsx`/`.pptx`), YAML, JSONL, SQL, CSS, and common source-code extensions. `index_codebase` walks a directory for the **Codebase** search tool — known languages get symbol-aware metadata, everything else (including legacy/niche formats) falls back to plain-text chunks; binaries/oversized/unreadable files are skipped with an explicit per-file reason, never a silent success.

</details>

<details>
<summary><b>Training data layer (kl-train)</b></summary>

Turns curated knowledge + the agentic audit trail into fine-tuning datasets, gated by the same trust lifecycle, in its own database. klayer never runs a teacher model or verifier itself — it's capture-only.

| Provenance | Meaning | Promotable? |
|---|---|---|
| `student` | drafted by the model being fine-tuned | **Never** — model-collapse guard |
| `teacher` | labelled by a stronger external model | Yes, via `promote_example` |
| `human` | authored by a person (`author_example`) | Already `trust=user`, exportable |

```
seed_from_topics("cybersecurity")                       # coverage faucet → proposed student stubs
capture_example("cybersecurity", user, assistant, provenance="teacher")
promote_example(id)                                      # gate: proposed → reviewed
export_dataset(out_dir="./dataset_out")                   # reviewed+user only, one <domain>.jsonl each
```

</details>

<details>
<summary><b>Environment variables</b></summary>

| Variable | Default | Description |
|---|---|---|
| `KLAYER_DB` | `klayer.db` | General knowledge database (SQLite) |
| `KLAYER_CODE_DB` | `klayer_code.db` | Codebase index database |
| `KLAYER_TRAIN_DB` | `klayer_train.db` | Training-data database |
| `KLAYER_SESSION_DB` | `klayer_session.db` | Session memory journal database |
| `KLAYER_TURSO_URL` / `KLAYER_TURSO_TOKEN` | — | Optional remote Turso sync target for the three `libsql` databases above |
| `KLAYER_MEDIA_DIR` | `<klayer dir>/media` | Filesystem root for stored media attachments |
| `KLAYER_MARKETPLACE` | `<klayer dir>/marketplace.json` | Marketplace template file |
| `KLAYER_DASHBOARD_PORT` | `7474` | Dashboard HTTP port |
| `KLAYER_NOTIFY_WEBHOOK_URL` | — | Notification relay target; unset disables it entirely |
| `KLAYER_PROPOSED_AGE_THRESHOLD_SECS` | `604800` (7d) | Age threshold before a "proposed item aging" alert fires |
| `KLAYER_DENIAL_SPIKE_THRESHOLD` | `5` | Denials within the window before a "denial spike" alert fires |
| `KLAYER_MAX_RETENTION_DAYS` | — | Per-tenant ceiling; clamps any `retention_days` request above it |
| `KLAYER_SESSION_RETENTION_DAYS` | — | TTL for session memory journal rows; unset = never expires |
| `KLAYER_SERVER_TOKEN` | auto-generated | Bearer token required by `--mode=server`; persisted at `~/.klayer/server_token.txt` if not set |
| `KLAYER_TLS_TERMINATED` | — | Set to silence the unencrypted-connection warning in `--mode=server` once a reverse proxy handles TLS |
| `KLAYER_MCP_VERSION` | `latest` | (npm shim only) pin the release tag `npx klayer-mcp` downloads |
| `KLAYER_SEARCH` | `auto` | `auto` · `duckduckgo` · `bing` · `brave` |
| `KLAYER_BRAVE_API_KEY` | — | Required when `KLAYER_SEARCH=brave` |
| `RUST_LOG` | `info` | Log level (stderr only, never the MCP channel) |

</details>

<details>
<summary><b>Vector retrieval (optional, not yet wired in)</b></summary>

Default build is keyword-only (FTS5/BM25) — zero extra native deps. Extension point: `Embedder` trait in `kl-core`; add a `chunks_vec` table via `sqlite-vec` + a local CPU embedder, fuse with FTS via RRF in `Store::recall`, gate behind the `embed-local` feature.

</details>

---

## Tools reference

<details>
<summary><b>Knowledge & recall</b></summary>

| Tool | Description |
|---|---|
| `recall` | Retrieve grounded knowledge for a domain (FTS5 + curated rules); enforced-domain items get imperative framing |
| `search_web` | Web search via configured engine; results are DATA only |
| `ingest` | Fetch a URL/file and chunk it into the reference tier |
| `remember` | Store a user-authored fact (`trust=user`, enforced immediately) |
| `propose` | Submit a candidate rule/fact (`trust=proposed`, not enforced) |
| `promote` | Validate a proposed item → `trust=reviewed` |
| `forget` | Delete a knowledge item by id |
| `list_knowledge` / `list_sources` / `list_domains` | List with ids, trust, provenance |
| `register_domain` | Create/update a domain (description, query hint, `enforced`/`redact_enabled` flags, `retention_days`/`clear_retention`) |
| `set_knowledge_retention` | Override a domain's retention window for one knowledge item |
| `set_preference` | Store a durable user preference (always honored) |
| `clear_domain` / `clear_domains` / `clear_knowledge` / `clear_sources` | Wipe knowledge-store data at varying scope |

</details>

<details>
<summary><b>Governance & compliance</b></summary>

| Tool | Description |
|---|---|
| `execute_change` | Precondition-gated action tool; refuses on enforced domains without a prior `recall` in-run unless `override: true` |
| `list_conflicts` / `resolve_conflict` | Surface and resolve contradictions between ingested/curated facts |
| `set_domain_permission` | Grant/revoke `(identity, domain)` access |
| `export_explainability` | Audit export (`format: json | pdf`) — knowledge used, trust, source, approvals, compliance gaps |

</details>

<details>
<summary><b>Codebase & session memory</b></summary>

| Tool | Description |
|---|---|
| `index_codebase` | Walk a directory, index source files for semantic search |
| `search_code` | Full-text + semantic search across indexed codebases |
| `list_repos` / `forget_repo` / `clear_codebase` | Manage indexed repositories |
| `log_work` | Append a curated session-journal entry (`done`/`failed`/`avoid`/`decision`/`note`) |
| `recall_session` | Replay a repo's session journal (`recent_context` or `full_session_summary`) |

</details>

<details>
<summary><b>Agent audit trail</b></summary>

| Tool | Description |
|---|---|
| `log_episode` | Record one step of an agentic run |
| `list_episodes` / `clear_episodes` | Query / wipe the audit trail |

</details>

<details>
<summary><b>Model routing</b></summary>

| Tool | Description |
|---|---|
| `configure_model_registry` | `add_model` / `add_sub_agent` / `add_routing_rule` / `update` / `remove`, two-step confirm |
| `estimate_task_complexity` | Advisory complexity + model/sub-agent recommendation, codebase- or domain-derived, optional `repo` scope |

</details>

<details>
<summary><b>Media</b></summary>

| Tool | Description |
|---|---|
| `ingest_media` | Store an image, linked to a knowledge item or standalone in a domain |
| `attach_media` | Link previously-standalone media to a knowledge item |
| `list_media` | List stored media, filtered by domain or knowledge item |

</details>

<details>
<summary><b>Training data</b></summary>

| Tool | Description |
|---|---|
| `capture_example` | Capture a candidate training pair (`teacher`/`student` provenance) |
| `author_example` | Author a human-written pair (`trust=user`, exportable immediately) |
| `promote_example` | Validation gate: proposed → reviewed (refuses `student` rows) |
| `list_training` / `export_dataset` | List / export reviewed+user rows as chat JSONL |
| `queue_weak` | Turn low/zero-hit recall queries into proposed question-stubs |
| `seed_from_topics` | Turn an existing domain's knowledge into varied proposed question-stubs |

</details>

---

## REST API reference

Default port **7474** (`KLAYER_DASHBOARD_PORT` to override). All dashboard pages are thin clients over this API.

<details>
<summary>Full endpoint list</summary>

| Endpoint | Params | Returns |
|---|---|---|
| `GET /` | — | Dashboard SPA |
| `GET /api/stats` | — | Aggregate counts |
| `GET /api/domains` | — | All domains with doc/rule counts |
| `POST /api/domain/update` | `{name, description, query_hint, enforced}` | Edit a domain in place |
| `GET /api/domain/delete` | `name` | Remove a domain and cascading data |
| `GET /api/knowledge` | `domain`, `trust`, `kind` | Knowledge items |
| `POST /api/knowledge/update` | `{id, title, body, …}` | Edit a knowledge item |
| `GET /api/knowledge/delete` | `id` | Remove a knowledge item |
| `GET /api/sources` | `domain` | Ingested sources |
| `POST /api/source/update` | `{id, title, uri}` | Edit a source |
| `GET /api/source/delete` | `id` | Remove a source and its chunks |
| `GET /api/source/chunks` | `source_id` | List a source's chunks |
| `POST /api/chunk/add` / `update` | `{source_id/id, text}` | Add/edit a chunk |
| `GET /api/chunk/delete` | `id` | Delete a chunk |
| `GET /api/conflicts` | — | Open knowledge conflicts |
| `GET /api/explainability` | `run_id?`, `format` (`json`\|`pdf`) | Compliance/explainability export |
| `GET /api/usage` | — | Action/outcome rollups + cost/token daily trend |
| `GET /api/storage-health` | — | Per-database status incl. libsql sync health |
| `GET /api/model-registry` | — | Model/sub-agent registry, grouped by harness → tier |
| `GET /api/routing-rules` | — | Routing rule matrix, grouped by harness |
| `GET /api/episodes` | `run_id` | Agentic run audit trail |
| `GET /api/preferences` | — | User preferences |
| `GET /api/training` | `domain`, `trust` | Training examples |
| `GET /api/journal` | `repo` | Session-journal entries |
| `GET /api/journal/clear` | `repo` | Clear a repo's journal |
| `GET /api/marketplace/templates` | — | Marketplace domain templates |
| `GET /api/marketplace/apply` | `template` | Apply a template |
| `GET /api/submissions` | `status` | Marketplace publish queue |
| `GET /api/submissions/get` | `id` | One submission + snapshotted items |
| `POST /api/submissions/publish` | `{domain}` | Snapshot a domain into a pending submission |
| `POST /api/submissions/review` | `{id, action, note}` | Approve/deny (admin build only) |
| `GET /api/submissions/export` | `id` | Download a submission as JSON |
| `POST /api/submissions/import` | `{json}` | Import a submission (admin build only) |
| `GET /api/submissions/delete` | `id` | Withdraw your own submission |
| `GET /api/author` | — | Author name + cooldown status |
| `POST /api/author` | `{name}` | Register/change author name (14-day cooldown) |
| `GET /api/admin` | — | Whether this is the admin build |

</details>

---

## License & Attribution

MIT — see [LICENSE](LICENSE). Free to use, modify, adapt, or distribute, provided you credit the original repository ([walkowicz19/klayer](https://github.com/walkowicz19/klayer)) and its author.
