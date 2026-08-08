# bwrap serve 后台进程管理（--daemon / --stop / --restart）设计

日期：2026-08-08
分支：`feature/serve-daemon-restart`（基于 `agentserver`）

## 背景与目标

`bwrap serve` 目前是前台阻塞运行的 axum 反向代理（`src/agent_server.rs`）。
`get_api_url_or_start_bw_serve_daemon`（`src/main.rs:178`）会把当前 exe 以
daemon 方式（`serve --hostname localhost --port 8087` + `setsid` 脱离终端）spawn
到后台，但没有记录 PID 的机制，也没有显式终止/重启的入口。

目标：为 `bwrap serve` 新增三个互斥 flag，提供后台进程管理能力，全平台可用
（Linux/macOS/Windows）：

- `--daemon`：以后台 daemon 方式启动并立即退出；端口被占用时报错退出。
- `--stop`：终止正在运行的后台 daemon。
- `--restart`：先 stop 再拉起新的后台 daemon。

## 方案：HTTP 控制端点

定位并终止 daemon 采用 **HTTP 控制端点** 方案（相对 PID 文件 / sysinfo 进程枚举）。

理由：

- 纯 HTTP，天然全平台兼容（Windows/Linux/macOS），无需跨平台 kill 系统调用差异。
- 能优雅回收 `bw serve` 子进程（`kill_on_drop(true)`）。
- axum 中显式路由优先于 `axum-reverse-proxy` 的 `fallback_service`
  （`axum-reverse-proxy-1.3.0/src/router.rs:30`），控制路由不会进反向代理。

### 控制端点

`POST /__bwrap/shutdown`

- 与 `bw serve` 现有端点（`/status`、`/object/...`）无路径冲突。
- handler 发送 `shutdown_tx.send(true)`，返回 200。

## 架构与数据流

### 1. `src/agent_server.rs` — 控制端点与优雅关闭

- `AppState` 增加字段 `shutdown_tx: watch::Sender<bool>`。
- `start()` 创建 `watch::channel(false)`，`shutdown_rx` 持有于 `start()`。
- `build_router` 增加显式路由 `.route("/__bwrap/shutdown", post(shutdown_handler))`。
- `start()` 改用：

  ```rust
  axum::serve(listener, app)
      .with_graceful_shutdown(async move {
          let _ = shutdown_rx.changed().await;
      })
      .await?;
  ```

- `axum::serve` 返回后进程退出，`bw serve` 子进程由 `kill_on_drop(true)`
  （`agent_server.rs:102`）回收。

### 2. `src/main.rs` — CLI 与 daemon 管理

- `BWServeArgs` 新增三个 bool flag，用 `clap::ArgGroup` 设为互斥：

  - `--daemon`：后台启动。
  - `--stop`：终止后台 daemon。
  - `--restart`：重启后台 daemon。

  均未设置时保持原有前台阻塞逻辑。

- 抽取共享函数 `spawn_daemon(hostname, port) -> Result<()>`（复用
  `get_api_url_or_start_bw_serve_daemon` 的拉起逻辑，`main.rs:178-242`），
  供 `get_api_url_or_start_bw_serve_daemon` 与 `--restart` 复用。
  补全 Windows 分支：`std::os::windows::process::CommandExt::creation_flags`
  （`DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`）。Cargo.toml 的
  `windows-sys` 增加 `Win32_System_Threading` feature。

- `bw_serve` 内分发三个 flag 后直接返回，不进前台阻塞：

  - `--daemon`：先探测端口（`TcpListener::bind`）；占用 → 报错退出；否则
    `spawn_daemon` 后返回。
  - `--stop`：向 `http://localhost:port/__bwrap/shutdown` 发 POST；连接失败 →
    `bail!("no running bwrap serve daemon at ...")`。
  - `--restart`：stop（未运行则报错）→ 轮询端口释放（超时 5s）→
    `spawn_daemon` → 轮询端口就绪（超时 5s）→ 返回。

### 3. 未改动的部分

- `--restart-agent`（`main.rs:67`）现状不动，其端口冲突缺陷本次不修。
- `--pidfile`（`main.rs:86`）仍为占位，不实现。

## 错误处理

- 优雅关闭：控制端点 → `shutdown_tx.send(true)` → `with_graceful_shutdown` 完成 →
  `bw serve` 子进程回收。
- stop 后等待端口释放设上限（5s）；spawn 后等待就绪设上限（5s），超时报错。
- 控制端点不可达（连接被拒 / 超时）→ 报错 `no running bwrap serve daemon`。

## 跨平台

| 能力 | Linux/macOS | Windows |
| --- | --- | --- |
| daemon 脱离终端 | `libc::setsid()`（已有） | `creation_flags(DETACHED_PROCESS \| CREATE_NEW_PROCESS_GROUP)` |
| 控制端点 | HTTP（axum） | HTTP（axum） |
| 端口探测 | `TcpListener::bind` | `TcpListener::bind` |

## 测试

- `agent_server.rs` 单测：构造带 `shutdown_tx` 的 AppState + router，POST
  控制端点断言 200 且 shutdown 信号触发。
- `main.rs` 单测：`spawn_daemon` 参数构造；`--stop` 对不可达端口的报错路径。
- 集成沿用 httpmock + rstest 模式，不引入真实 `bw`。

## 验收标准

- `bwrap serve --daemon` 端口空闲时后台拉起 daemon 并退出；端口占用时报错。
- `bwrap serve --stop` 优雅终止 daemon，`bw serve` 子进程被回收。
- `bwrap serve --restart` 完整走 stop → 端口释放 → spawn → 就绪。
- `cargo test`、`cargo clippy --all-targets`、`cargo fmt` 通过。
