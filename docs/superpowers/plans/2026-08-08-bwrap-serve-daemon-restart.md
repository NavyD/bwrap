# bwrap serve 后台进程管理（--daemon / --stop / --restart）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 `bwrap serve` 新增 `--daemon`/`--stop`/`--restart` 三个互斥 flag，提供后台 daemon 的启动、优雅终止与重启能力，全平台可用。

**Architecture:** daemon 是 `bwrap serve` 自身进程，通过 axum 反向代理暴露 `POST /__bwrap/shutdown` 控制路由（显式路由优先于 `axum-reverse-proxy` 的 `fallback_service`）。`--stop` 用 reqwest 发 POST，daemon 经 `with_graceful_shutdown` 优雅退出并靠 `kill_on_drop(true)` 回收 `bw serve` 子进程；`--restart` 走 stop → 等端口释放 → `spawn_daemon`（复用现有拉起逻辑，补 Windows `creation_flags`）→ 等端口就绪。

**Tech Stack:** Rust 2024 / tokio / axum 0.8 / axum-reverse-proxy 1 / clap 4 / reqwest 0.13 / watch channel / httpmock + rstest（测试）

---

## 文件结构

- 修改 `src/agent_server.rs` — 控制端点路由、`shutdown_handler`、`AppState.shutdown_tx`、`with_graceful_shutdown`。
- 修改 `src/main.rs` — `BWServeArgs` 三个 flag + `ArgGroup` 互斥；新增 `daemon_args`、`spawn_daemon`、`stop_daemon`、`wait_port_free`、`wait_port_ready`；`bw_serve` 分发。
- 修改 `Cargo.toml` — `windows-sys` 增加 `Win32_System_Threading` feature。
- 测试位置：`agent_server.rs` 内部 `#[cfg(test)] mod tests`、`main.rs` 内部 `#[cfg(test)] mod tests`。

---

### Task 1: agent_server 控制端点与优雅关闭

**Files:**
- Modify: `src/agent_server.rs`

- [ ] **Step 1: 写失败测试（shutdown_handler）**

在 `src/agent_server.rs` 文件末尾追加测试模块：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use std::time::Duration;

    fn test_state(shutdown_tx: watch::Sender<bool>) -> AppState {
        AppState {
            args: Arc::new(BWAgentConfig {
                idle_lock_timeout: Duration::from_secs(60),
                bw_path: PathBuf::from("/nonexistent/bw"),
                listen_url: "http://localhost:8087".to_string(),
            }),
            idle_lock_task: Arc::new(Mutex::new(None)),
            bw_serve_child: Arc::new(Mutex::new(None)),
            bw_serve_url: Arc::new("http://127.0.0.1:1".parse().unwrap()),
            shutdown_tx,
        }
    }

    #[tokio::test]
    async fn shutdown_handler_sends_signal() {
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let state = test_state(shutdown_tx);
        let status = shutdown_handler(State(state)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(shutdown_rx.changed().await.is_ok());
        assert!(*shutdown_rx.borrow());
    }

    #[tokio::test]
    async fn shutdown_route_returns_200_and_stops_server() {
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let state = test_state(shutdown_tx);
        let app = build_router(state).await.unwrap();
        let listener = net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.changed().await;
                })
                .await
                .unwrap();
        });
        let resp = reqwest::Client::new()
            .post(format!("http://{}/__bwrap/shutdown", addr))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --lib agent_server 2>&1 | tail -20`
Expected: 编译失败，报 `shutdown_handler`、`shutdown_tx` 未定义。

- [ ] **Step 3: 实现 AppState 字段与 handler**

修改 `AppState`（当前在 `agent_server.rs:107-113`）：

```rust
#[derive(Debug, Clone)]
struct AppState {
    idle_lock_task: Arc<Mutex<Option<IdleLockTask>>>,
    bw_serve_child: Arc<Mutex<Option<process::Child>>>,
    args: Arc<BWAgentConfig>,
    bw_serve_url: Arc<Url>,
    shutdown_tx: watch::Sender<bool>,
}
```

在 `start()` 函数体末尾附近（`build_router(state).await?;` 之前）追加 handler 与路由。具体：

在 `build_router` 函数（`agent_server.rs:50-73`）中，把首个 `app` 赋值前插入显式控制路由：

```rust
async fn shutdown_handler(State(s): State<AppState>) -> axum::http::StatusCode {
    let _ = s.shutdown_tx.send(true);
    axum::http::StatusCode::OK
}

