# bwrap

> [English](./README.md)

围绕 Bitwarden `bw` CLI 的 Rust 封装，提供按需拉起、空闲自动锁定的本地 HTTP 服务器。

`bwrap` 输出的原始 JSON 与 `bw` 完全一致，任何未识别的子命令都会透传给真实的
`bw` 二进制。

## 特性

- **`bwrap serve`** — axum 反向代理，首个请求时自动拉起 `bw serve` 子进程，
  空闲超时（默认 `10m`）后优雅关闭
- **原始 JSON 输出** — `get` / `list` / `status` 直接访问本地 `bw serve` API，
  输出与 `bw` 相同的 JSON
- **`bwrap unlock`** — 解锁密码库，并可选择以后台 daemon 方式启动服务器
- **daemon 管理** — `serve` / `unlock` 均支持 `--daemon` / `--stop` /
  `--restart`
- **透传** — 任何未识别的子命令都会转发给真实的 `bw` 二进制，
  例如 `bwrap sync` 开箱即用

## 安装

从源码安装：

```bash
cargo install --path .
```

或构建 release 二进制：

```bash
cargo build --release
# 二进制位于 target/release/bwrap
```

## 快速开始

解锁密码库并以 daemon 方式启动服务器：

```bash
bwrap unlock --restart --raw
```

现在本地 API 已可用：

```bash
bwrap status
bwrap list items --search github
bwrap get item <id>
```

完成后停止 daemon：

```bash
bwrap serve --stop
```

## 工作原理

`bwrap serve` 默认监听 `127.0.0.1:8087`。首个请求会拉起 `bw serve`
（监听随机空闲端口，或通过 `--bw-serve-url` 复用已有服务）并反向代理到它。
超过 `--idle-lock-timeout`（默认 `10m`）无流量后，子进程被优雅关闭并释放端口。

任何与服务器通信的命令都需要 `BW_SESSION` 环境变量（解锁后设置）。
