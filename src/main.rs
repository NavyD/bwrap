use std::any::Any;
use std::fmt::Debug;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::{ExitCode, ExitStatus, Stdio};
use std::str::FromStr;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bwrap::agent_server::BWAgentConfig;
use bwrap::bwserve_api::{
    BWGetArgs, BwListArgs, DEFAULT_LOCALHOST_IP, DEFAULT_TCP_PORT,
};
use bwrap::{agent_server, bwserve_api};
use clap::{CommandFactory, Parser, Subcommand};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use sonic_rs::{from_str as json_dec, to_string as ser_to_json};
use tokio::fs::{self};
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, ToSocketAddrs};
use tokio::process;
use tracing::level_filters::LevelFilter;
#[allow(unused_imports)]
use tracing::{debug, error, info, instrument, trace, warn};
use url::Url;

#[cfg(all(target_os = "linux", target_env = "musl"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let unknown_code: u8 = 255;
    let Err(e) = run().await else {
        return ExitCode::SUCCESS;
    };

    // 保证输出和 exitcode 与原 bw 一致
    let (code, emsg) = e
        .downcast::<BWCliError>()
        .map(|cli_err| match cli_err {
            BWCliError::Msg(s) => (1, Some(s)),
            BWCliError::Follow(exit_status) => (
                exit_status.code().map(|c| c as u8).unwrap_or(unknown_code),
                None,
            ),
        })
        .unwrap_or_else(|e| (unknown_code, Some(e.to_string())));
    if let Some(emsg) = emsg {
        write_str(io::stderr(), emsg)
            .await
            .expect("write str error");
    }
    code.into()
}

async fn run() -> Result<()> {
    let mut cli = BWCli::try_parse()?;
    trace!(cli = ?cli, "parsed BWCli");

    let (bw_args, cfg_serve_args) = if let Some(c) = &cli.bw_args.daemon_cfg {
        let d = json_dec::<BWDaemonCfg>(c).inspect_err(|e| {
            error!(
                type = std::any::type_name::<BWDaemonCfg>(),
                json_str = c,
                error = %e,
                "parsing json error",
            )
        })?;
        (d.0, Some(d.1))
    } else {
        (cli.bw_args, None)
    };
    cli.bw_args = bw_args;

    let _log_guards = init_log(&cli.bw_args.log_file)?;

    use BWCommands::*;
    match &cli.cmd {
        Some(Get(get_args)) => bw_get(&cli.bw_args, get_args).await,
        Some(List(args)) => bw_list(&cli.bw_args, args).await,
        Some(Status) => bw_status(&cli.bw_args).await,
        Some(Serve(serve_args)) => {
            bw_serve(
                &cli.bw_args,
                cfg_serve_args.as_ref().unwrap_or(serve_args),
            )
            .await
        }
        Some(Unlock(unlock_args)) => bw_unlock(&cli.bw_args, unlock_args).await,
        Some(External(sub_args)) => bw_external(&cli.bw_args, sub_args).await,
        None => Ok(()),
    }
}

fn init_log(
    log_files: &[BWLogFile],
) -> Result<Vec<tracing_appender::non_blocking::WorkerGuard>> {
    use tracing_appender::rolling;
    use tracing_subscriber::Registry;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{EnvFilter, fmt};

    let mut guards = vec![];
    let mut layers = vec![];
    for log_file in log_files {
        let layer = fmt::layer::<Registry>();
        let (log_layer, guard) = match log_file {
            BWLogFile::Stdout => {
                let f = std::io::stdout();
                let is_ansi = f.is_terminal();
                let (nio, guard) = tracing_appender::non_blocking(f);
                let layer = layer.pretty().with_ansi(is_ansi).with_writer(nio);
                (layer.boxed(), guard)
            }
            BWLogFile::Stderr => {
                let f = std::io::stderr();
                let is_ansi = f.is_terminal();
                let (nio, guard) = tracing_appender::non_blocking(f);
                let layer = layer.pretty().with_ansi(is_ansi).with_writer(nio);
                (layer.boxed(), guard)
            }
            BWLogFile::File(p) => {
                let appender = rolling::Builder::default()
                    .rotation(rolling::Rotation::DAILY)
                    .filename_prefix(
                        p.file_stem()
                            .and_then(|s| s.to_str())
                            .context("not found file stem")?,
                    )
                    .filename_suffix(
                        p.extension()
                            .map(|s| s.to_str().context("path to str error"))
                            .unwrap_or(Ok::<_, anyhow::Error>("log"))?,
                    )
                    .max_log_files(14)
                    .build(p.parent().context("not found parent")?)?;
                let (nio, guard) = tracing_appender::non_blocking(appender);
                let layer = layer.with_ansi(false).with_writer(nio);
                (layer.boxed(), guard)
            }
        };

        layers.push(log_layer);
        guards.push(guard);
    }
    Registry::default()
        .with(layers)
        .with(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::OFF.into())
                .from_env_lossy(),
        )
        .try_init()?;
    Ok(guards)
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