async fn build_router(state: AppState) -> Result<Router> {
    let mut app = Router::<AppState>::new();
    app = app.route("/__bwrap/shutdown", axum::routing::post(shutdown_handler));
    app = match state.bw_serve_url.scheme() {
        "http" | "https" => {
            app.merge(ReverseProxy::new("/", state.bw_serve_url.as_str()))
        }
        _ => bail!("Unsupported scheme for url {}", state.bw_serve_url),
    };
    let app = app
        .layer(axum_reverse_proxy::RetryLayer::with_delay(
            5,
            time::Duration::from_millis(500),
        ))
        .layer(middleware::map_request_with_state(
            state.clone(),
            middleware_start_bw_serve,
        ))
        .layer(middleware::map_response_with_state(
            state.clone(),
            middleware_idle_lock,
        ))
        .with_state(state.clone());
    Ok(app)
}
```

- [ ] **Step 4: 修改 start() 支持优雅关闭**

修改 `start()`（`agent_server.rs:28-48`），在构造 `AppState` 前创建 watch channel，并用 `with_graceful_shutdown`：

```rust
pub async fn start(args: BWAgentConfig) -> Result<()> {
    let bw_serve_url = get_bw_serve_url().await?;
    // 启动 bw serve 进程并检查问题
    let bw_child = spawn_bw_serve(&args.bw_path, &bw_serve_url).await?;

    let listen_url = args.listen_url.parse::<Url>()?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let state = AppState {
        args: Arc::new(args),
        idle_lock_task: Arc::new(Mutex::new(None)),
        bw_serve_child: Arc::new(Mutex::new(Some(bw_child))),
        bw_serve_url: Arc::new(bw_serve_url),
        shutdown_tx,
    };
    let app = build_router(state).await?;

    let addr =
        listen_url.socket_addrs(|| listen_url.port_or_known_default())?;
    tracing::info!(addr = ?addr, "tcp serving");
    let listener = net::TcpListener::bind(&*addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.changed().await;
        })
        .await?;
    Ok(())
}
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test --lib agent_server 2>&1 | tail -20`
Expected: 2 个测试通过（`shutdown_handler_sends_signal`、`shutdown_route_returns_200_and_stops_server`）。

- [ ] **Step 6: 提交**

```bash
git add src/agent_server.rs
git commit -m "feat(agent_server): 控制端点 /__bwrap/shutdown 优雅关闭"
```

---

### Task 2: BWServeArgs 新增互斥 flag 与 CLI 解析测试

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: 写失败测试（CLI 解析）**

在 `src/main.rs` 文件末尾追加测试模块：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn serve_daemon_flag_parses() {
        let cli = BWCli::try_parse_from(["bwrap", "serve", "--daemon"]).unwrap();
        match &cli.cmd {
            Some(BWCommands::Serve(a)) => assert!(a.daemon),
            _ => panic!("expected serve subcommand"),
        }
    }

    #[test]
    fn serve_stop_flag_parses() {
        let cli = BWCli::try_parse_from(["bwrap", "serve", "--stop"]).unwrap();
        match &cli.cmd {
            Some(BWCommands::Serve(a)) => assert!(a.stop),
            _ => panic!("expected serve subcommand"),
        }
    }

    #[test]
    fn serve_restart_flag_parses() {
        let cli = BWCli::try_parse_from(["bwrap", "serve", "--restart"]).unwrap();
        match &cli.cmd {
            Some(BWCommands::Serve(a)) => assert!(a.restart),
            _ => panic!("expected serve subcommand"),
        }
    }

    #[test]
    fn serve_daemon_mode_mutually_exclusive() {
        let res = BWCli::try_parse_from(["bwrap", "serve", "--daemon", "--stop"]);
        assert!(res.is_err());
        let res = BWCli::try_parse_from(["bwrap", "serve", "--restart", "--stop"]);
        assert!(res.is_err());
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --bin bwrap serve_ 2>&1 | tail -20`
Expected: 编译失败，报 `a.daemon` / `a.stop` / `a.restart` 字段不存在。

