use std::any::Any;
use std::cell::LazyCell;
use std::fmt::Debug;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::{ExitCode, ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bwrap::agent_server::BWAgentConfig;
use bwrap::bwserve_api::{BWGetArgs, BwListArgs};
use bwrap::{agent_server, bwserve_api};
use clap::{CommandFactory, Parser, Subcommand};
use directories::ProjectDirs;
use rustix::path::Arg;
use sonic_rs::to_string as ser_to_json;
use tokio::fs::{self, File};
use tokio::io::{self, AsyncWriteExt};
use tokio::net::{TcpListener, ToSocketAddrs};
use tokio::process;
#[allow(unused_imports)]
use tracing::{debug, error, info, instrument, trace, warn};
use tracing_subscriber::util::SubscriberInitExt;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = BWCli::parse();

    let log_writer = std::io::stderr();
    let is_ansi = log_writer.is_terminal();
    let (non_blk_io, _guard) = tracing_appender::non_blocking(log_writer);
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(non_blk_io)
        .with_ansi(is_ansi)
        .finish()
        .init();

    use BWCommands::*;
    let res = match &cli.cmd {
        Some(Get(get_args)) => bw_get(&cli.bw_args, get_args).await,
        Some(List(args)) => bw_list(&cli.bw_args, args).await,
        Some(Status) => bw_status(&cli.bw_args).await,
        Some(Serve(serve_args)) => bw_serve(&cli.bw_args, serve_args).await,
        Some(Unlock(unlock_args)) => bw_unlock(&cli.bw_args, unlock_args).await,
        None => Ok(()),
    };
    let Err(e) = res else {
        return ExitCode::SUCCESS;
    };

    tracing::error!(error = %e, cli = ?cli, "failed to run cmd");
    // 保证输出和 exitcode 与原 bw 一致
    let (code, emsg) = e
        .downcast::<BWCliError>()
        .map(|cli_err| match cli_err {
            BWCliError::Msg(s) => (1, Some(s)),
            BWCliError::Follow(exit_status) => {
                (exit_status.code().unwrap_or(255) as u8, None)
            }
        })
        .unwrap_or_else(|e| (255, Some(e.to_string())));
    if let Some(emsg) = emsg {
        write_str(io::stderr(), emsg)
            .await
            .expect("write str error");
    }
    code.into()
}

#[derive(Debug, thiserror::Error)]
enum BWCliError {
    #[error("{0}")]
    Msg(String),
    #[error("{0}")]
    Follow(ExitStatus),
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
    #[arg(long, default_value = "bw")]
    bw_path: String,
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

#[derive(clap::Args, Debug, Clone)]
struct BWUnlockArgs {
    #[arg(long)]
    check: bool,
    #[arg(long)]
    passwordenv: Option<String>,
    #[arg(long)]
    passwordfile: Option<String>,

