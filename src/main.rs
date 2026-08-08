use std::cell::LazyCell;
use std::fmt::Debug;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use bwrap::agent_server::BWAgentConfig;
use bwrap::bwserve_api::{BWGetArgs, BwListArgs};
use bwrap::{agent_server, bwserve_api};
use clap::{Parser, Subcommand};
use directories::ProjectDirs;
use sonic_rs::to_string as ser_to_json;
use tokio::fs::{self, File};
use tokio::io::{self, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process;
#[allow(unused_imports)]
use tracing::{debug, error, info, instrument, trace, warn};
use tracing_subscriber::util::SubscriberInitExt;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = BWCli::parse();
    // let (non_blk_io, _guard) = tracing_appender::non_blocking(std::io::stderr());
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        // .with_writer(non_blk_io)
        .finish()
        .try_init()?;
    use BWCommands::*;
    match &cli.cmd {
        Some(Get(get_args)) => bw_get(&cli.bw_args, get_args).await?,
        Some(List(args)) => bw_list(&cli.bw_args, args).await?,
        Some(Status) => bw_status(&cli.bw_args).await?,
        Some(Serve(serve_args)) => bw_serve(&cli.bw_args, serve_args).await?,
        None => {}
    }
    Ok(())
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct BWCli {
    #[command(flatten)]
    bw_args: BWArgs,
    #[command(subcommand)]
    cmd: Option<BWCommands>,
}

#[derive(clap::Args, Debug, Clone)]
struct BWArgs {
    #[arg(long, global = true)]
    session: Option<String>,
    #[arg(long, global = true)]
    raw: bool,
    #[arg(long, global = true)]
    nointeraction: bool,

    /// NOTE: 私有选项
    /// 支持
    /// - http://localhost[:$port]/
    /// - unix://[localhost[:$port]]/path/to/file.socket
    #[arg(long)]
    api_url: Option<String>,
    #[arg(long)]
    restart_agent: bool,
}

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

#[derive(Subcommand, Debug)]
enum BWCommands {
    Get(BWGetArgs),
    List(BwListArgs),
    Serve(BWServeArgs),
    Status,
}

async fn write_str(
    mut w: impl AsyncWriteExt + Unpin,
    s: impl AsRef<str>,
) -> Result<()> {
    w.write_all(s.as_ref().as_bytes()).await?;
    w.write_u8(b'\n').await?;
    w.flush().await?;
    Ok(())
}

async fn handle_resp_err<T: Debug>(
    resp_data: &bwserve_api::BWServeResp<T>,
) -> Result<bool> {
    if resp_data.success {
        return Ok(false);
    }

    let Some(msg) = &resp_data.message else {
        bail!("invalid response data: {:?}", resp_data)
    };
    // 原 bw get 只需要输出 mes 即可
    write_str(io::stderr(), msg).await?;
    Ok(true)
}

async fn bw_get(bw_args: &BWArgs, get_args: &BWGetArgs) -> Result<()> {
    let api = bwserve_api::BWServeApi::new(
        &get_api_url_or_start_bw_serve_daemon(bw_args).await?,
    )?;
    let resp_data = api.get(get_args).await?;
    if handle_resp_err(&resp_data).await? {
        return Ok(());
    }

    use bwserve_api::BWServeGetRespData::*;
    let s = match resp_data.data {
        Some(Item(item)) => ser_to_json(&item)?,
        Some(ItemProp { object: _, data }) => data,
        d => bail!("Unsupported data: {:?}", d),
    };
    write_str(io::stdout(), s).await?;
    Ok(())
}

async fn bw_list(bw_args: &BWArgs, list_args: &BwListArgs) -> Result<()> {
    let api = bwserve_api::BWServeApi::new(
        &get_api_url_or_start_bw_serve_daemon(bw_args).await?,
    )?;
    let resp_data = api.list(list_args).await?;
    if handle_resp_err(&resp_data).await? {
        return Ok(());
    }

    let Some(data) = &resp_data.data else {
        bail!("invalid response: {:?}", resp_data)
    };
    let s = ser_to_json(&data.data)?;
    write_str(io::stdout(), s).await?;
    Ok(())
}

async fn bw_status(bw_args: &BWArgs) -> Result<()> {
    let api = bwserve_api::BWServeApi::new(
        &get_api_url_or_start_bw_serve_daemon(bw_args).await?,
    )?;
    let resp_data = api.status().await?;
    if handle_resp_err(&resp_data).await? {
        return Ok(());
    }

    let Some(data) = &resp_data.data else {
        bail!("invalid response: {:?}", resp_data)
    };
    write_str(io::stdout(), ser_to_json(&data.template)?).await?;
    Ok(())
}

const PROJECTDIRS: LazyCell<ProjectDirs> = LazyCell::new(|| {
    ProjectDirs::from("xyz", "navyd", env!("CARGO_BIN_NAME"))
        .expect("not found project dirs")
});
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

async fn bw_serve(_bw_args: &BWArgs, serve_args: &BWServeArgs) -> Result<()> {
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
            bail!(
                "timeout waiting for port {}:{} to be released",
                hostname,
                port
            );
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use tokio::net;

    #[test]
    fn serve_daemon_flag_parses() {
        let cli =
            BWCli::try_parse_from(["bwrap", "serve", "--daemon"]).unwrap();
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
        let cli =
            BWCli::try_parse_from(["bwrap", "serve", "--restart"]).unwrap();
        match &cli.cmd {
            Some(BWCommands::Serve(a)) => assert!(a.restart),
            _ => panic!("expected serve subcommand"),
        }
    }

    #[test]
    fn serve_daemon_mode_mutually_exclusive() {
        let res =
            BWCli::try_parse_from(["bwrap", "serve", "--daemon", "--stop"]);
        assert!(res.is_err());
        let res =
            BWCli::try_parse_from(["bwrap", "serve", "--restart", "--stop"]);
        assert!(res.is_err());
    }

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
        stop_daemon(&host, port).await.unwrap();
    }

    #[tokio::test]
    async fn stop_daemon_no_daemon() {
        // 绑定一个端口后立即释放，确保该端口无监听
        let addr = net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();
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
        let res =
            wait_port_free("127.0.0.1", port, Duration::from_millis(200)).await;
        assert!(res.is_err());
        drop(listener);
        wait_port_free("127.0.0.1", port, Duration::from_secs(1))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn wait_port_ready_when_listening() {
        // 端口被占用（有监听）→ wait_port_ready 应立即返回 Ok
        let listener = net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();
        wait_port_ready("127.0.0.1", port, Duration::from_secs(1))
            .await
            .unwrap();

        // 释放后无监听 → wait_port_ready 应超时
        // 注意：不能用特权端口 1（普通用户 bind 返回 PermissionDenied 会被误判为"就绪"）
        drop(listener);
        let listener2 = net::TcpListener::bind(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        let port = listener2.local_addr().unwrap().port();
        drop(listener2);
        let res =
            wait_port_ready("127.0.0.1", port, Duration::from_millis(200))
                .await;
        assert!(res.is_err());
    }
}