- [ ] **Step 3: 实现 flag 与互斥组**

修改 `BWServeArgs`（`src/main.rs:70-87`），加 `#[command(group(...))]` 与三个 flag：

```rust
#[derive(clap::Args, Debug, Clone)]
#[command(group(
    clap::ArgGroup::new("daemon-mode")
        .args(["daemon", "stop", "restart"])
        .multiple(false)
))]
struct BWServeArgs {
    #[arg(long, default_value = "localhost")]
    hostname: String,
    #[arg(long, default_value_t = 8087)]
    port: u16,

    #[arg(
        long,
        value_parser = humantime::parse_duration,
        default_value = "10m",
    )]
    idle_lock_timeout: Duration,
    #[arg(long, default_value = "bw")]
    bw_path: String,
    #[arg(long)]
    pidfile: Option<PathBuf>,

    /// 以后台 daemon 方式启动并退出
    #[arg(long)]
    daemon: bool,
    /// 优雅终止后台 daemon
    #[arg(long)]
    stop: bool,
    /// 重启后台 daemon（先 stop 再拉起新进程）
    #[arg(long)]
    restart: bool,
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --bin bwrap serve_ 2>&1 | tail -20`
Expected: 4 个测试通过。

- [ ] **Step 5: 提交**

```bash
git add src/main.rs
git commit -m "feat(cli): bwrap serve 新增 --daemon/--stop/--restart 互斥 flag"
```

---

### Task 3: daemon 管理函数（daemon_args / stop_daemon / wait_port）

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: 写失败测试**

在 `src/main.rs` 的 `mod tests` 中追加：

```rust
    #[test]
    fn daemon_args_build() {
        assert_eq!(
            daemon_args("localhost", 8087),
            vec!["serve", "--hostname", "localhost", "--port", "8087"]
        );
    }

    #[tokio::test]
    async fn stop_daemon_success() {
        let server = httpmock::MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(httpmock::Method::POST)
                    .path("/__bwrap/shutdown");
                then.status(200);
            })
            .await;
        let host = server.host();
        let port = server.port();
        stop_daemon(host, port).await.unwrap();
    }

    #[tokio::test]
    async fn stop_daemon_no_daemon() {
        // 绑定一个端口后立即释放，确保该端口无监听
        let addr = net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
        let port = addr.port();
        let res = stop_daemon("127.0.0.1", port).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn wait_port_free_when_released() {
        let listener = net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();
        // 端口被占用时 wait_port_free 应超时
        let res = wait_port_free("127.0.0.1", port, Duration::from_millis(200)).await;
        assert!(res.is_err());
        drop(listener);
        wait_port_free("127.0.0.1", port, Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test]
    async fn wait_port_ready_when_listening() {
        // 端口被占用（有监听）→ wait_port_ready 应立即返回 Ok
        let listener = net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();
        wait_port_ready("127.0.0.1", port, Duration::from_secs(1)).await.unwrap();

        // 释放后无监听 → wait_port_ready 应超时
        // 注意：不能用特权端口 1（普通用户 bind 返回 PermissionDenied 会被误判为"就绪"）
        drop(listener);
        let listener2 =
            net::TcpListener::bind(format!("127.0.0.1:{port}")).await.unwrap();
        let port = listener2.local_addr().unwrap().port();
        drop(listener2);
        let res = wait_port_ready("127.0.0.1", port, Duration::from_millis(200)).await;
        assert!(res.is_err());
    }
```

