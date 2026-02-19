use std::{
    collections::VecDeque,
    env::args_os,
    ffi::OsString,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    pin::pin,
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use parking_lot::Mutex;

use axum::{
    body::Body,
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use axum_extra::middleware::option_layer;
use clap::Parser;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use indicatif::{ProgressBar, ProgressStyle};
use rand::{distr::Open01, prelude::*};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::{TcpListener, TcpStream},
    process::{Child, Command},
    sync::{mpsc, oneshot},
};
use tokio_stream::StreamExt as _;
use tower::ServiceBuilder;
use tower_http::{
    ServiceBuilderExt as _,
    request_id::MakeRequestUuid,
    trace::{DefaultMakeSpan, DefaultOnRequest, DefaultOnResponse, TraceLayer},
};
use tracing::{error, info, trace, warn};
use tracing_indicatif::IndicatifLayer;
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

mod debounce;
mod watch;

#[derive(Parser, Debug)]
struct Cli {
    /// The port to listen on.
    #[arg(short, long)]
    port: u16,
    /// The directory to run the server in.
    #[arg(long)]
    pwd: Option<PathBuf>,
    /// Whether or not to enable verbose logging.
    #[arg(long, short = 'V')]
    verbose: bool,
}

#[tokio::main]
async fn main() {
    let args = args_os().take_while(|arg| arg != "--");
    let cli = Cli::parse_from(args);

    let indicatif_layer = IndicatifLayer::new();

    let default_filter = if cli.verbose {
        "build_proxy=trace,tower_http=trace,build=trace"
    } else {
        "build_proxy=debug,build=info"
    };

    let env_filter = match std::env::var("RUST_LOG") {
        Ok(filter) => match filter.parse::<EnvFilter>() {
            Ok(filter) => filter,
            Err(err) => {
                eprintln!("invalid RUST_LOG value ({err}); falling back to {default_filter}");
                EnvFilter::new(default_filter)
            }
        },
        Err(_) => EnvFilter::new(default_filter),
    };

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(indicatif_layer.get_stderr_writer())
                .with_filter(env_filter),
        )
        .with(indicatif_layer)
        .init();

    let pwd = match cli.pwd {
        Some(path) => path,
        None => match std::env::current_dir() {
            Ok(path) => path,
            Err(err) => {
                error!(?err, "failed to determine current working directory");
                return;
            }
        },
    };

    let command_args = args_os()
        .skip_while(|arg| arg != "--")
        .skip(1)
        .collect::<Vec<_>>();
    let (server, handle) = Server::new(pwd.clone(), command_args).await;
    tokio::spawn(server.run());

    let sensitive_headers = Arc::from([axum::http::header::COOKIE]);

    let app = axum::routing::any(handler)
        .layer(option_layer(cli.verbose.then(|| {
            ServiceBuilder::new()
                .map_response(|res: Response<_>| res.map(Body::new))
                .sensitive_request_headers(Arc::clone(&sensitive_headers))
                .set_x_request_id(MakeRequestUuid)
                .layer(
                    TraceLayer::new_for_http()
                        .make_span_with(
                            DefaultMakeSpan::default()
                                .level(tracing::Level::TRACE)
                                .include_headers(true),
                        )
                        .on_request(DefaultOnRequest::default().level(tracing::Level::TRACE))
                        .on_response(
                            DefaultOnResponse::default()
                                .include_headers(true)
                                .level(tracing::Level::TRACE),
                        ),
                )
                .propagate_x_request_id()
                .sensitive_response_headers(sensitive_headers)
        })))
        .with_state(AppState { handle });

    info!("listening on localhost:{}", cli.port);
    let listener = match TcpListener::bind((Ipv4Addr::UNSPECIFIED, cli.port)).await {
        Ok(listener) => listener,
        Err(err) => {
            error!(?err, port = cli.port, "failed to bind listener");
            return;
        }
    };
    if let Err(err) = axum::serve(listener, app).await {
        error!(?err, "server exited with error");
    }
}

#[derive(Debug, Clone)]
struct AppState {
    handle: Handle,
}

