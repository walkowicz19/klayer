# klayer

<p align="center">
  <img src="logo-v2.jpg" alt="klayer logo" width="200" />
</p>

A domain-agnostic, **grounded knowledge layer** for LLMs, shipped as a single
Rust MCP server. One binary gives any MCP-compatible client (Claude Code, Claude
Desktop, Cursor, …) the ability to ingest sources, recall them with provenance,
enforce only validated rules, honor user preferences, and keep an audit trail
for agentic runs — without fat SKILL.md files and without per-server install pain.

## Why it exists

- **Skills bloat.** Large SKILL.md files degrade attention and invite hallucination.
  klayer keeps the skill a _thin router_ and pulls data on demand via `recall`.
- **MCP install friction.** One static binary, one command, one config block.
- **Trust.** Ingested content is _untrusted data_, never instructions. Only
  `reviewed`/`user` knowledge is ever enforced. This is the safety spine, in code.

## Architecture

```
kl-core    types + traits (Kind, Trust, SearchBackend, Embedder, RecallHit)
kl-store   SQLite: schema, migrations, FTS5 retrieval, trust lifecycle
kl-ingest  fetch (HTTP or local file) -> content-type dispatch -> chunk
kl-search  SearchBackend trait + DuckDuckGo / Bing / Brave with auto-fallback
kl-skill   renders the THIN SKILL.md router from registries only
kl-mcp     the `klayer` binary: rmcp server wiring the tool surface
```

Trust lifecycle (the one invariant across every use case):

```
untrusted ──(LLM extracts)──> proposed ──(promote = validation gate)──> reviewed
                                                                         user (authored)
only reviewed + user are ENFORCED.
```

## Tools

| Tool | Description |
|---|---|
| `recall` | Retrieve grounded knowledge for a domain (FTS5 + curated rules) |
| `search_web` | Web search via configured engine; results are DATA only |
| `ingest` | Fetch a URL or local file and chunk it into the reference tier |
| `remember` | Store a user-authored fact (`trust=user`, enforced immediately) |
| `propose` | Submit a candidate rule/fact (`trust=proposed`, not enforced) |
| `promote` | Validate a proposed item → `trust=reviewed` (now enforceable) |
| `forget` | Delete a knowledge item by id |
| `list_knowledge` | List facts/rules/procedures in a domain with trust and ids |
| `list_sources` | List every ingested file or URL with provenance and fetch time |
| `list_episodes` | Query the agentic run audit trail |
| `list_domains` | Show all registered domains with doc and rule counts |
| `set_preference` | Store a durable user preference (always honored) |
| `register_domain` | Create or update a domain with description and query hint |
| `clear_domain` | Bulk-delete chunks and/or knowledge for a domain |
| `log_episode` | Record one step of an agentic run for auditability |
| `compile_skill` | Regenerate the SKILL.md router from the current registries |

## Quick start (pre-built binary)

A compiled Windows binary is included at `target/release/klayer.exe`.
No Rust toolchain or model downloads required — just download and run.

1. Download `target/release/klayer.exe` from this repository.
2. Wire it into your MCP client config:

```json
{
  "mcpServers": {
    "klayer": {
      "command": "C:\\path\\to\\klayer.exe",
      "env": {
        "KLAYER_DB": "C:\\path\\to\\klayer.db"
      }
    }
  }
}
```

3. Start your MCP client — klayer is ready.

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `KLAYER_DB` | `klayer.db` | Path to the SQLite database |
| `KLAYER_SKILL` | `skills/klayer/SKILL.md` | Path where `compile_skill` writes the router |
| `KLAYER_SEARCH` | `auto` | Search engine: `auto` · `duckduckgo` · `bing` · `brave` |
| `KLAYER_BRAVE_API_KEY` | — | Required when `KLAYER_SEARCH=brave` |
| `RUST_LOG` | `info` | Log level (logs go to stderr, not the MCP channel) |

### Search engine options

| Value | Behaviour |
|---|---|
| `auto` _(default)_ | DuckDuckGo first; falls back to Bing automatically on empty results |
| `duckduckgo` | DuckDuckGo HTML scraping only |
| `bing` | Bing HTML scraping only |
| `brave` | Brave Search REST API — most reliable; free tier 2 000 req/month at [brave.com/search/api](https://brave.com/search/api/) |

## Ingest sources

`ingest` accepts HTTP/HTTPS URLs, absolute local file paths, and `file://` URIs.
Content-type is detected automatically:

| Type | Detected by |
|---|---|
| HTML | `text/html` response header or `.html`/`.htm` extension |
| PDF | `application/pdf` or `.pdf` extension |
| JSON | `application/json` or `.json` extension |
| Markdown | `text/markdown` or `.md`/`.markdown` extension |
| Plain text | `text/plain` or `.txt`/`.csv` extension |

```
# Web URL
ingest("https://example.com/policy.html", "company-policies")

# Local PDF — no server needed
ingest("C:\\policies\\hr.pdf", "company-policies")

# JSON API response
ingest("https://api.example.com/rules.json", "secure-coding")
```

## Typical workflow

```
1. register_domain("my-domain", description, query_hint)
2. ingest(url_or_path, "my-domain")          # repeate for each source
3. recall("my-domain", "your question")      # model grounds its answer
4. propose("my-domain", "rule", title, body) # model extracts a candidate rule
5. promote(id)                               # you validate it → now enforced
6. compile_skill()                           # regenerate SKILL.md router
```

## Memory management

```
list_knowledge("my-domain")                  # see all curated items + ids
list_sources("my-domain")                    # see all ingested files/URLs
forget(id)                                   # delete one item
clear_domain("my-domain")                    # wipe everything
clear_domain("my-domain", chunks_only=true)  # keep rules, clear ingested docs
```

## Build from source

Only needed to modify klayer or target a different OS. Requires `rustup`.

```bash
cargo build --release
KLAYER_DB=./klayer.db ./target/release/klayer
```

## Vector retrieval (optional, later)

Default build is **keyword-only** (FTS5/BM25) — zero extra native dependencies.
The vector extension point:

- `Embedder` trait in `kl-core`.
- Add `chunks_vec` virtual table via `sqlite-vec` + a local CPU embedder
  (e.g. `fastembed`, bge-small-384), fuse FTS + vector with RRF in `Store::recall`.
- Gate behind the `embed-local` feature flag in `kl-mcp`.