#[derive(Clone, Debug, Deserialize, Serialize)]
enum BWLogFile {
    Stdout,
    Stderr,
    File(PathBuf),
}

impl FromStr for BWLogFile {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> std::prelude::v1::Result<Self, Self::Err> {
        let v = match s {
            "stdout" => Self::Stdout,
            "stderr" => Self::Stderr,
            s => Self::File(s.parse()?),
        };
        Ok(v)
    }
}

#[derive(clap::Args, Debug, Clone, Deserialize, Serialize)]
struct BWArgs {
    #[arg(long, global = true)]
    session: Option<String>,
    #[arg(long, global = true)]
    raw: bool,
    #[arg(long, global = true)]
    nointeraction: bool,

    // NOTE: 私有选项
    #[arg(long, global = true, default_value_t = format!(
        "http://{}:{}",
        DEFAULT_LOCALHOST_IP,
        DEFAULT_TCP_PORT
    ))]
    api_url: String,
    #[arg(long, global = true, default_value = "bw")]
    bw_path: String,
    #[arg(long, global = true)]
    bw_serve_url: Option<url::Url>,

    #[arg(long, global = true, default_value = "stderr")]
    log_file: Vec<BWLogFile>,
    /// 用于 spawn daemon 使用重置选项，方便维护通用选项
    #[arg(long, global = true, hide = true)]
    daemon_cfg: Option<String>,
}

#[derive(clap::Args, Debug, Clone, Deserialize, Serialize)]
struct BWServeArgs {
    #[arg(long, default_value = DEFAULT_LOCALHOST_IP)]
    hostname: String,
    #[arg(long, default_value_t = DEFAULT_TCP_PORT)]
    port: u16,

    #[arg(
        long,
        value_parser = humantime::parse_duration,
        default_value = "10m",
    )]
    #[serde(with = "humantime_serde")]
    idle_lock_timeout: Duration,

    #[arg(
        long,
        value_parser = humantime::parse_duration,
        default_value = "2s",
    )]
    #[serde(with = "humantime_serde")]
    wait_port_timeout: Duration,

    /// 以后台 daemon 方式启动并退出
    #[arg(long)]
    daemon: bool,
    /// 优雅终止后台 daemon
    #[arg(long)]
    stop: bool,
    /// 重启后台 daemon（先 stop 再拉起新进程）
    #[arg(long, short = 'R')]
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

    #[command(flatten)]
    serve_args: BWServeArgs,
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
    #[command(external_subcommand)]
    External(Vec<String>),
}

async fn bw_external(bw_args: &BWArgs, sub_args: &[String]) -> Result<()> {
    // NOTE: 必须配置 bw 否则默认使用的是当前 bw 可能导致无限循环
    let bw_path = find_real_bw(&bw_args.bw_path).await?;
    let st = process::Command::new(bw_path)
        .args(sub_args)
        .spawn()
        .with_context(|| format!("bw_args={:?}, args={:?}", bw_args, sub_args))?
        .wait()
        .await?;
    if st.success() {
        return Ok(());
    }
    Err(BWCliError::Follow(st).into())
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
    let api = bwserve_api::BWServeApi::new(&get_api_url(bw_args).await?)?;
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
    let api = bwserve_api::BWServeApi::new(&get_api_url(bw_args).await?)?;
    let resp_data = api.list(list_args).await.handle_resp_err()?;

    let Some(data) = &resp_data.data else {
        bail!("invalid response: {:?}", resp_data)
    };
    let s = ser_to_json(&data.data)?;
    write_str(io::stdout(), s).await?;
    Ok(())
}

async fn bw_status(bw_args: &BWArgs) -> Result<()> {
    let api = bwserve_api::BWServeApi::new(&get_api_url(bw_args).await?)?;
    let resp_data = api.status().await.handle_resp_err()?;

    let Some(data) = &resp_data.data else {
        bail!("invalid response: {:?}", resp_data)
    };
    write_str(io::stdout(), ser_to_json(&data.template)?).await?;
    Ok(())
}

