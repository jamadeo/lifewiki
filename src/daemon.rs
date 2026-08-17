//! The local inference daemon.
//!
//! Loading a 17 GB model takes about a minute, and `goose-local-inference`
//! already caches loaded models — the cache just dies with the process. This
//! holds one process open so the cache outlives a single command.
//!
//! It runs *only* inference. The agent loop, the tools, the wiki and the log
//! all stay in the CLI process, so the daemon never touches the wiki and
//! `lw ask`'s read-only guarantee is unaffected.

use crate::model::{Backend, LocalBackend};
use crate::protocol::{read_frame, write_frame, CompleteRequest, Request, Response, Status};
use anyhow::{Context, Result};
use fs2::FileExt;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

pub const BINARY_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn socket_path(home: &Path) -> PathBuf {
    home.join("daemon.sock")
}

fn lock_path(home: &Path) -> PathBuf {
    home.join("daemon.lock")
}

/// What the daemon holds between requests.
struct State {
    /// Loaded models, keyed by name. `LocalBackend` wraps a provider whose
    /// runtime caches the weights, so keeping the backend keeps the model.
    backends: HashMap<String, Arc<dyn Backend>>,
    started_at: String,
    requests_served: u64,
    /// Load order, for `lw daemon status`.
    order: Vec<String>,
}

impl State {
    fn new() -> Self {
        Self {
            backends: HashMap::new(),
            started_at: chrono::Local::now().to_rfc3339(),
            requests_served: 0,
            order: Vec::new(),
        }
    }

    fn status(&self) -> Status {
        Status {
            binary_version: BINARY_VERSION.to_string(),
            protocol_version: crate::protocol::VERSION,
            pid: std::process::id(),
            started_at: self.started_at.clone(),
            requests_served: self.requests_served,
            models: self.order.clone(),
        }
    }
}

/// Run the daemon until asked to stop. Blocks; this is `lw __daemon`.
pub fn serve(home: &Path) -> Result<()> {
    // Held for the process's lifetime. A pidfile would go stale if we were
    // SIGKILLed and the pid reused; a held lock cannot.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path(home))
        .context("could not open the daemon lock")?;
    if lock.try_lock_exclusive().is_err() {
        // Another daemon owns this home. Not an error: whoever started us
        // wanted a daemon running, and one is.
        eprintln!(
            "a lifewiki daemon is already running for {}",
            home.display()
        );
        return Ok(());
    }

    let socket = socket_path(home);
    // A socket file left by a killed daemon would make bind fail. The lock we
    // just took proves nobody is serving it.
    let _ = std::fs::remove_file(&socket);

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            let listener = UnixListener::bind(&socket)
                .with_context(|| format!("could not bind {}", socket.display()))?;
            // The socket carries a life's worth of text; keep it to its owner.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
            }

            eprintln!(
                "lifewiki daemon {} listening on {}",
                BINARY_VERSION,
                socket.display()
            );

            let state = Arc::new(Mutex::new(State::new()));
            let (stop_tx, mut stop_rx) = tokio::sync::mpsc::channel::<()>(1);

            // A daemon whose repository has been deleted has nothing to serve,
            // and would otherwise hold its models forever. Nothing reconnects
            // to it, so nothing would ever notice.
            let mut orphan_check = tokio::time::interval(std::time::Duration::from_secs(10));
            orphan_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let watched = socket.clone();

            loop {
                tokio::select! {
                    _ = stop_rx.recv() => break,
                    // Ctrl-C and `kill` should still clean up the socket.
                    _ = tokio::signal::ctrl_c() => break,
                    _ = orphan_check.tick() => {
                        if !watched.exists() {
                            eprintln!("socket is gone; the repository was removed or reset");
                            break;
                        }
                        continue;
                    }
                    accepted = listener.accept() => {
                        let (stream, _) = accepted.context("accept failed")?;
                        let state = state.clone();
                        let stop = stop_tx.clone();
                        tokio::spawn(async move {
                            if let Err(error) = handle(stream, state, stop).await {
                                eprintln!("connection error: {error:#}");
                            }
                        });
                    }
                }
            }

            let _ = std::fs::remove_file(&socket);
            eprintln!("lifewiki daemon stopped");
            Ok::<(), anyhow::Error>(())
        })?;

    let _ = fs2::FileExt::unlock(&lock);
    Ok(())
}

async fn handle(
    mut stream: UnixStream,
    state: Arc<Mutex<State>>,
    stop: tokio::sync::mpsc::Sender<()>,
) -> Result<()> {
    let request: Request = read_frame(&mut stream).await?;
    let response = match request {
        Request::Status => Response::Status(state.lock().await.status()),
        Request::Shutdown => {
            let _ = stop.try_send(());
            Response::ShuttingDown
        }
        Request::Complete(complete) => complete_one(*complete, &state).await,
    };
    write_frame(&mut stream, response).await
}