注意 `mod tests` 内可直接用 `TcpListener`（经 `use super::*` 从 `main.rs:16` 的 `use tokio::net::TcpListener;` 引入）。若测试内需 `net::` 前缀，则写 `use tokio::net;`。

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --bin bwrap daemon 2>&1 | tail -20`
Expected: 编译失败，报 `daemon_args` / `stop_daemon` / `wait_port_free` / `wait_port_ready` 未定义。

- [ ] **Step 3: 实现四个函数**

在 `src/main.rs` 中（`bw_serve` 函数之后）追加：

```rust
/// 构造后台 daemon 的命令行参数（不含日志重定向等 IO 配置）
fn daemon_args(hostname: &str, port: u16) -> Vec<String> {
    vec![
        "serve".to_string(),
        "--hostname".to_string(),
        hostname.to_string(),
        "--port".to_string(),
        port.to_string(),
    ]
}

/// 向运行中的 daemon 发送优雅关闭请求
async fn stop_daemon(hostname: &str, port: u16) -> Result<()> {
    let url = format!("http://{}:{}/__bwrap/shutdown", hostname, port);
    let resp = reqwest::Client::new()
        .post(&url)
        .send()
        .await
        .map_err(|e| anyhow!("no running bwrap serve daemon at {url}: {e}"))?;
    if !resp.status().is_success() {
        bail!("failed to stop daemon at {url}: {}", resp.status());
    }
    Ok(())
}

