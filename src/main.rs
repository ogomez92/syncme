// No console window for release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod announce;
mod autostart;
mod discovery;
mod index;
mod model;
mod peer;
mod state;
mod sync;
#[cfg(feature = "tray")]
mod tray;
mod web;

use anyhow::{Context, Result};
use parking_lot::{Mutex, RwLock};
use state::App;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// `--data-dir` as given on the command line, reused for "start at login".
pub static DATA_DIR_ARG: OnceLock<String> = OnceLock::new();

struct Args {
    data_dir: Option<PathBuf>,
    port: Option<u16>,
    name: Option<String>,
    background: bool,
    headless: bool,
}

const HELP: &str = "SyncMe - keeps folders in sync between your computers.

Usage: syncme [options]
  --data-dir <dir>   where settings and indexes are kept
                     (default: a 'syncme-data' folder next to the program if it exists,
                     otherwise your user config folder)
  --port <port>      port for the web app and other devices (default 47474)
  --name <name>      name shown to other devices (default: computer name)
  --background       don't open the web app on start
  --headless         no tray icon; run until stopped (for servers and tests)
";

fn parse_args() -> Args {
    let mut a = Args { data_dir: None, port: None, name: None, background: false, headless: false };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--data-dir" => {
                let d = it.next().expect("--data-dir needs a value");
                let _ = DATA_DIR_ARG.set(d.clone());
                a.data_dir = Some(PathBuf::from(d));
            }
            "--port" => a.port = it.next().and_then(|p| p.parse().ok()),
            "--name" => a.name = it.next(),
            "--background" => a.background = true,
            "--headless" => a.headless = true,
            "-h" | "--help" => {
                println!("{HELP}");
                std::process::exit(0);
            }
            other => eprintln!("ignoring unknown argument {other}"),
        }
    }
    a
}

fn default_data_dir() -> PathBuf {
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.join("syncme-data"))) {
        if dir.is_dir() {
            return dir; // portable mode
        }
    }
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("SyncMe")
}

fn init_logging(data_dir: &std::path::Path) {
    let log = data_dir.join("syncme.log");
    if std::fs::metadata(&log).map(|m| m.len() > 5_000_000).unwrap_or(false) {
        let _ = std::fs::rename(&log, data_dir.join("syncme.old.log"));
    }
    let file = std::fs::OpenOptions::new().create(true).append(true).open(&log).ok();
    let filter = tracing_subscriber::filter::Targets::new()
        .with_default(tracing::Level::INFO)
        .with_target("mdns_sd", tracing::Level::WARN)
        .with_target("syncme", tracing::Level::DEBUG);
    use tracing_subscriber::prelude::*;
    let registry = tracing_subscriber::registry().with(filter);
    match file {
        Some(f) => {
            let f = Arc::new(f);
            registry
                .with(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(f))
                .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
                .init()
        }
        None => registry.with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr)).init(),
    }
}

fn main() {
    let args = parse_args();
    let data_dir = args.data_dir.clone().unwrap_or_else(default_data_dir);
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        eprintln!("cannot create {}: {e}", data_dir.display());
        std::process::exit(1);
    }

    // One instance per data folder: a second launch just opens the web app.
    let lock_file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(data_dir.join("syncme.lock")).expect("lock file");
    if lock_file.try_lock().is_err() {
        if let Ok(port) = std::fs::read_to_string(data_dir.join("port")) {
            let _ = open::that(format!("http://127.0.0.1:{}/", port.trim()));
        }
        return;
    }

    init_logging(&data_dir);
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().worker_threads(4).build().expect("tokio runtime");
    let app = match rt.block_on(start(&args, data_dir.clone())) {
        Ok(app) => app,
        Err(e) => {
            tracing::error!("startup failed: {e:#}");
            std::process::exit(1);
        }
    };
    let open_browser = !args.background && !args.headless && app.cfg.read().settings.open_browser_on_start;
    if open_browser {
        let _ = open::that_detached(app.ui_url());
    }
    if args.headless || cfg!(not(feature = "tray")) {
        rt.block_on(async {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = app.shutdown.cancelled() => {}
            }
        });
        app.save();
        drop(lock_file);
        return;
    }
    #[cfg(feature = "tray")]
    {
        let _keep_lock = lock_file;
        tray::run(app, rt);
    }
}

async fn bind(port: Option<u16>) -> Result<tokio::net::TcpListener> {
    if let Some(p) = port {
        return tokio::net::TcpListener::bind(("0.0.0.0", p)).await.with_context(|| format!("port {p} is in use"));
    }
    for p in model::DEFAULT_PORT..model::DEFAULT_PORT + 10 {
        if let Ok(l) = tokio::net::TcpListener::bind(("0.0.0.0", p)).await {
            return Ok(l);
        }
    }
    anyhow::bail!("no free port between {} and {}", model::DEFAULT_PORT, model::DEFAULT_PORT + 9)
}

async fn start(args: &Args, data_dir: PathBuf) -> Result<state::AppRef> {
    let cfg = state::load_or_init(&data_dir, args.name.clone());
    let listener = bind(args.port).await?;
    let port = listener.local_addr()?.port();
    std::fs::write(data_dir.join("port"), port.to_string())?;

    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(4))
        .read_timeout(Duration::from_secs(60))
        .no_proxy()
        .build()?;
    let (ui_tx, _) = tokio::sync::broadcast::channel(256);
    let app = Arc::new(App {
        data_dir,
        port,
        cfg: RwLock::new(cfg),
        discovered: Mutex::new(HashMap::new()),
        online: Mutex::new(HashMap::new()),
        incoming: Mutex::new(HashMap::new()),
        outgoing: Mutex::new(HashMap::new()),
        http,
        ui_tx,
        activity: Mutex::new(VecDeque::new()),
        ui_visible: AtomicUsize::new(0),
        announcer: announce::Announcer::start(),
        transfers: Mutex::new(HashMap::new()),
        next_transfer: AtomicUsize::new(1),
        tray: Mutex::new(None),
        engine: sync::Engine::default(),
        shutdown: tokio_util::sync::CancellationToken::new(),
    });
    app.save();
    tracing::info!("SyncMe {} as \"{}\" ({}) on port {port}, data in {}", env!("CARGO_PKG_VERSION"), app.my_name(), app.me(), app.data_dir.display());

    let router = web::routes(app.clone()).merge(peer::routes()).with_state(app.clone());
    let shutdown = app.shutdown.clone();
    tokio::spawn(async move {
        let svc = router.into_make_service_with_connect_info::<std::net::SocketAddr>();
        if let Err(e) = axum::serve(listener, svc).with_graceful_shutdown(async move { shutdown.cancelled().await }).await {
            tracing::error!("server: {e}");
        }
    });
    sync::reconcile(&app);
    discovery::start(&app);
    tokio::spawn(peer::monitor(app.clone()));
    Ok(app)
}
