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
    response::{Html, IntoResponse, Response},
};
use axum_extra::middleware::option_layer;
use clap::Parser;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use indicatif::ProgressStyle;
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
use tracing_indicatif::{IndicatifLayer, span_ext::IndicatifSpanExt as _};
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
    let (server, handle) = Server::new(pwd.clone(), command_args);
    tokio::spawn(server.run());

    // Trigger initial build in the background.
    let warmup_handle = handle.clone();
    tokio::spawn(async move {
        warmup_handle.warmup_build().await;
    });

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

    let port = match handle.get_port().await {
        Ok(port) => port,
        Err(err) => return err.into_response(),
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
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    child: Result<Option<(Child, u16)>, ChildBuildError>,
    building: bool,
    waiters: Vec<oneshot::Sender<Result<u16, ChildBuildError>>>,
    pwd: PathBuf,
    args: Vec<OsString>,
}

impl Server {
    fn new(pwd: PathBuf, args: Vec<OsString>) -> (Server, Handle) {
        let (tx, rx) = mpsc::channel::<Msg>(1024);

        (
            Server {
                tx: tx.clone(),
                rx,
                child: Ok(None),
                building: false,
                waiters: Vec::new(),
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
                if let Ok(Some((_, port))) = &self.child {
                    _ = reply.send(Ok(*port));
                    return;
                }

                self.waiters.push(reply);
                self.ensure_child_ready(true);
            }
            Msg::WarmupBuild => {
                self.ensure_child_ready(false);
            }
            Msg::BuildFinished { result } => {
                self.building = false;
                match result {
                    Ok((child, port)) => {
                        self.child = Ok(Some((child, port)));
                        info!(?port, "started child process");
                        for waiter in self.waiters.drain(..) {
                            _ = waiter.send(Ok(port));
                        }
                    }
                    Err(err) => {
                        for waiter in self.waiters.drain(..) {
                            _ = waiter.send(Err(err.clone()));
                        }
                        self.child = Err(err);
                    }
                }
            }
        }
    }

    fn ensure_child_ready(&mut self, retry_after_error: bool) {
        match &self.child {
            Ok(Some(_)) => {}
            Ok(None) => self.start_build_if_needed(),
            Err(_) => {
                if retry_after_error {
                    info!("retrying build...");
                    self.child = Ok(None);
                    self.start_build_if_needed();
                }
            }
        }
    }

    fn start_build_if_needed(&mut self) {
        if self.building {
            return;
        }

        self.building = true;
        let tx = self.tx.clone();
        let pwd = self.pwd.clone();
        let args = self.args.clone();

        tokio::spawn(async move {
            let result = run_child_process(&pwd, args.iter()).await;
            if let Err(err) = tx.send(Msg::BuildFinished { result }).await {
                error!(?err, "failed to send build result to server task");
            }
        });
    }
}

#[derive(Debug, Clone)]
struct Handle {
    tx: mpsc::Sender<Msg>,
}

impl Handle {
    async fn get_port(&self) -> Result<u16, ChildBuildError> {
        let (tx, rx) = oneshot::channel();
        if let Err(err) = self.tx.send(Msg::GetPort { reply: tx }).await {
            error!(?err, "failed to contact server task");
            return Err(ChildBuildError::default());
        }

        match rx.await {
            Ok(port) => port,
            Err(err) => {
                error!(?err, "server task dropped reply channel");
                Err(ChildBuildError::default())
            }
        }
    }

    async fn warmup_build(&self) {
        if let Err(err) = self.tx.send(Msg::WarmupBuild).await {
            error!(?err, "failed to trigger warmup build");
        }
    }
}

