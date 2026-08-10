use std::cell::LazyCell;
use std::fmt::Debug;
use std::io::IsTerminal;
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
use tokio::net::{TcpListener, ToSocketAddrs};
use tokio::process;
#[allow(unused_imports)]
use tracing::{debug, error, info, instrument, trace, warn};
use tracing_subscriber::util::SubscriberInitExt;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = BWCli::parse();

    let log_writer = std::io::stderr();
    let is_ansi = log_writer.is_terminal();
    let (non_blk_io, _guard) =
        tracing_appender::non_blocking(log_writer);
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(non_blk_io)
        .with_ansi(is_ansi)
        .finish()
        .try_init()?;

    use BWCommands::*;
    match &cli.cmd {
        Some(Get(get_args)) => bw_get(&cli.bw_args, get_args).await,
        Some(List(args)) => bw_list(&cli.bw_args, args).await,
        Some(Status) => bw_status(&cli.bw_args).await,
        Some(Serve(serve_args)) => bw_serve(&cli.bw_args, serve_args).await,
        None => Ok(()),
    }
    .inspect_err(
        |e| tracing::error!(error = %e, cli = ?cli, "failed to run cmd"),
    )?;
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

    // NOTE: 私有选项
    #[arg(long, global = true, default_value = "http://localhost:8087")]
    api_url: url::Url,
}

#[derive(clap::Args, Debug, Clone)]
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
async fn spawn_daemon(hostname: &str, port: u16) -> Result<process::Child> {
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
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    let child = cmd.spawn()?;
    tracing::info!(cmd = ?cmd, child = ?child, "spawned agent server");
    Ok(child)
}

async fn get_api_url_or_start_bw_serve_daemon(
    bw_args: &BWArgs,
) -> Result<String> {
    let url = &bw_args.api_url;
    let hostname = url
        .host_str()
        .ok_or_else(|| anyhow!("not found host in {url}"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("not found port in {url}"))?;
    let url = url.to_string();
    // 如果 addr 被使用则跳过
    if addr_in_use((hostname, port)).await? {
        return Ok(url);
    }

    // 启动 serve
    start_bw_serve_daemon(hostname, port).await?;
    Ok(url)
}

async fn start_bw_serve_daemon(hostname: &str, port: u16) -> Result<()> {
    let mut child = spawn_daemon(hostname, port).await?;
    wait_tcp_port((hostname, port), false, Duration::from_secs(2))
        .await
        // 超时时检查后台进程是否存在
        .map_err(|e| match child.try_wait() {
            Ok(Some(s)) => e.context(format!("daemon exited status={}", s)),
            Err(e1) => e.context(e1),
            Ok(None) => e,
        })?;
    Ok(())
}

async fn bw_serve(_bw_args: &BWArgs, serve_args: &BWServeArgs) -> Result<()> {
    if serve_args.stop {
        return bw_serve_stop(serve_args).await;
    }
    // 检查是否解锁
    let name = "BW_SESSION";
    std::env::var(name).map_err(|e| anyhow!("{} {} for to unlock", name, e))?;

    if serve_args.daemon {
        return bw_serve_daemon(serve_args).await;
    }

    if serve_args.restart
        && let Err(e) = bw_serve_stop(serve_args).await
    {
        tracing::info!(error = %e, "bw serve stopped")
    }

    let bw_path = tokio::task::spawn_blocking({
        let bw_path = serve_args.bw_path.clone();
        move || {
            // 检查 bw bin 路径，如果与当前 bwrap bin
            // 重命名的文件路径一致则使用另一个 bw
            let exe = std::env::current_exe()?;
            let mut it = which::which_all(&bw_path)?;
            it.next()
                .and_then(|p| if p == exe { it.next() } else { Some(p) })
                .ok_or_else(|| anyhow!("not found bin {}", bw_path))
        }
    })
    .await??;
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

async fn addr_in_use(addr: impl ToSocketAddrs + Debug) -> Result<bool> {
    let res = TcpListener::bind(&addr)
        .await
        .map(|_| false)
        .or_else(|e| {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                Ok(true)
            } else {
                Err(e)
            }
        })
        .map_err(Into::into);
    tracing::trace!(result = ?res, addr = ?addr, "addr in use");
    res
}

/// --daemon：端口空闲则后台拉起，否则报错
async fn bw_serve_daemon(serve_args: &BWServeArgs) -> Result<()> {
    if serve_args.restart
        && let Err(e) = bw_serve_stop(serve_args).await
    {
        tracing::info!(error = %e, "failed to stopping bw serve when try restart");
    }
    // 检查端口是否空闲
    if addr_in_use((&*serve_args.hostname, serve_args.port)).await? {
        bail!(
            "port {}:{} already in use, use --restart to restart daemon",
            serve_args.hostname,
            serve_args.port
        )
    }
    start_bw_serve_daemon(&serve_args.hostname, serve_args.port).await?;
    Ok(())
}

/// --stop：优雅终止后台 daemon
async fn bw_serve_stop(serve_args: &BWServeArgs) -> Result<()> {
    stop_daemon(&serve_args.hostname, serve_args.port).await?;
    wait_tcp_port(
        (&*serve_args.hostname, serve_args.port),
        true,
        Duration::from_secs(2),
    )
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
    trace!(url = url, "sending shutdown request");
    let resp = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()?
        .post(&url)
        .send()
        .await
        .map_err(|e| anyhow!("no running bwrap serve daemon at {url}: {e}"))?;
    if !resp.status().is_success() {
        bail!("failed to stop daemon at {url}: {}", resp.status());
    }
    Ok(())
}

async fn wait_tcp_port(
    addr: impl ToSocketAddrs + Debug,
    free_or_ready: bool,
    timeout: Duration,
) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    let interval = Duration::from_millis(100);
    loop {
        if addr_in_use(&addr).await.map(|used| free_or_ready != used)? {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "timeout waiting for port {:?} to be {}",
                addr,
                if free_or_ready { "released" } else { "used" }
            );
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

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
        // httpmock 对未 mock 的路径返回 404，确定性模拟"端口上有服务但不是 bwrap daemon"
        // 注意不能用 bind 后立即释放的方式模拟"无监听"：并发测试下端口可能被
        // 其他测试（如 stop_daemon_success 的 mock server）复用，导致偶发竞态失败。
        let server = httpmock::MockServer::start_async().await;
        let host = server.host();
        let port = server.port();
        let res = stop_daemon(&host, port).await;
        assert!(res.is_err());
    }
}