macro_rules! field_kv {
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

/// 构建 bw unlock 列表参数
async fn get_bw_unlock_cmd_args(
    bw_args: &BWArgs,
    unlock_args: &BWUnlockArgs,
) -> Result<Vec<String>> {
    let cmd = BWCli::command();
    let subcmd_name = cmd
        .clone()
        .try_get_matches()?
        .subcommand_name()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("not found subcommand name"))?;
    let subcmd = cmd
        .find_subcommand(&subcmd_name)
        .context("not found subcommand")?;
    let mut cmd_args = vec![
        find_real_bw(&bw_args.bw_path)
            .await?
            .to_string_lossy()
            .to_string(),
        subcmd_name,
    ];
    if let Some(pw) = unlock_args.password.to_owned() {
        cmd_args.push(pw);
    }

    let mut extend_args = |(name, val): (&str, &dyn Any)| -> Result<()> {
        // 从子命令或父命令中获取选项
        let opt_name = get_clap_opt(subcmd, name)
            .or_else(|e| get_clap_opt(&cmd, name).context(e))?;
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
    extend_args(field_kv!(&unlock_args.check))?;
    extend_args(field_kv!(&unlock_args.passwordenv))?;
    extend_args(field_kv!(&unlock_args.passwordfile))?;
    extend_args(field_kv!(&bw_args.raw))?;
    Ok(cmd_args)
}

const BW_SESSION_NAME: &str = "BW_SESSION";

async fn bw_unlock(bw_args: &BWArgs, unlock_args: &BWUnlockArgs) -> Result<()> {
    if unlock_args.serve_args.restart && !bw_args.raw {
        bail!("--restart flag required --raw")
    }

    let cmd_args = get_bw_unlock_cmd_args(bw_args, unlock_args).await?;
    info!(cmd_args = ?cmd_args, "spawning command");

    let output = process::Command::new(&cmd_args[0])
        .args(&cmd_args[1..])
        .stdout(if unlock_args.serve_args.restart {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .spawn()?
        .wait_with_output()
        .await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.trim().is_empty() {
        write_str(io::stdout(), &stdout).await?;
    }
    trace!(output = ?output, "got output");
    if !output.status.success() {
        return Err(BWCliError::Follow(output.status).into());
    }

    let mut serve_args = unlock_args.serve_args.clone();
    if !serve_args.restart && !serve_args.stop {
        return Ok(());
    }
    // 当指定 restart 时默认为 daemon
    if serve_args.restart && !serve_args.daemon {
        serve_args.daemon = true;
    }

    // SAFETY: 单线程安全性
    unsafe {
        std::env::set_var(BW_SESSION_NAME, stdout.to_string());
    }
    if let Err(e) = bw_serve(bw_args, &serve_args).await {
        info!(error = %e, "failed to stopping bw serve");
    }
    Ok(())
}

static PROJECT_DIRS: LazyLock<ProjectDirs> = LazyLock::new(|| {
    ProjectDirs::from("xyz", "navyd", env!("CARGO_BIN_NAME"))
        .expect("not found project dirs")
});

#[derive(Deserialize, Serialize, Debug, Clone)]
struct BWDaemonCfg(BWArgs, BWServeArgs);

/// 以当前可执行文件拉起后台 daemon（`bwrap serve --hostname --port`）
async fn spawn_daemon(
    bw_args: &BWArgs,
    serve_args: &BWServeArgs,
) -> Result<process::Child> {
    let log_path = PROJECT_DIRS.cache_dir().join("bw-serve-daemon.log");
    if let Some(pp) = log_path.parent() {
        fs::create_dir_all(pp).await?
    }

    let mut bw_args = bw_args.clone();
    bw_args.log_file = vec![BWLogFile::Stderr, BWLogFile::File(log_path)];
    bw_args.daemon_cfg = None;
    let mut serve_args = serve_args.clone();
    serve_args.daemon = false;
    serve_args.restart = false;
    serve_args.stop = false;
    let daemon_cfg = ser_to_json(&BWDaemonCfg(bw_args, serve_args))?;

    let exe = std::env::current_exe()?;
    let mut cmd = process::Command::new(exe);
    cmd.args(["serve", "--daemon-cfg", &daemon_cfg])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());

    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            rustix::process::setsid().map(|_| ()).map_err(Into::into)
        })
    };
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
        };
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    let child = cmd.spawn()?;
    info!(cmd = ?cmd, child = ?child, "spawned agent server");
    Ok(child)
}