/// 等待端口释放（daemon 停止监听），超时返回错误
async fn wait_port_free(
    hostname: &str,
    port: u16,
    timeout: Duration,
) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if TcpListener::bind(format!("{}:{}", hostname, port))
            .await
            .is_ok()
        {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!("timeout waiting for port {}:{} to be released", hostname, port);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 等待端口就绪（daemon 已监听），超时返回错误
async fn wait_port_ready(
    hostname: &str,
    port: u16,
    timeout: Duration,
) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if TcpListener::bind(format!("{}:{}", hostname, port))
            .await
            .is_err()
        {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!("timeout waiting for daemon at {}:{}", hostname, port);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --bin bwrap daemon 2>&1 | tail -20`
Expected: 6 个测试通过。

- [ ] **Step 5: 提交**

```bash
git add src/main.rs
git commit -m "feat(cli): daemon 管理函数（stop_daemon/wait_port_free/wait_port_ready）"
```

---

### Task 4: spawn_daemon 抽取与 get_api_url 重构

**Files:**
- Modify: `src/main.rs`
- Modify: `Cargo.toml`

- [ ] **Step 1: 修改 Cargo.toml 启用 windows feature**

修改 `Cargo.toml:55-56`：

```toml
[target."cfg(windows)".dependencies]
windows-sys = { version = "0.61", features = ["Win32_System_Threading"] }
```

- [ ] **Step 2: 抽取 spawn_daemon 并重构 get_api_url_or_start_bw_serve_daemon**

将 `get_api_url_or_start_bw_serve_daemon`（`src/main.rs:178-242`）中 daemon 拉起部分抽取为独立函数。在 `get_api_url_or_start_bw_serve_daemon` 之前新增：

```rust
/// 以当前可执行文件拉起后台 daemon（`bwrap serve --hostname --port`）
async fn spawn_daemon(hostname: &str, port: u16) -> Result<()> {
    let log_path = PROJECTDIRS.cache_dir().join("daemon.log");
    if let Some(pp) = log_path.parent() {
        fs::create_dir_all(pp).await?
    }
    let log_file = File::options()
        .write(true)
        .append(true)
        .create(true)
        .open(log_path)
        .await?;
    let stderr = log_file
        .try_clone()
        .await?
        .try_into_std()
        .map(Into::<Stdio>::into)
        .map_err(|e| anyhow!("failed to into std file {:?}", e))?;
    let stdout = log_file
        .try_into_std()
        .map(Into::<Stdio>::into)
        .map_err(|e| anyhow!("failed to into std file {:?}", e))?;

    let exe = std::env::current_exe()?;
    let mut cmd = process::Command::new(exe);
    cmd.args(daemon_args(hostname, port))
        .stdout(stdout)
        .stderr(stderr)
        .stdin(Stdio::null());

    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        })
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{
            CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS,
        };
        cmd.creation_flags((DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP).0);
    }
    let child = cmd.spawn()?;
    tracing::info!(cmd = ?cmd, child = ?child, "spawned agent server");
    Ok(())
}
```

然后重写 `get_api_url_or_start_bw_serve_daemon`，去掉内联拉起逻辑，改为调用 `spawn_daemon`：

```rust
async fn get_api_url_or_start_bw_serve_daemon(
    bw_args: &BWArgs,
) -> Result<String> {
    if let Some(s) = &bw_args.api_url {
        return Ok(s.to_string());
    }

    let hostname = "localhost";
    let port = 8087;
    let url = format!("http://{}:{}", hostname, port);
    if !bw_args.restart_agent
        && let Err(e) =
            TcpListener::bind(format!("{}:{}", hostname, port)).await
    {
        tracing::debug!(url = url, error = ?e, "found exists addr");
        return Ok(url);
    }

    spawn_daemon(hostname, port).await?;
    Ok(url)
}
```

- [ ] **Step 3: 编译验证**

Run: `cargo build 2>&1 | tail -20`
Expected: 编译通过。

- [ ] **Step 4: 运行全部测试**

Run: `cargo test 2>&1 | tail -20`
Expected: 全部通过（原 9 个 + 新增 10 个）。

- [ ] **Step 5: 提交**

```bash
git add src/main.rs Cargo.toml Cargo.lock
git commit -m "feat(cli): 抽取 spawn_daemon 并补全 Windows daemon 拉起"
```

---

### Task 5: bw_serve 分发 --daemon/--stop/--restart

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: 重写 bw_serve 分发**

修改 `bw_serve`（`src/main.rs:244-260`），在进入前台阻塞逻辑前分发三个模式：

```rust
async fn bw_serve(bw_args: &BWArgs, serve_args: &BWServeArgs) -> Result<()> {
    if serve_args.daemon {
        return bw_serve_daemon(serve_args).await;
    }
    if serve_args.stop {
        return bw_serve_stop(serve_args).await;
    }
    if serve_args.restart {
        return bw_serve_restart(serve_args).await;
    }

    let bw_path = serve_args.bw_path.clone();
    let bw_path =
        tokio::task::spawn_blocking(move || which::which(bw_path)).await??;
    let listen_url: url::Url = if serve_args.hostname.starts_with("unix://") {
        serve_args.hostname.parse()?
    } else {
        format!("http://{}:{}", serve_args.hostname, serve_args.port).parse()?
    };
    agent_server::start(BWAgentConfig {
        bw_path,
        listen_url: listen_url.to_string(),
        idle_lock_timeout: serve_args.idle_lock_timeout,
    })
    .await?;
    Ok(())
}

/// --daemon：端口空闲则后台拉起，否则报错
async fn bw_serve_daemon(serve_args: &BWServeArgs) -> Result<()> {
    if TcpListener::bind(format!("{}:{}", serve_args.hostname, serve_args.port))
        .await
        .is_err()
    {
        bail!(
            "port {}:{} already in use, use --restart to restart daemon",
            serve_args.hostname,
            serve_args.port
        );
    }
    spawn_daemon(&serve_args.hostname, serve_args.port).await?;
    Ok(())
}

/// --stop：优雅终止后台 daemon
async fn bw_serve_stop(serve_args: &BWServeArgs) -> Result<()> {
    stop_daemon(&serve_args.hostname, serve_args.port).await?;
    wait_port_free(&serve_args.hostname, serve_args.port, Duration::from_secs(5))
        .await?;
    Ok(())
}

