# mcpmux

Centralizes a single instance of each of your agent's local stdio mcp servers to avoid repetitive instantiation across sessions and lower resource usage on your machine. All harnesses that support mcp over http streams are supported. The tool starts the centralized server lazily and stops again after a configurable timeout, so processes are not held open indefinitely. This allows you to scale your mcp enabled local agents without running out of memory or resorting to remote mcp.

Currently, `mcpmux` is limited to macos (launchd, `~/Library/Logs`, `arc4random`).

## How it works

- Each `[servers.<name>]` in the config is served at `http://127.0.0.1:7800/<name>/mcp`.
- Requests from all sessions are multiplexed onto one child process. JSON-RPC ids and progress tokens are rewritten per request, and replies and progress are routed back to the right caller. When a client hangs up or sends `notifications/cancelled`, the upstream call is cancelled.
- `initialize` and list responses are cached on disk, so opening a session spawns nothing.
- Children run in their own process group, so stopping a server also kills its subprocesses. Crashed servers restart with backoff.
- Requests need a bearer token, and the Host/Origin headers must be loopback, which blocks browser DNS rebinding.
- Logs record tool names, timings and errors, never arguments or results.

## Install

```sh
cargo install --path . --locked          # → ~/.cargo/bin/mcpmux (stable path for launchd)
mkdir -p ~/.config/mcpmux
cp config.example.toml ~/.config/mcpmux/config.toml   # then edit
mcpmux check --live                      # validates commands, secret files and TCP preconditions, handshakes each server, warms the cache
mcpmux install                           # creates ~/.config/mcpmux/token, loads the LaunchAgent
```

## Configure

```toml
listen = "127.0.0.1:7800"     # default
idle_timeout = "10m"          # default; per-server override allowed
startup_timeout = "60s"       # default
notify = true                 # default; macOS notification when a server crashes, fails to start, or its tunnel drops
log_max_size = "10mb"         # default; units b, kb, mb, gb
log_rollover = false          # default: a log past the limit is deleted; true keeps it as <name>.log.1
label = "local.mcpmux"        # launchd label (default)
path = ["/opt/homebrew/bin", "/usr/bin", "/bin"]   # PATH for children

[servers.postgres]
command = "uvx"
args = ["postgres-mcp", "--access-mode=restricted"]
requires_tcp = "127.0.0.1:5432"          # fail fast instead of spawning if nothing listens
requires_tcp_hint = "run `ssh -fN db-tunnel`"   # appended to that error
requires_tcp_cmd = "ssh -fN db-tunnel"   # run when the port is closed on a call (or T in the TUI); waits 15s
keep_warm = "09:00-18:00"                # no idle stop inside this local-time window
deny_tools = ["some_tool"]               # hidden from tools/list and refused
max_rss_mb = 1024                        # stop (restarts on next call) if the process group grows past this
[servers.postgres.env]
PLAIN = "value"
DATABASE_URI = { secret_file = "~/.secrets/pg.txt", template = "postgresql://user:{secret}@127.0.0.1:5432/db" }
```

Secrets are read from files when the server spawns. They never appear in the config, logs or cache. See [config.example.toml](config.example.toml).

`mcpmux reload` (or `kill -HUP`) applies edits without a daemon restart. Timeouts, deny lists, tunnel settings and `notify` apply at once; a running server whose `command`, `args`, `env`, `cwd` or `path` changed restarts once its in-flight calls finish. Added servers appear and removed ones stop. Changing `listen` still needs a daemon restart.

## Point clients at it

```sh
mcpmux clients claude     # prints `claude mcp remove/add` commands
mcpmux clients pi         # prints mcp.json entries; the token is read via Pi's `!command` header values
mcpmux headers            # {"Authorization": "Bearer …"} for any client with a header helper
```

Keep each server's name the same as its old stdio entry, and tool names and permission allowlists won't change.

## Use

```sh
mcpmux status              # state, PID, RSS, idle countdown, sessions, calls/errors/latency (1h)
mcpmux tui                 # the same, live; ? lists every key and command
mcpmux start <server>      # warm a server before you need it
mcpmux restart <server>    # e.g. after a tunnel reconnects
mcpmux pin <server>        # start it and skip idle stops until `unpin` or a daemon restart
mcpmux reload              # apply config.toml edits
mcpmux logs [server]       # daemon log, or one server's stderr
```

### TUI

The dashboard adds a savings estimate (RAM and processes not spawned per session), tunnel state, the token cost of each server's tool schemas, and 60-minute sparklines for calls, p95 and RSS.

| Key | |
|---|---|
| `s` `x` `r` | start, stop (clears a failure), restart |
| `p` / `T` | toggle pin / run `requires_tcp_cmd` |
| `l` `L` | server log / daemon log: `/` find, `&` filter, `v` level, `f` follow, `o` pager, `y` copy |
| `t` | tool inspector: token cost per tool; space toggles `deny_tools` |
| `R` | reload config |
| `:` | command line: `:restart all`, `:set github.keep_warm 09:00-18:00`, `:check --live`, `:clients claude`; tab completes |
| `?` | help: keys (type to filter, enter runs), settings (edit config.toml in place, comments kept), guide |

## Files

| Path | What |
|---|---|
| `~/.config/mcpmux/config.toml` | Servers (`--config` / `MCPMUX_CONFIG` to override) |
| `~/.config/mcpmux/token` | Bearer token (0600) |
| `~/.cache/mcpmux/<server>.json` | Cached `initialize` and list responses |
| `~/Library/Logs/mcpmux/` | `mcpmux.log` (daemon), `<server>.log` (child stderr); cleared past `log_max_size` (or rolled to `.log.1` with `log_rollover`) |
| `~/.local/state/mcpmux/admin.sock` | CLI/TUI control socket |
| `~/Library/LaunchAgents/<label>.plist` | Written by `install` |

After you change a server's `command`, `args` or `env`, its cache is ignored until the next spawn. `mcpmux check --live` refreshes it. If a fresh spawn advertises a different version or tool set than the cache, connected clients get `notifications/tools/list_changed` and the TUI flags the server with `!`.

## Uninstall

```sh
mcpmux uninstall   # unloads the LaunchAgent; config, token and cache stay
```

Then point clients back at their stdio commands.