async fn complete_one(request: CompleteRequest, state: &Arc<Mutex<State>>) -> Response {
    // The whole request is serialised behind this guard. llama.cpp contexts are
    // not shareable, and two concurrent runs would each want several GB of KV
    // cache; queueing is simpler and kinder to the machine.
    let mut guard = state.lock().await;

    let backend = match guard.backends.get(&request.model) {
        Some(backend) => backend.clone(),
        None => match LocalBackend::new(&request.model).await {
            Ok(backend) => {
                let backend: Arc<dyn Backend> = Arc::new(backend);
                guard
                    .backends
                    .insert(request.model.clone(), backend.clone());
                guard.order.push(request.model.clone());
                backend
            }
            Err(error) => return Response::Failed(format!("{error:#}")),
        },
    };

    match backend
        .complete(&request.system, &request.messages, &request.tools)
        .await
    {
        Ok((message, usage)) => {
            guard.requests_served += 1;
            Response::Completed {
                message: Box::new(message),
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
            }
        }
        Err(error) => Response::Failed(format!("{error:#}")),
    }
}

/// Send one request to a running daemon, if there is one.
pub async fn request(home: &Path, body: Request) -> Result<Response> {
    let mut stream = UnixStream::connect(socket_path(home))
        .await
        .context("no daemon is listening")?;
    write_frame(&mut stream, body).await?;
    read_frame(&mut stream).await
}

/// True if a daemon is listening and speaking our protocol.
pub async fn probe(home: &Path) -> Option<Status> {
    match request(home, Request::Status).await {
        Ok(Response::Status(status)) => Some(status),
        _ => None,
    }
}

