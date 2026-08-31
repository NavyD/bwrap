use anyhow::Result;
use clap::Parser;
use std::{
    fs,
    io::{Write, copy, stderr, stdout},
    path::{Path, PathBuf},
    process::ExitCode,
};

/// Execute a command with optional stdout/stderr redirection
#[derive(Parser)]
struct Cli {
    #[arg(long, short = 'o', env = "MOCKBW_STDOUT")]
    stdout: Option<String>,
    #[arg(long, short = 'O', env = "MOCKBW_STDOUT_FILE")]
    stdout_file: Option<PathBuf>,

    #[arg(long, short = 'e', env = "MOCKBW_STDERR")]
    stderr: Option<String>,
    #[arg(long, short = 'E', env = "MOCKBW_STDERR_FILE")]
    stderr_file: Option<PathBuf>,

    #[arg(long, short = 'c', default_value_t = 0, env = "MOCKBW_EXITCODE")]
    exitcode: u8,

    /// Arguments for the command (captures all remaining arguments)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

fn main() -> ExitCode {
    run().expect("failed to run")
}

fn run() -> Result<ExitCode> {
    let cli = Cli::try_parse()?;
    let mut w = stdout();
    if let Some(s) = cli.stdout {
        writeln!(w, "{}", s)?;
    }
    if let Some(p) = cli.stdout_file {
        copy_file(&p, &mut w)?;
    }

    let mut w = stderr();
    if let Some(s) = cli.stderr {
        writeln!(w, "{}", s)?;
    }
    if let Some(p) = cli.stderr_file {
        copy_file(&p, &mut w)?;
    }
    Ok(cli.exitcode.into())
}

fn copy_file(path: &Path, mut w: impl Write) -> Result<()> {
    let mut file = fs::OpenOptions::new().read(true).open(path)?;
    copy(&mut file, &mut w)?;
    Ok(())
}