async fn handler(State(state): State<AppState>, req: Request) -> Response {
    let AppState { handle } = state;

    let Some(port) = handle.get_port().await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    let io = match TcpStream::connect(("localhost", port)).await {
        Ok(io) => io,
        Err(err) => {
            error!(?err, "failed to connect to child");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let (mut sender, conn) = match http1::Builder::new()
        .handshake::<_, Body>(TokioIo::new(io))
        .await
    {
        Ok(parts) => parts,
        Err(err) => {
            error!(?err, "failed to handshake with child");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    tokio::task::spawn(conn);

    let res = match sender.send_request(req).await {
        Ok(res) => res,
        Err(err) => {
            error!(?err, "failed to send request to child");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    res.into_response()
}

struct Server {
    rx: mpsc::Receiver<Msg>,
    child: Result<Option<(Child, u16)>, ChildBuildError>,
    pwd: PathBuf,
    args: Vec<OsString>,
}

impl Server {
    async fn new(pwd: PathBuf, args: Vec<OsString>) -> (Server, Handle) {
        let (tx, rx) = mpsc::channel::<Msg>(1024);
        let child = run_child_process(&pwd, args.iter()).await.map(Some);

        (
            Server {
                rx,
                child,
                pwd,
                args,
            },
            Handle { tx },
        )
    }

    async fn run(mut self) {
        let mut watcher = pin!(watch::make_watcher(&self.pwd).filter(|event| {
            event.paths.iter().all(|path| {
                let path_str = path.to_string_lossy();
                path.extension().is_some()
                    && !path.ends_with("swagger-initializer.js")
                    && !path_str.contains("/node_modules/")
                    && !path_str.contains("/.jj/repo/")
                    && !path_str.contains("/.jj/working_copy/")
            })
        }));

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
        trace!(?event, "received fs event");

        let child = match &mut self.child {
            Ok(child) => child,
            Err(_) => {
                // will trigger a rebuild on the next request
                self.child = Ok(None);
                return;
            }
        };

        let Some((child, port)) = child.take() else {
            return;
        };

        let Some(pid) = child.id() else {
            return;
        };

        match nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        ) {
            Ok(_) => {
                info!(?pid, "killed child process");
                kill_processes_listening_on_port(port);
            }
            Err(err) => {
                error!(%err, "failed to kill child process");
            }
        }
    }

    async fn handle_msg(&mut self, msg: Msg) {
        match msg {
            Msg::GetPort { reply } => {
                let mut rebuild_attempted = false;
                loop {
                    match &self.child {
                        Ok(Some((_, port))) => {
                            _ = reply.send(Some(*port));
                            break;
                        }
                        Ok(None) => {
                            rebuild_attempted = true;
                            let new_child = run_child_process(&self.pwd, self.args.iter())
                                .await
                                .map(Some);
                            if let Ok(Some((_, port))) = new_child {
                                info!(?port, "restarted child process");
                            }
                            self.child = new_child;
                        }
                        Err(_) => {
                            if rebuild_attempted {
                                // Already tried to rebuild this request, give up
                                _ = reply.send(None);
                                break;
                            }
                            // Reset to Ok(None) to attempt a rebuild
                            // This handles the case where external conditions have changed
                            // (e.g., dependencies installed, env vars set) without file changes
                            info!("retrying build...");
                            self.child = Ok(None);
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
struct Handle {
    tx: mpsc::Sender<Msg>,
}

impl Handle {
    async fn get_port(&self) -> Option<u16> {
        let (tx, rx) = oneshot::channel();
        if let Err(err) = self.tx.send(Msg::GetPort { reply: tx }).await {
            error!(?err, "failed to contact server task");
            return None;
        }

        match rx.await {
            Ok(port) => port,
            Err(err) => {
                error!(?err, "server task dropped reply channel");
                None
            }
        }
    }
}

#[derive(Debug)]
enum Msg {
    GetPort { reply: oneshot::Sender<Option<u16>> },
}

async fn run_child_process<'a, I>(pwd: &Path, args: I) -> Result<(Child, u16), ChildBuildError>
where
    I: Iterator<Item = &'a OsString>,
{
    let args = args
        .map(|s| s.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    let env_vars = args.iter().map_while(|arg| {
        if let Some((key, value)) = arg.split_once('=') {
            if let Some(first) = key.chars().next() {
                if first.is_uppercase() {
                    return Some((key.to_owned(), value.to_owned()));
                }
            }
        }
        None
    });

    let mut command_args = args.iter().skip_while(|arg| {
        if let Some((key, _)) = arg.split_once('=') {
            if let Some(first) = key.chars().next() {
                return first.is_uppercase();
            }
        }
        false
    });

    let Some(command) = command_args.next() else {
        error!("no command provided after --");
        return Err(ChildBuildError);
    };

    let mut cmd = Command::new(command);
    cmd.args(command_args);
    cmd.envs(env_vars);
    cmd.current_dir(pwd);
    let port = random_port();
    cmd.env("PORT", port.to_string());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    info!(?cmd, "running child process");

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            error!(%err, "failed to spawn child process");
            return Err(ChildBuildError);
        }
    };

    // Spawn tasks to read and log stdout/stderr
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            error!("child process stdout was not piped");
            return Err(ChildBuildError);
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            error!("child process stderr was not piped");
            return Err(ChildBuildError);
        }
    };

    // Collect recent stderr lines to display on build failure
    let stderr_buffer: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
    let stderr_buffer_clone = Arc::clone(&stderr_buffer);

    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = strip_ansi(&line);
            info!(target: "build", "{}", line);
        }
    });

    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = strip_ansi(&line);
            // Store in buffer for potential error display
            {
                let mut buffer = stderr_buffer_clone.lock();
                buffer.push_back(line.clone());
                // Keep last 20 lines
                while buffer.len() > 20 {
                    buffer.pop_front();
                }
            }
            warn!(target: "build", "{}", line);
        }
    });

    let spinner = ProgressBar::new_spinner();
    let spinner_style = match ProgressStyle::default_spinner().template("{spinner:.cyan} {msg}") {
        Ok(style) => style,
        Err(err) => {
            warn!(?err, "failed to configure spinner style, using default");
            ProgressStyle::default_spinner()
        }
    };
    spinner.set_style(spinner_style);
    spinner.set_message("Building...");
    spinner.enable_steady_tick(Duration::from_millis(100));

    loop {
        match TcpStream::connect(("localhost", port)).await {
            Ok(_) => {
                spinner.finish_and_clear();
                break;
            }
            Err(_) => {
                match child.try_wait() {
                    Ok(Some(status)) if !status.success() => {
                        spinner.finish_and_clear();

                        // Give stderr task a moment to flush remaining output
                        tokio::time::sleep(Duration::from_millis(50)).await;

                        // Display captured error output
                        let buffer = stderr_buffer.lock();
                        if !buffer.is_empty() {
                            error!("");
                            error!("build failed:");
                            error!("");
                            for line in buffer.iter() {
                                error!("  {}", line);
                            }
                            error!("");
                        } else {
                            error!("build failed");
                        }

                        return Err(ChildBuildError);
                    }
                    _ => {}
                }

                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }

    Ok((child, port))
}

fn random_port() -> u16 {
    let val = rand::rng().sample::<f32, _>(Open01);
    let min = 4000;
    let max = 6000;
    ((max - min) as f32 * val + min as f32) as u16
}

#[derive(Debug, Clone, Copy)]
struct ChildBuildError;

fn strip_ansi(s: &str) -> String {
    let bytes = strip_ansi_escapes::strip(s);
    String::from_utf8_lossy(&bytes).into_owned()
}

fn kill_processes_listening_on_port(port: u16) {
    let output = match std::process::Command::new("lsof")
        .args(["-ti"])
        .args([format!(":{port}")])
        .output()
    {
        Ok(output) => output,
        Err(err) => {
            warn!(?err, ?port, "failed to run lsof");
            return;
        }
    };

    if !output.status.success() {
        trace!(?port, status = ?output.status, "lsof found no processes to kill");
        return;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let pid = match line.parse::<i32>() {
            Ok(pid) => pid,
            Err(err) => {
                warn!(?err, line, "failed to parse pid from lsof output");
                continue;
            }
        };

        if let Err(err) = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        ) {
            trace!(?err, ?pid, "failed to kill process on port");
            continue;
        }

        info!(?pid, "killed child process");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_port_returns_none_if_server_task_is_gone() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);

        let handle = Handle { tx };

        assert_eq!(handle.get_port().await, None);
    }
}