    password: Option<String>,
}

#[derive(Subcommand, Debug, strum::Display)]
// 注意： clap subcommand 默认的命令规则是 kebab-case
// 参考 `#[command(rename_all = "snake_case")]`
#[strum(serialize_all = "kebab-case")]
enum BWCommands {
    Get(BWGetArgs),
    List(BwListArgs),
    Serve(BWServeArgs),
    Status,
    Unlock(BWUnlockArgs),
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

trait RespErrHandler<T> {
    fn handle_resp_err(self) -> Result<bwserve_api::BWServeResp<T>>;
}
impl<T: Debug> RespErrHandler<T> for Result<bwserve_api::BWServeResp<T>> {
    fn handle_resp_err(self) -> Result<bwserve_api::BWServeResp<T>> {
        let resp_data = self?;
        if resp_data.success {
            return Ok(resp_data);
        }

        let Some(msg) = resp_data.message else {
            bail!("invalid response data: {:?}", resp_data)
        };
        Err(BWCliError::Msg(msg).into())
    }
}

async fn bw_get(bw_args: &BWArgs, get_args: &BWGetArgs) -> Result<()> {
    let api = bwserve_api::BWServeApi::new(
        &get_api_url_or_start_bw_serve_daemon(bw_args).await?,
    )?;
    let resp_data = api.get(get_args).await.handle_resp_err()?;

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
    let resp_data = api.list(list_args).await.handle_resp_err()?;

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
    let resp_data = api.status().await.handle_resp_err()?;

    let Some(data) = &resp_data.data else {
        bail!("invalid response: {:?}", resp_data)
    };
    write_str(io::stdout(), ser_to_json(&data.template)?).await?;
    Ok(())
}

macro_rules! field {
    ($obj:ident.$field:ident) => {
        (stringify!($field), $obj.$field)
    };
    (&$obj:ident.$field:ident) => {
        (stringify!($field), &$obj.$field)
    };
}
/// 通过 struct 字段名获取 clap 配置的选项名如 struct.config -> --config|-c
fn get_clap_opt(c: &clap::Command, field_name: &str) -> Result<String> {
    c.get_arguments()
        .find(|a| a.get_id() == field_name)
        .and_then(|a| {
            a.get_long()
                .map(|n| format!("--{}", n))
                .or_else(|| a.get_short().map(|n| format!("-{}", n)))
        })
        .ok_or_else(|| anyhow!("not found arg with id={}", field_name))
}

struct ClapSubCmdName {
    cmd: clap::Command,
    subcmd: clap::Command,
    subcmd_name: String,
}
/// 原 command().try_get_matches()? 会读取 env::args_os()
/// 分离避免测试时不存在会导致 panic
fn get_clap_subcommand() -> Result<ClapSubCmdName> {
    let cmd = BWCli::command();
    let subcmd_name = cmd
        .clone()
        .try_get_matches()?
        .subcommand_name()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("not found subcommand name"))?;
    cmd.find_subcommand(&subcmd_name)
        .cloned()
        .map(|subcmd| ClapSubCmdName {
            cmd,
            subcmd,
            subcmd_name,
        })
        .ok_or_else(|| anyhow!("not found subcommand"))
}

/// 构建 bw unlock 列表参数
async fn get_bw_unlock_cmd_args(
    bw_args: &BWArgs,
    unlock_args: &BWUnlockArgs,
    clapcmd: ClapSubCmdName,
) -> Result<Vec<String>> {
    let mut cmd_args = vec![
        find_real_bw(&bw_args.bw_path)
            .await?
            .to_string_lossy()
            .to_string(),
        clapcmd.subcmd_name,
    ];
    if let Some(pw) = unlock_args.password.to_owned() {
        cmd_args.push(pw);
    }

    let mut extend_args = |(name, val): (&str, &dyn Any)| -> Result<()> {
        // 从子命令或父命令中获取选项
        let opt_name = get_clap_opt(&clapcmd.subcmd, name)
            .or_else(|e| get_clap_opt(&clapcmd.cmd, name).context(e))?;
        if let Some(v) = val.downcast_ref::<bool>() {
            if *v {
                cmd_args.push(opt_name);
            }
            return Ok(());
        }
        if let Some(val) = val.downcast_ref::<Option<String>>() {
            if let Some(val) = val.to_owned() {
                cmd_args.push(opt_name);
                cmd_args.push(val);
            }
            return Ok(());
        }
        bail!("Unsupported type of arg={:?}", val);
    };
    // 编译期保证不会出现错误
    extend_args(field!(&unlock_args.check))?;
    extend_args(field!(&unlock_args.passwordenv))?;
    extend_args(field!(&unlock_args.passwordfile))?;
    extend_args(field!(&bw_args.raw))?;
    Ok(cmd_args)
}

async fn bw_unlock(bw_args: &BWArgs, unlock_args: &BWUnlockArgs) -> Result<()> {
    let cmd_args =
        get_bw_unlock_cmd_args(bw_args, unlock_args, get_clap_subcommand()?)
            .await?;
    info!(cmd_args = ?cmd_args, "spawning command");
    let s = process::Command::new(&cmd_args[0])
        .args(&cmd_args[1..])
        // .stdout(Stdio::piped())
        .spawn()?
        .wait()
        .await?;
    debug!(status = ?s, "got status");
    if !s.success() {
        return Err(BWCliError::Follow(s).into());
    }

    // 解锁后停止后台进程
    let cli_args = ["bw", "serve", "--stop"];
    debug!(cli_args = ?cli_args, "parsing cli for bw serve daemon");
    let cli = BWCli::parse_from(cli_args);
    trace!(cli = ?cli, "bw serve args parsed");
    let Some(BWCommands::Serve(serve_args)) = &cli.cmd else {
        bail!("failed to parse serve args: {:?}", cli_args);
    };
    if let Err(e) = bw_serve_stop(serve_args).await {
        info!(error = %e, "failed to stopping bw serve");
    }
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
            rustix::process::setsid().map(|_| ()).map_err(Into::into)
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

async fn bw_serve(bw_args: &BWArgs, serve_args: &BWServeArgs) -> Result<()> {
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

    let listen_url: url::Url = if serve_args.hostname.starts_with("unix://") {
        serve_args.hostname.parse()?
    } else {
        format!("http://{}:{}", serve_args.hostname, serve_args.port).parse()?
    };
    agent_server::start(BWAgentConfig {
        bw_path: find_real_bw(&bw_args.bw_path).await?,
        listen_url: listen_url.to_string(),
        idle_lock_timeout: serve_args.idle_lock_timeout,
    })
    .await?;
    Ok(())
}

async fn find_real_bw(path: impl Into<String>) -> Result<PathBuf> {
    let path = path.into();
    tokio::task::spawn_blocking(move || {
        // let path = path.into();
        // 检查 bw bin 路径，如果与当前 bwrap bin
        // 重命名的文件路径一致则使用另一个 bw
        let exe = std::env::current_exe()?;
        let mut it = which::which_all(&path)?;
        it.next()
            .and_then(|p| if p == exe { it.next() } else { Some(p) })
            .ok_or_else(|| anyhow!("not found bin {}", path))
    })
    .await?
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
        tracing::info!(
            error = %e, "failed to stopping bw serve when try restart"
        );
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
    use rstest::rstest;
    use similar_asserts::assert_eq;

    #[rstest]
    #[case(&["/bw", "unlock"])]
    #[case(&["/bw", "unlock", "--raw"])]
    #[case(&["/bw", "unlock", "--raw", "--passwordfile", "/a/b.file"])]
    #[case(&["/bw", "unlock", "--raw", "somepw"])]
    #[tokio::test]
    async fn get_bw_unlock_cmd_args_test(#[case] args: &[&str]) {
        let cli = BWCli::parse_from(args);
        let Some(BWCommands::Unlock(unlock_args)) = &cli.cmd else {
            panic!("invalid subcommand {:?}", cli.cmd);
        };
        // 原 command().try_get_matches()? 会读取 env::args_os()
        // 而测试时不存在会导致 panic
        // get_clap_subcommand()
        let cmd = BWCli::command();
        let subcmd_name = args[1].to_string();
        let subcmd = cmd.find_subcommand(&subcmd_name).cloned().unwrap();
        let cmd_args = get_bw_unlock_cmd_args(
            &cli.bw_args,
            unlock_args,
            ClapSubCmdName {
                cmd,
                subcmd,
                subcmd_name,
            },
        )
        .await
        .unwrap();
        // NOTE: bw path 可能由于本地环境的影响返回实际的路径，所以跳过检查
        assert_eq!(cmd_args.len(), args.len());
        // 返回的参数顺序与原始不一致
        assert!(cmd_args[1..].iter().all(|e| args.contains(&e.as_str())));
    }

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
