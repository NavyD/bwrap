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
async fn get_api_url_or_start_bw_serve_daemon(
    bw_args: &BWArgs,
) -> Result<String> {
    if let Some(s) = &bw_args.api_url {
        return Ok(s.to_string());
    }

    let hostname = "localhost";
    let port = "8087";
    let url = format!("http://{}:{}", hostname, port);
    if !bw_args.restart_agent
        && let Err(e) =
            TcpListener::bind(format!("{}:{}", hostname, port)).await
    {
        tracing::debug!(url = url, error = ?e, "found exists addr");
        return Ok(url);
    }

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
    cmd.args(["serve", "--hostname", hostname, "--port", port])
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
        // TODO
        todo!()
    }
    let child = cmd.spawn()?;
    tracing::info!(cmd = ?cmd, child = ?child, "spawned agent server");
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
}