/// Start a daemon and wait for it to accept connections.
///
/// Spawns this same executable with a hidden subcommand, so there is nothing
/// separate to install and no way for the two to be different builds.
pub async fn start(home: &Path) -> Result<Status> {
    let executable = std::env::current_exe().context("could not find the lifewiki executable")?;

    std::process::Command::new(executable)
        .arg("__daemon")
        .env("LIFEWIKI_HOME", home)
        // Detach: the daemon must outlive the command that started it, and its
        // startup chatter should not land in the middle of the user's output.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("could not start the daemon")?;

    // Loading is lazy, so this only waits for the socket, not the model.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut wait = std::time::Duration::from_millis(20);
    while std::time::Instant::now() < deadline {
        if let Some(status) = probe(home).await {
            return Ok(status);
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(std::time::Duration::from_millis(250));
    }
    anyhow::bail!("the daemon did not start within 30 seconds")
}

/// Ask a running daemon to exit. Returns false if none was running.
pub async fn stop(home: &Path) -> Result<bool> {
    match request(home, Request::Shutdown).await {
        Ok(_) => Ok(true),
        Err(_) => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::VERSION;

    /// The server generic over a backend, so the socket path is testable
    /// without a model. Mirrors `serve` minus the lock and the real backend.
    async fn serve_scripted(
        socket: PathBuf,
        backend: Arc<dyn Backend>,
        mut shutdown: tokio::sync::oneshot::Receiver<()>,
    ) {
        let listener = UnixListener::bind(&socket).unwrap();
        let served = Arc::new(std::sync::atomic::AtomicU64::new(0));
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                accepted = listener.accept() => {
                    let (mut stream, _) = accepted.unwrap();
                    let backend = backend.clone();
                    let served = served.clone();
                    tokio::spawn(async move {
                        let request: Request = read_frame(&mut stream).await.unwrap();
                        let response = match request {
                            Request::Status => Response::Status(Status {
                                binary_version: BINARY_VERSION.to_string(),
                                protocol_version: VERSION,
                                pid: std::process::id(),
                                started_at: "now".into(),
                                requests_served: served.load(std::sync::atomic::Ordering::SeqCst),
                                models: vec!["scripted".into()],
                            }),
                            Request::Shutdown => Response::ShuttingDown,
                            Request::Complete(complete) => {
                                served.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                match backend
                                    .complete(&complete.system, &complete.messages, &complete.tools)
                                    .await
                                {
                                    Ok((message, usage)) => Response::Completed {
                                        message: Box::new(message),
                                        input_tokens: usage.input_tokens,
                                        output_tokens: usage.output_tokens,
                                    },
                                    Err(error) => Response::Failed(format!("{error:#}")),
                                }
                            }
                        };
                        write_frame(&mut stream, response).await.unwrap();
                    });
                }
            }
        }
        let _ = std::fs::remove_file(&socket);
    }

    struct Fixture {
        _dir: crate::tools::tests::tempdir::Dir,
        home: PathBuf,
        shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    }

    impl Fixture {
        async fn start(backend: Arc<dyn Backend>) -> Self {
            let dir = crate::tools::tests::tempdir::Dir::new();
            let home = dir.path().to_path_buf();
            let (tx, rx) = tokio::sync::oneshot::channel();
            let socket = socket_path(&home);
            tokio::spawn(serve_scripted(socket, backend, rx));

            // Wait for bind.
            for _ in 0..200 {
                if probe(&home).await.is_some() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            Self {
                _dir: dir,
                home,
                shutdown: Some(tx),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(tx) = self.shutdown.take() {
                let _ = tx.send(());
            }
        }
    }

    fn scripted() -> Arc<dyn Backend> {
        crate::model::tests::ScriptedBackend::new(vec![
            crate::model::tests::ScriptedBackend::text("first"),
            crate::model::tests::ScriptedBackend::text("second"),
        ])
    }

    #[tokio::test]
    async fn a_request_reaches_the_backend_and_the_reply_comes_back() {
        let fixture = Fixture::start(scripted()).await;
        let response = request(
            &fixture.home,
            Request::Complete(Box::new(CompleteRequest {
                model: "scripted".into(),
                system: "sys".into(),
                messages: vec![
                    goose_providers::conversation::message::Message::user().with_text("hello")
                ],
                tools: Vec::new(),
            })),
        )
        .await
        .unwrap();

        match response {
            Response::Completed { message, .. } => {
                assert_eq!(message.as_concat_text(), "first");
            }
            other => panic!("wrong response: {other:?}"),
        }
    }

    /// The point of the daemon: state survives between connections.
    #[tokio::test]
    async fn state_persists_across_connections() {
        let fixture = Fixture::start(scripted()).await;
        for expected in ["first", "second"] {
            let response = request(
                &fixture.home,
                Request::Complete(Box::new(CompleteRequest {
                    model: "scripted".into(),
                    system: "sys".into(),
                    messages: Vec::new(),
                    tools: Vec::new(),
                })),
            )
            .await
            .unwrap();
            match response {
                Response::Completed { message, .. } => {
                    assert_eq!(message.as_concat_text(), expected)
                }
                other => panic!("wrong response: {other:?}"),
            }
        }

        // The script advanced across two separate connections, which only works
        // if the server kept it.
        match probe(&fixture.home).await {
            Some(status) => assert_eq!(status.requests_served, 2),
            None => panic!("no status"),
        }
    }

    #[tokio::test]
    async fn probing_nothing_returns_none() {
        let dir = crate::tools::tests::tempdir::Dir::new();
        assert!(probe(dir.path()).await.is_none());
    }

    /// A model failure must arrive as a message, not a dropped connection.
    #[tokio::test]
    async fn a_backend_error_is_reported_over_the_wire() {
        // An exhausted script errors on the first call.
        let fixture = Fixture::start(crate::model::tests::ScriptedBackend::new(Vec::new())).await;
        let response = request(
            &fixture.home,
            Request::Complete(Box::new(CompleteRequest {
                model: "scripted".into(),
                system: String::new(),
                messages: Vec::new(),
                tools: Vec::new(),
            })),
        )
        .await
        .unwrap();
        assert!(matches!(response, Response::Failed(_)), "{response:?}");
    }

    #[tokio::test]
    async fn status_reports_the_binary_version() {
        let fixture = Fixture::start(scripted()).await;
        let status = probe(&fixture.home).await.unwrap();
        assert_eq!(status.binary_version, BINARY_VERSION);
        assert_eq!(status.protocol_version, VERSION);
    }

    /// A socket left behind by a killed daemon must not look like a live one.
    #[tokio::test]
    async fn a_stale_socket_file_is_not_mistaken_for_a_daemon() {
        let dir = crate::tools::tests::tempdir::Dir::new();
        std::fs::write(socket_path(dir.path()), b"").unwrap();
        assert!(probe(dir.path()).await.is_none());
    }

    /// A daemon whose repository is deleted must exit, not hold its models
    /// forever. Nothing reconnects to such a daemon, so nothing else would
    /// notice — 19 of them once accumulated here, ~300 GB of allocations.
    #[tokio::test]
    async fn a_daemon_exits_when_its_socket_disappears() {
        let dir = crate::tools::tests::tempdir::Dir::new();
        let home = dir.path().to_path_buf();
        let socket = socket_path(&home);

        // Mirrors the orphan check in `serve`.
        let watcher = tokio::spawn(async move {
            let mut ticks = tokio::time::interval(std::time::Duration::from_millis(10));
            loop {
                ticks.tick().await;
                if !socket.exists() {
                    return "exited";
                }
            }
        });

        std::fs::write(socket_path(&home), b"").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        std::fs::remove_file(socket_path(&home)).unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), watcher)
            .await
            .expect("the daemon should notice its socket is gone")
            .unwrap();
        assert_eq!(result, "exited");
    }

    #[tokio::test]
    async fn stopping_nothing_is_not_an_error() {
        let dir = crate::tools::tests::tempdir::Dir::new();
        assert!(!stop(dir.path()).await.unwrap());
    }
}