async fn get_api_url(bw_args: &BWArgs) -> Result<String> {
    let url = &bw_args.api_url.parse::<Url>()?;
    let hostname = url
        .host_str()
        .ok_or_else(|| anyhow!("not found host in {url}"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("not found port in {url}"))?;
    let url = url.to_string();
    // 如果 addr 被使用则跳过
    if !addr_in_use((hostname, port)).await? {
        bail!("Please start the `bw serve` service")
    }
    Ok(url)
}

async fn start_bw_serve_daemon(
    bw_args: &BWArgs,
    serve_args: &BWServeArgs,
) -> Result<()> {
    let mut child = spawn_daemon(bw_args, serve_args).await?;

    let Err(e) = wait_tcp_port(
        (serve_args.hostname.as_str(), serve_args.port),
        false,
        serve_args.wait_port_timeout,
    )
    .await
    else {
        return Ok(());
    };

    let mut stderr_str = None;
    if let Some(mut stderr) = child.stderr.take() {
        let mut s = String::new();
        stderr.read_to_string(&mut s).await?;
        error!(stderr = s, "readded stderr");
        stderr_str = Some(s);
    }
    let mut stdout_str = None;
    if let Some(mut stdout) = child.stdout.take() {
        let mut s = String::new();
        stdout.read_to_string(&mut s).await?;
        error!(stdout = s, "readded stdout");
        stdout_str = Some(s);
    }
    Err(match child.try_wait() {
        Ok(Some(s)) => e.context(format!(
            "daemon exited status={}, stdout={:?}, stderr={:?}",
            s, stdout_str, stderr_str
        )),
        Err(e1) => e.context(e1),
        Ok(None) => e,
    })
}

async fn bw_serve(bw_args: &BWArgs, serve_args: &BWServeArgs) -> Result<()> {
    if serve_args.stop {
        return bw_serve_stop(serve_args).await;
    }
    // 检查是否解锁
    std::env::var(BW_SESSION_NAME)
        .map_err(|e| anyhow!("{} {} for to unlock", BW_SESSION_NAME, e))?;

    if serve_args.daemon {
        return bw_serve_daemon(bw_args, serve_args).await;
    }

    if serve_args.restart
        && let Err(e) = bw_serve_stop(serve_args).await
    {
        info!(error = %e, "bw serve stopped")
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
        bw_serve_url: bw_args.bw_serve_url.clone(),
    })
    .await?;
    Ok(())
}

async fn find_real_bw(path: impl Into<String>) -> Result<PathBuf> {
    let path = path.into();
    tokio::task::spawn_blocking(move || {
        // 检查 bw bin 路径，如果与当前 bwrap bin
        // 重命名的文件路径一致则使用另一个 bw
        let exe = std::env::current_exe()?.canonicalize()?;
        which::which_all(&path)?
            .find(|p| p.canonicalize().as_ref().unwrap_or(p) != &exe)
            .ok_or_else(|| {
                anyhow!(
                    "not found bin {} filter by exe {}",
                    path,
                    exe.display()
                )
            })
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
    trace!(result = ?res, addr = ?addr, "addr in use");
    res
}

/// --daemon：端口空闲则后台拉起，否则报错
async fn bw_serve_daemon(
    bw_args: &BWArgs,
    serve_args: &BWServeArgs,
) -> Result<()> {
    if serve_args.restart
        && let Err(e) = bw_serve_stop(serve_args).await
    {
        info!(
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
    start_bw_serve_daemon(bw_args, serve_args).await?;
    Ok(())
}

/// --stop：优雅终止后台 daemon
async fn bw_serve_stop(serve_args: &BWServeArgs) -> Result<()> {
    stop_daemon(&serve_args.hostname, serve_args.port).await?;
    wait_tcp_port(
        (&*serve_args.hostname, serve_args.port),
        true,
        serve_args.wait_port_timeout,
    )
    .await?;
    Ok(())
}

/// 向运行中的 daemon 发送优雅关闭请求
async fn stop_daemon(hostname: &str, port: u16) -> Result<()> {
    if !addr_in_use((hostname, port)).await? {
        bail!(
            "failed to stopping daemon unavailable addr={:?}",
            (hostname, port)
        )
    }
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
    let interval = Duration::from_millis(200);
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