#[derive(Debug)]
enum Msg {
    GetPort {
        reply: oneshot::Sender<Result<u16, ChildBuildError>>,
    },
    WarmupBuild,
    BuildFinished {
        result: Result<(Child, u16), ChildBuildError>,
    },
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
        return Err(ChildBuildError::default());
    };

    let mut cmd = Command::new(command);
    cmd.args(command_args);
    // Cargo disables color when stderr is piped. Respect inherited and explicit overrides.
    if std::env::var_os("CARGO_TERM_COLOR").is_none() {
        cmd.env("CARGO_TERM_COLOR", "always");
    }
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
            return Err(ChildBuildError {
                output: format!("Failed to spawn child process: {err}"),
            });
        }
    };

    // Spawn tasks to read and log stdout/stderr
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            error!("child process stdout was not piped");
            return Err(ChildBuildError::default());
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            error!("child process stderr was not piped");
            return Err(ChildBuildError::default());
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

    let mut stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            // Preserve ANSI styles for the HTML error page.
            // Store in buffer for potential error display
            {
                let mut buffer = stderr_buffer_clone.lock();
                buffer.push_back(line.clone());
                // Keep last 20 lines
                while buffer.len() > 20 {
                    buffer.pop_front();
                }
            }
            warn!(target: "build", "{}", strip_ansi(&line));
        }
    });

    // Share the logging layer's progress manager so logs clear and redraw the spinner.
    let spinner = tracing::info_span!("building");
    let spinner_style = match ProgressStyle::default_spinner().template("{spinner:.cyan} {msg}") {
        Ok(style) => style,
        Err(err) => {
            warn!(?err, "failed to configure spinner style, using default");
            ProgressStyle::default_spinner()
        }
    };
    spinner.pb_set_style(&spinner_style);
    spinner.pb_set_message("Building...");
    spinner.pb_start();

    loop {
        match TcpStream::connect(("localhost", port)).await {
            Ok(_) => {
                drop(spinner);
                break;
            }
            Err(_) => {
                match child.try_wait() {
                    Ok(Some(status)) if !status.success() => {
                        drop(spinner);

                        // Drain stderr, but don't wait forever if a descendant holds the pipe open.
                        _ = tokio::time::timeout(Duration::from_secs(1), &mut stderr_task).await;

                        // Display captured error output
                        let buffer = stderr_buffer.lock();
                        if !buffer.is_empty() {
                            error!("");
                            error!("build failed:");
                            error!("");
                            for line in buffer.iter() {
                                error!("  {}", strip_ansi(line));
                            }
                            error!("");
                        } else {
                            error!("build failed");
                        }

                        return Err(ChildBuildError {
                            output: buffer.iter().cloned().collect::<Vec<_>>().join("\n"),
                        });
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

#[derive(Debug, Clone, Default)]
struct ChildBuildError {
    output: String,
}

impl IntoResponse for ChildBuildError {
    fn into_response(self) -> Response {
        let plain_output = strip_ansi(&self.output);
        let output = if plain_output.trim().is_empty() {
            "The child server could not start. No compiler output was captured."
        } else {
            &self.output
        };
        // Conversion escapes HTML before adding style tags. Fall back to escaped plain text
        // if malformed ANSI output cannot be converted.
        let rendered = match ansi_to_html::convert(output) {
            Ok(html) => html,
            Err(err) => {
                warn!(?err, "failed to convert compiler colors to HTML");
                strip_ansi(output)
                    .replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
                    .replace('"', "&quot;")
                    .replace('\'', "&#39;")
            }
        };
        let html = format!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
             <title>Build failed</title><style>\
             body {{ background: #181818; color: #eee; padding: 2rem; font-family: system-ui; }}\
             pre {{ overflow-x: auto; padding: 1rem; background: #222; line-height: 1.5; }}\
             </style></head><body>\
             <h1>Build failed</h1><p>Fix the error and refresh to retry the build.</p>\
             <pre>{rendered}</pre></body></html>"
        );
        (StatusCode::INTERNAL_SERVER_ERROR, Html(html)).into_response()
    }
}

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
    use super::{ChildBuildError, Handle, IntoResponse, StatusCode, mpsc, run_child_process};
    use http_body_util::BodyExt as _;
    use std::ffi::OsString;

    #[tokio::test]
    async fn build_error_renders_colors_and_escapes_html() {
        let response = ChildBuildError {
            output: "\x1b[31merror: <script>alert('x')</script> & failed\x1b[0m".into(),
        }
        .into_response();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            response.headers()["content-type"],
            "text/html; charset=utf-8"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("<span style='color:"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("&amp; failed"));
        assert!(!html.contains("<script>"));
        assert!(!html.contains('\x1b'));
    }

    #[tokio::test]
    async fn failed_child_preserves_ansi_output() {
        let dir = tempfile::tempdir().unwrap();
        let args = [
            OsString::from("sh"),
            OsString::from("-c"),
            OsString::from("printf '\\033[31mcompile error <T>\\033[0m\\n' >&2; exit 1"),
        ];
        let err = run_child_process(dir.path(), args.iter())
            .await
            .unwrap_err();
        assert_eq!(err.output, "\x1b[31mcompile error <T>\x1b[0m");
    }

    #[tokio::test]
    async fn build_error_without_output_renders_fallback() {
        let response = ChildBuildError::default().into_response();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("No compiler output was captured."));
    }

    #[tokio::test]
    async fn get_port_returns_error_if_server_task_is_gone() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);

        let handle = Handle { tx };

        assert!(handle.get_port().await.is_err());
    }
}
