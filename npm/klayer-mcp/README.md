# klayer-mcp

A zero-install launcher for the [klayer](https://github.com/walkowicz19/klayer) MCP server.

This package does not contain klayer itself. On first run it detects your OS/arch, downloads the matching prebuilt `klayer` binary from the project's [GitHub Releases](https://github.com/walkowicz19/klayer/releases) page, caches it under `~/.klayer/bin/<version>/`, and execs it — transparently proxying stdin/stdout/stderr. Every run after the first reuses the cached binary and starts instantly.

Supported platforms: Linux x64, Windows x64, macOS x64 (Intel), macOS arm64 (Apple Silicon). Any other platform/arch fails loudly with a clear error rather than guessing.

## Use it

No install step required — point your MCP client at `npx`:

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

The first launch downloads the binary (may take a few seconds); subsequent launches are instant since the binary is cached at `~/.klayer/bin/`.

This is an *additive* install path — the manual binary download documented in the main [klayer README](https://github.com/walkowicz19/klayer#-for-users) still works and is unaffected.

### Environment variables

| Variable | Effect |
|---|---|
| `KLAYER_MCP_VERSION` | Pin to a specific release tag (e.g. `v1.6.5`) instead of resolving `latest` |
| `KLAYER_DB`, `KLAYER_CODE_DB`, `KLAYER_TRAIN_DB`, `KLAYER_SESSION_DB`, etc. | Passed through untouched — same env vars the klayer binary itself reads, see the main README |

Any extra CLI arguments passed to `klayer-mcp` are forwarded straight through to the underlying binary, e.g.:

```bash
npx -y klayer-mcp@latest --print-mcp-config
```

## Manual publish (no CI automation)

This package is **not** published automatically — there is no GitHub Actions workflow wired up for `npm publish`, by explicit design (no `NPM_TOKEN` secret exists in this repo). Publishing is a manual step performed locally:

```bash
cd npm/klayer-mcp
npm login
npm publish
```

**Before publishing**, bump the `version` field in `package.json` to match the klayer release tag this build targets (e.g. klayer `v1.6.6` → `klayer-mcp` version `1.6.6`). The launcher itself defaults to resolving whatever GitHub release is currently tagged `latest`, so keeping the npm package version aligned with the release tag is just for humans tracking compatibility, not something the script depends on at runtime.
