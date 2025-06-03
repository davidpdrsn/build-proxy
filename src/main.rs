use std::{
    env::args_os,
    ffi::OsString,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    process,
    time::Duration,
};

use axum::{
    body::Body,
    extract::{Request, State},
    response::{IntoResponse, Response},
};
use clap::Parser;
use futures::StreamExt;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use rand::{distr::Open01, prelude::*};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
};
use tracing::{debug, error, info};
use tracing_subscriber::EnvFilter;

mod watch;

#[derive(Parser, Debug)]
struct Cli {
    #[arg(short, long)]
    port: u16,
    #[arg(long)]
    pwd: PathBuf,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::fmt()
        .with_env_filter("build_proxy=debug".parse::<EnvFilter>().unwrap())
        .init();

    let args = args_os().take_while(|arg| arg != "--");
    let cli = Cli::parse_from(args);

    let command_args = args_os()
        .skip_while(|arg| arg != "--")
        .skip(1)
        .collect::<Vec<_>>();
    let (server, handle) = Server::new(cli.pwd.clone(), command_args).await;
    tokio::spawn(server.run());

    let app = axum::routing::any(handler).with_state(AppState { handle });
    info!("listening on localhost:{}", cli.port);
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, cli.port))
        .await
        .unwrap();
    axum::serve(listener, app).await.unwrap();
}

#[derive(Debug, Clone)]
struct AppState {
    handle: Handle,
}

async fn handler(State(state): State<AppState>, req: Request) -> Response {
    let AppState { handle } = state;

    let port = handle.get_port().await;

    let io = TcpStream::connect(("0.0.0.0", port)).await.unwrap();

    let (mut sender, conn) = http1::Builder::new()
        .handshake::<_, Body>(TokioIo::new(io))
        .await
        .unwrap();

    tokio::task::spawn(async move {
        if let Err(err) = conn.await {
            error!(%err, "connection failed");
        }
    });

    let res = sender.send_request(req).await.unwrap();

    res.into_response()
}

struct Server {
    rx: mpsc::Receiver<Msg>,
    child: Option<(process::Child, u16)>,
    pwd: PathBuf,
    args: Vec<OsString>,
}

impl Server {
    async fn new(pwd: PathBuf, args: Vec<OsString>) -> (Server, Handle) {
        let (tx, rx) = mpsc::channel::<Msg>(1024);
        let (child, port) = run_child_process(&pwd, args.iter()).await;

        (
            Server {
                rx,
                child: Some((child, port)),
                pwd,
                args,
            },
            Handle { tx },
        )
    }

    async fn run(mut self) {
        let mut watcher = watch::make_watcher(&self.pwd).filter(|event| {
            std::future::ready(
                event
                    .paths
                    .iter()
                    .any(|path| path.extension().is_some_and(|ext| ext == "go")),
            )
        });

        loop {
            tokio::select! {
                Some(msg) = self.rx.recv() => {
                    self.handle_msg(msg).await;
                }
                Some(event) = watcher.next() => {
                    self.handle_fs_event(event).await;
                }
            }
        }
    }

    async fn handle_fs_event(&mut self, event: notify::Event) {
        let Some((child, _)) = self.child.take() else {
            return;
        };

        debug!(?event, "received fs event");

        let pid = child.id();
        match nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as _),
            nix::sys::signal::Signal::SIGKILL,
        ) {
            Ok(_) => info!("killed child process"),
            Err(err) => {
                error!(%err, "failed to kill child process");
            }
        }
    }

    async fn handle_msg(&mut self, msg: Msg) {
        match msg {
            Msg::GetPort { reply } => loop {
                if let Some((_, port)) = &self.child {
                    _ = reply.send(*port);
                    break;
                } else {
                    let (new_child, new_port) =
                        run_child_process(&self.pwd, self.args.iter()).await;
                    info!(?new_port, "restarted child process");
                    self.child = Some((new_child, new_port));
                }
            },
        }
    }
}

#[derive(Debug, Clone)]
struct Handle {
    tx: mpsc::Sender<Msg>,
}

impl Handle {
    async fn get_port(&self) -> u16 {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Msg::GetPort { reply: tx }).await.unwrap();
        rx.await.unwrap()
    }
}

#[derive(Debug)]
enum Msg {
    GetPort { reply: oneshot::Sender<u16> },
}

async fn run_child_process<'a, I>(pwd: &Path, args: I) -> (process::Child, u16)
where
    I: Iterator<Item = &'a OsString>,
{
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
        if let Some((key, _)) = arg.split_once('=') {
            if key.chars().next().unwrap().is_uppercase() {
                return true;
            }
        }
        false
    });

    let mut cmd = std::process::Command::new(command_args.next().unwrap());
    cmd.args(command_args);
    cmd.envs(env_vars);
    cmd.current_dir(pwd);
    let port = random_port();
    cmd.env("PORT", port.to_string());

    info!(?cmd, "running child process");

    let child = cmd.spawn().unwrap();

    loop {
        match TcpStream::connect(("0.0.0.0", port)).await {
            Ok(_) => break,
            Err(_) => {
                debug!("child not ready yet...");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }

    (child, port)
}

fn random_port() -> u16 {
    let val = rand::rng().sample::<f32, _>(Open01);
    let min = 4000;
    let max = 6000;
    ((max - min) as f32 * val + min as f32) as u16
}
