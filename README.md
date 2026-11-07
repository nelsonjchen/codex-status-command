# codex-status-json

Small local CLI that prints Codex account/quota status as JSON.

This exists because there is not yet a supported `codex status --json` command. Upstream context: [openai/codex#10233](https://github.com/openai/codex/issues/10233).

The default backend uses the documented-but-experimental [`codex app-server`](https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md) JSON-RPC interface.

## Install

```sh
cargo install --path .
```

That installs `codex-status-json` into `~/.cargo/bin`. On this machine, `~/.cargo/bin` is already in `PATH`, so other projects can call:

```sh
codex-status-json
```

## Usage

```sh
codex-status-json
codex-status-json --backend app-server
codex-status-json --backend auto
codex-status-json --backend pty
codex-status-json --timeout-ms 30000
```

Default mode is `--backend app-server`, which uses Codex's JSON-RPC interface and never opens the TUI.

`--backend auto` tries `codex app-server` JSON-RPC first, then falls back to opening the interactive TUI in a PTY and running `/status`.

Example output:

```json
{
  "schema_version": 1,
  "account": {
    "email": "user@example.com",
    "plan": "Pro",
    "raw": "user@example.com (Pro)"
  },
  "model": {
    "name": "gpt-5.5",
    "details": "reasoning high",
    "raw": "gpt-5.5 (reasoning high)"
  },
  "buckets": [
    {
      "name": "default",
      "limits": [
        {
          "kind": "5h",
          "remaining_percent": 99,
          "resets": "14:44",
          "reset_at_unix": 1780350290
        },
        {
          "kind": "weekly",
          "remaining_percent": 70,
          "resets": "08:23 on 7 Jun",
          "reset_at_unix": 1780845815
        }
      ]
    }
  ]
}
```

The PTY backend omits `reset_at_unix` because `/status` renders reset times as text.

The app-server backend also includes `banked_resets` when Codex provides a reset-credit summary:

```json
{
  "banked_resets": {
    "available_count": 1,
    "credits": [
      {
        "id": "example-credit",
        "reset_type": "codexRateLimits",
        "status": "available",
        "granted_at_unix": 1780350290,
        "expires_at_unix": 1782942290,
        "title": "Full reset",
        "description": null
      }
    ]
  }
}
```

`credits: null` means details are unavailable; `credits: []` means details were fetched but no available credits were returned. The backend may return fewer details than `available_count`. A null `expires_at_unix` means the credit does not expire. Title and description may also be null. These reads never redeem or consume resets.

Older Codex versions that do not return a summary, and the PTY backend, omit `banked_resets` rather than reporting a zero balance. This optional addition retains `schema_version: 1`.

## Backends

`app-server` starts `codex app-server --listen stdio://` and calls:

- `account/read`
- `account/rateLimits/read`
- `config/read`

This is structured and less fragile than screen scraping, but `codex app-server` is still documented as experimental.

`pty` starts `codex --no-alt-screen` in a PTY, handles the `TERM=dumb` continue prompt, sends `/status`, waits for the panel to settle, captures the rendered text, sends `/quit`, strips terminal escape sequences, and parses quota lines.

The PTY backend uses runtime-only `-c` overrides to avoid starting MCP servers where possible and to avoid writing project trust state. If Codex still asks to trust the working directory, the command refuses instead of modifying Codex config.

## Tests

```sh
cargo test
cargo run -- --parse-fixture fixtures/status-panel.txt
```

## Limitations

- This is a local workaround, not a supported Codex API.
- Field names and JSON shape from `codex app-server` may change.
- The PTY parser depends on the current `/status` rendering and may need updates when the TUI changes.
- Reset text is formatted in the local timezone.
- The command reads existing Codex auth/config state but should not modify login state, repo files, Git state, or Codex config.
