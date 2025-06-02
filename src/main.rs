#![allow(warnings)]

use std::{
    ffi::OsString,
    path::PathBuf,
    process::{self, Stdio},
};

use axum::{
    body::Body,
    extract::{Request, State},
    http,
    response::{IntoResponse, Response},
};
use clap::Parser;
use http_body_util::BodyExt;
use hyper::Method;
use hyper_util::rt::TokioExecutor;
use rand::{distr::Open01, prelude::*};

#[derive(Parser, Debug)]
struct Cli {
    #[arg(short, long)]
    port: u16,
    #[arg(long)]
    pwd: PathBuf,
}

#[tokio::main]
async fn main() {
    let args = std::env::args_os().take_while(|arg| arg != "--");
    let command_args = std::env::args_os().skip_while(|arg| arg != "--").skip(1);
    let cli = Cli::parse_from(args);

    let (_child_process, child_port) = run_child_process(&cli, command_args);

    let app = axum::routing::any(handler).with_state(AppState { child_port });

    println!("listening on localhost:{}", cli.port);

    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, cli.port))
        .await
        .unwrap();
    axum::serve(listener, app).await.unwrap();
}

fn run_child_process(cli: &Cli, mut args: impl Iterator<Item = OsString>) -> (process::Child, u16) {
    let val = rand::rng().sample::<f32, _>(Open01);
    let min = 4000;
    let max = 6000;
    let random_port = ((max - min) as f32 * val + min as f32) as u16;

    let args = args
        .map(|s| s.to_str().unwrap().to_owned())
        .collect::<Vec<_>>();

    let env_vars = args.iter().map_while(|arg| {
        if let Some((key, value)) = arg.split_once('=') {
            if key.chars().next().unwrap().is_uppercase() {
                return Some((key, value));
            }
        }
        None
    });

    let mut command_args = args.iter().skip_while(|arg| {
        if let Some((key, value)) = arg.split_once('=') {
            if key.chars().next().unwrap().is_uppercase() {
                return true;
            }
        }
        return false;
    });

    let mut cmd = std::process::Command::new(command_args.next().unwrap());
    // cmd.stdout(Stdio::piped());
    // cmd.stderr(Stdio::piped());
    cmd.args(command_args);
    cmd.envs(env_vars);
    cmd.current_dir(&cli.pwd);
    cmd.env("PORT", format!("{random_port}"));

    let mut child = cmd.spawn().unwrap();

    (child, random_port)
}

#[derive(Debug, Clone, Copy)]
struct AppState {
    child_port: u16,
}

async fn handler(State(state): State<AppState>, req: Request) -> Response {
    let io = tokio::net::TcpStream::connect(("0.0.0.0", state.child_port))
        .await
        .unwrap();
    let io = hyper_util::rt::TokioIo::new(io);
    let (mut sender, conn) = hyper::client::conn::http1::Builder::new()
        .handshake::<_, Body>(io)
        .await
        .unwrap();
    tokio::task::spawn(async move {
        if let Err(err) = conn.await {
            println!("Connection failed: {:?}", err);
        }
    });

    let res = sender.send_request(req).await.unwrap();

    res.map(Body::new)
}
