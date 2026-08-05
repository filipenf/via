//! Thin `via-remote` binary: `serve` (control-socket daemon) and `proxy`
//! (stdio ↔ socket bridge). Plain argv parsing — deliberately no clap to keep
//! the helper dependency tree minimal.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};

use via_remote::{ProxyArgs, ServeArgs, default_control_socket};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("via-remote: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some(subcommand) = args.first() else {
        return usage();
    };
    match subcommand.as_str() {
        "serve" => via_remote::serve::run(parse_serve(&args[1..])?),
        "proxy" => via_remote::proxy::run(parse_proxy(&args[1..])?),
        "help" | "--help" | "-h" => usage(),
        "version" | "--version" | "-V" => {
            println!("via-remote {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => {
            eprintln!("via-remote: unknown subcommand `{other}`");
            usage()
        }
    }
}

fn usage() -> Result<()> {
    eprintln!(
        "usage: via-remote serve [--socket PATH] [--foreground] [--cwd DIR]\n\
         \x20      via-remote proxy [--socket PATH]\n\
         \x20      via-remote help | version"
    );
    bail!("bad arguments")
}

fn parse_serve(args: &[String]) -> Result<ServeArgs> {
    let mut socket = None;
    let mut foreground = false;
    let mut cwd = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--socket" => socket = Some(PathBuf::from(next_value(&mut iter, "--socket")?)),
            "--foreground" => foreground = true,
            "--cwd" => cwd = Some(PathBuf::from(next_value(&mut iter, "--cwd")?)),
            _ => bail!("unknown serve flag `{arg}`"),
        }
    }
    Ok(ServeArgs {
        socket: socket.unwrap_or_else(default_control_socket),
        foreground,
        cwd: cwd.unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
    })
}

fn parse_proxy(args: &[String]) -> Result<ProxyArgs> {
    let mut socket = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--socket" => socket = Some(PathBuf::from(next_value(&mut iter, "--socket")?)),
            _ => bail!("unknown proxy flag `{arg}`"),
        }
    }
    Ok(ProxyArgs {
        socket: socket.unwrap_or_else(default_control_socket),
    })
}

fn next_value<'a>(iter: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<&'a str> {
    iter.next()
        .map(String::as_str)
        .with_context(|| format!("{flag} requires a value"))
}