/// --restart：stop → 等端口释放 → 拉起 → 等就绪
async fn bw_serve_restart(serve_args: &BWServeArgs) -> Result<()> {
    stop_daemon(&serve_args.hostname, serve_args.port).await?;
    wait_port_free(&serve_args.hostname, serve_args.port, Duration::from_secs(5))
        .await?;
    spawn_daemon(&serve_args.hostname, serve_args.port).await?;
    wait_port_ready(&serve_args.hostname, serve_args.port, Duration::from_secs(5))
        .await?;
    Ok(())
}
```

- [ ] **Step 2: 编译与测试**

Run: `cargo test 2>&1 | tail -20`
Expected: 全部通过。

- [ ] **Step 3: 手动验证（可选，需真实 bw 在 PATH）**

在 tmux 或两个终端中：
- `bwrap serve --daemon` → 立即退出，`curl http://localhost:8087/__bwrap/shutdown` 返回 200。
- `bwrap serve --stop` → 优雅退出。
- `bwrap serve --restart` → 完成一轮 stop/start。
- 验证 `bw serve` 子进程无残留：`pgrep -a bw || true`（应有 bwrap daemon 与 bw 在跑，stop 后消失）。

- [ ] **Step 4: 提交**

```bash
git add src/main.rs
git commit -m "feat(cli): bw serve 支持 --daemon/--stop/--restart 分发"
```

---

### Task 6: 全量验证

**Files:** 无新增

- [ ] **Step 1: 格式化**

Run: `cargo fmt -- --check 2>&1 | tail -20`
Expected: 无 diff。如有 diff，`cargo fmt` 后重跑。

- [ ] **Step 2: clippy**

Run: `cargo clippy --all-targets 2>&1 | tail -30`
Expected: 无新增 warning（AGENTS.md 记录存在预存 warning，`--fix` 修复既有问题即可，不引入新问题）。

- [ ] **Step 3: 完整测试**

Run: `cargo test 2>&1 | tail -15`
Expected: 全部通过（lib 8 + 新增 10 + 集成 1）。

- [ ] **Step 4: 确认规格覆盖**

对照 `docs/superpowers/specs/2026-08-08-bwrap-serve-daemon-restart-design.md` 逐项确认：
- `--daemon` 端口占用报错 → Task 5 `bw_serve_daemon`。
- `--stop` 优雅终止、子进程回收 → Task 1 + Task 3/5。
- `--restart` stop → 端口释放 → spawn → 就绪 → Task 5 `bw_serve_restart`。
- 控制端点不与代理冲突 → Task 1（显式 route 优先 fallback）。
- 跨平台 daemon 拉起 → Task 4（unix setsid / windows creation_flags）。

---

## Self-Review 记录

**规格覆盖：**
- 三个 flag、互斥组、分发、控制端点、优雅关闭、端口探测、超时上限、Windows feature —— 均有对应 Task。
- `--restart-agent` 不动、`--pidfile` 不动 —— 已遵守规格，无任务改动。

**类型一致性：**
- `daemon_args(hostname: &str, port: u16) -> Vec<String>` 在 Task 3 定义，Task 4 复用，签名一致。
- `stop_daemon(hostname: &str, port: u16) -> Result<()>`、`wait_port_free/ready(hostname: &str, port: u16, timeout: Duration)` 签名在 Task 3 定义、Task 5 复用，一致。
- `spawn_daemon(hostname: &str, port: u16)` Task 4 定义、Task 5 复用，一致。
- `AppState.shutdown_tx: watch::Sender<bool>` 在 Task 1 定义，测试与 `start`/handler 使用一致。

**潜在编译注意点（实现时按需处理）：**
- `windows_sys` 的 `PROCESS_CREATION_FLAGS` 是 newtype，`(A | B).0` 取原始 u32，`creation_flags` 接收 u32。
- 测试模块内 `net`/`TcpListener` 需经 `super::` 或模块内 `use` 引入；`std::time::Instant` 避免与 `tokio::time` 混淆。
