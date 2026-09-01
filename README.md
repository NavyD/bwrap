# bwrap

> [中文](./README.zh-CN.md)

A Rust CLI wrapper around Bitwarden's `bw` CLI that adds an
on-demand, auto-locking local HTTP server.

`bwrap` prints the same raw JSON as `bw`, and any unknown
subcommand is passed through to the real `bw` binary.

## Features

- **`bwrap serve`** — an axum reverse proxy that spawns a
  `bw serve` subprocess on the first request and shuts it down
  gracefully after an idle timeout (default: `10m`)
- **Raw JSON output** — `get`, `list`, `status` talk to the
  local `bw serve` API directly and print the same JSON as `bw`
- **`bwrap unlock`** — unlock the vault and optionally start
  the server as a background daemon
- **Daemon management** — `--daemon`, `--stop`, `--restart` on
  `serve` and `unlock`
- **Passthrough** — any unknown subcommand is forwarded to the
  real `bw` binary, so `bwrap sync` just works

## Install

From source:

```bash
cargo install --path .
```

Or build a release binary:

```bash
cargo build --release
# binary at target/release/bwrap
```

## Quick start

Unlock the vault and start the server as a daemon:

```bash
bwrap unlock --restart --raw
```

Now the local API is available:

```bash
bwrap status
bwrap list items --search github
bwrap get item <id>
```

Stop the daemon when done:

```bash
bwrap serve --stop
```

## How it works

`bwrap serve` listens on `127.0.0.1:8087` by default. The first
request spawns `bw serve` on a random free port (or reuses an
existing server via `--bw-serve-url`) and proxies to it. After
`--idle-lock-timeout` (default `10m`) without traffic, the
subprocess is shut down gracefully and the port is released.

`BW_SESSION` is required for any command that talks to the
server (it is set after unlocking).
