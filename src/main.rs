use std::fmt::Debug;

use anyhow::{Result, bail};
use bwrap::bwserve_api;
use bwrap::bwserve_api::{BWGetArgs, BwListArgs};
use clap::{Parser, Subcommand};
use sonic_rs::to_string as ser_to_json;
use tokio::io::{self, AsyncWriteExt};
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
    #[arg(long, env = "BW_SERVE_API_URL")]
    api_url: String,
}

#[derive(Subcommand, Debug)]
enum BWCommands {
    Get(BWGetArgs),
    List(BwListArgs),
    Status,
}

async fn write_str(mut w: impl AsyncWriteExt + Unpin, s: impl AsRef<str>) -> Result<()> {
    w.write_all(s.as_ref().as_bytes()).await?;
    w.write_u8(b'\n').await?;
    w.flush().await?;
    Ok(())
}

async fn handle_resp_err<T: Debug>(resp_data: &bwserve_api::BWServeResp<T>) -> Result<bool> {
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
    let api = bwserve_api::BWServeApi::new(&bw_args.api_url)?;
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
    let api = bwserve_api::BWServeApi::new(&bw_args.api_url)?;
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
    let api = bwserve_api::BWServeApi::new(&bw_args.api_url)?;
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
