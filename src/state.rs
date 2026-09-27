//! Persistent configuration and shared runtime state.

use crate::announce::Announcer;
use crate::model::*;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tokio::sync::broadcast;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    /// Announce sync activity through the screen reader when the web app isn't visible.
    pub announce: bool,
    /// Also announce through Prism while the web app is visible (it has its own live region).
    pub announce_when_visible: bool,
    /// Fall back to the system voice when no screen reader is running.
    pub allow_tts: bool,
    pub open_browser_on_start: bool,
    pub start_at_login: bool,
    pub trash_days: u64,
    /// Share the clipboard (text and images, never files) with connected
    /// devices that also have this on.
    pub sync_clipboard: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { announce: true, announce_when_visible: false, allow_tts: false, open_browser_on_start: true, start_at_login: false, trash_days: 30, sync_clipboard: false }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PeerRec {
    pub id: NodeId,
    pub name: String,
    pub os: String,
    pub token: String,
    /// Known addresses (host:port), most recently working first.
    pub addrs: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Persisted {
    pub node_id: NodeId,
    pub name: String,
    #[serde(default)]
    pub peers: BTreeMap<NodeId, PeerRec>,
    #[serde(default)]
    pub shares: BTreeMap<String, Share>,
    #[serde(default)]
    pub settings: Settings,
    /// Addresses the user typed in by hand; probed forever.
    #[serde(default)]
    pub manual_addrs: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Discovered {
    pub hello: Hello,
    pub addr: String,
    pub via: String,
    #[serde(skip)]
    pub last_seen: Option<Instant>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Incoming {
    pub hello: Hello,
    pub addr: String,
    #[serde(skip)]
    pub token: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Activity {
    pub time_ms: u64,
    pub text: String,
    pub level: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum UiEvent {
    /// Something changed; the page should refetch /api/state.
    State,
    /// A human-readable event the page should add to its live region.
    Event { text: String, level: String },
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Transfer {
    pub share: String,
    pub path: String,
    pub peer: String,
    pub done: u64,
    pub total: u64,
    pub upload: bool,
}

/// What the tray/menu bar shows.
#[derive(Clone, Debug, Default)]
#[cfg_attr(not(feature = "tray"), allow(dead_code))]
pub struct TrayStatus {
    pub busy: bool,
    pub status: String,
    /// Most recent event and when it happened (ms since the epoch; 0 if none).
    pub last: String,
    pub last_ms: u64,
}

pub type TraySink = Box<dyn Fn(TrayStatus) + Send + Sync>;

pub struct App {
    pub data_dir: PathBuf,
    pub port: u16,
    pub cfg: RwLock<Persisted>,
    pub discovered: Mutex<HashMap<NodeId, Discovered>>,
    /// Paired peers currently reachable -> working address.
    pub online: Mutex<HashMap<NodeId, String>>,
    pub incoming: Mutex<HashMap<NodeId, Incoming>>,
    /// Pairing requests we sent: node -> token.
    pub outgoing: Mutex<HashMap<NodeId, String>>,
    pub http: reqwest::Client,
    pub ui_tx: broadcast::Sender<UiEvent>,
    pub activity: Mutex<VecDeque<Activity>>,
    pub ui_visible: AtomicUsize,
    pub announcer: Announcer,
    pub transfers: Mutex<HashMap<u64, Transfer>>,
    pub next_transfer: AtomicUsize,
    pub tray: Mutex<Option<TraySink>>,
    pub engine: crate::sync::Engine,
    pub shutdown: tokio_util::sync::CancellationToken,
    /// Set by `changed()`; drained by `ui_pump`, which coalesces bursts.
    pub ui_kick: tokio::sync::Notify,
    pub clipboard: crate::clipboard::Service,
}

pub type AppRef = Arc<App>;

impl App {
    /// Builds the shared state and starts its UI pump. Call inside a tokio runtime.
    pub fn new(data_dir: PathBuf, port: u16, cfg: Persisted, clipboard: crate::clipboard::Service) -> anyhow::Result<AppRef> {
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(4))
            .read_timeout(std::time::Duration::from_secs(60))
            .no_proxy()
            .build()?;
        let (ui_tx, _) = broadcast::channel(256);
        clipboard.set_enabled(cfg.settings.sync_clipboard);
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
            announcer: Announcer::start(),
            transfers: Mutex::new(HashMap::new()),
            next_transfer: AtomicUsize::new(1),
            tray: Mutex::new(None),
            engine: crate::sync::Engine::default(),
            shutdown: tokio_util::sync::CancellationToken::new(),
            ui_kick: tokio::sync::Notify::new(),
            clipboard,
        });
        tokio::spawn(App::ui_pump(app.clone()));
        Ok(app)
    }

    pub fn me(&self) -> NodeId {
        self.cfg.read().node_id.clone()
    }

    pub fn my_name(&self) -> String {
        self.cfg.read().name.clone()
    }

    pub fn hello(&self) -> Hello {
        let c = self.cfg.read();
        Hello {
            app: APP.into(),
            protocol: PROTOCOL,
            id: c.node_id.clone(),
            name: c.name.clone(),
            os: std::env::consts::OS.into(),
            port: self.port,
            version: env!("CARGO_PKG_VERSION").into(),
        }
    }

    pub fn node_name(&self, id: &str) -> String {
        let c = self.cfg.read();
        if id == c.node_id {
            return c.name.clone();
        }
        if let Some(p) = c.peers.get(id) {
            return p.name.clone();
        }
        drop(c);
        self.discovered.lock().get(id).map(|d| d.hello.name.clone()).unwrap_or_else(|| "unknown device".into())
    }

    pub fn save(&self) {
        let path = self.data_dir.join("state.json");
        let data = serde_json::to_vec_pretty(&*self.cfg.read()).expect("serialize state");
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, data).and_then(|_| std::fs::rename(&tmp, &path)) {
            tracing::error!("saving state: {e}");
        }
    }

    /// Marks the state as changed. The web app and tray are refreshed by
    /// `ui_pump` shortly after, so a burst of small transfers costs one update.
    pub fn changed(&self) {
        self.ui_kick.notify_one();
    }

    /// Records an event and shows it in the web app, without speaking it.
    pub fn note(&self, level: &str, text: impl Into<String>) -> String {
        let text = text.into();
        tracing::info!("[{level}] {text}");
        {
            let mut a = self.activity.lock();
            a.push_front(Activity { time_ms: crate::index::now_ms(), text: text.clone(), level: level.into() });
            a.truncate(200);
        }
        let _ = self.ui_tx.send(UiEvent::Event { text: text.clone(), level: level.into() });
        self.changed();
        text
    }

    /// Records an event, shows it in the web app, and speaks it through the
    /// screen reader when nobody is looking at the web app.
    pub fn event(&self, level: &str, text: impl Into<String>) {
        let text = self.note(level, text);
        let s = self.cfg.read().settings.clone();
        let visible = self.ui_visible.load(Ordering::SeqCst) > 0;
        let muted = std::env::var_os("SYNCME_MUTE").is_some();
        if s.announce && !muted && (!visible || s.announce_when_visible) {
            self.announcer.say(format!("SyncMe: {text}"), s.allow_tts);
        }
    }

    pub fn transfer_start(&self, t: Transfer) -> u64 {
        let id = self.next_transfer.fetch_add(1, Ordering::SeqCst) as u64;
        self.transfers.lock().insert(id, t);
        self.changed();
        id
    }

    pub fn transfer_progress(&self, id: u64, done: u64) {
        if let Some(t) = self.transfers.lock().get_mut(&id) {
            t.done = done;
        }
    }

    pub fn transfer_end(&self, id: u64) {
        self.transfers.lock().remove(&id);
        self.changed();
    }

    pub fn tray_status(&self) -> TrayStatus {
        let transfers = self.transfers.lock();
        let downloads: Vec<&Transfer> = transfers.values().filter(|t| !t.upload).collect();
        let uploads = transfers.len() - downloads.len();
        let online = self.online.lock().len();
        let errors = self.engine.error_count();
        let status = if let Some(t) = downloads.first() {
            let more = if downloads.len() > 1 { format!(" (+{} more)", downloads.len() - 1) } else { String::new() };
            format!("Receiving {} from {}{more}", t.path.rsplit('/').next().unwrap_or(&t.path), t.peer)
        } else if uploads > 0 {
            format!("Sending {uploads} file{}", if uploads == 1 { "" } else { "s" })
        } else if errors > 0 {
            format!("{errors} folder{} need attention", if errors == 1 { "" } else { "s" })
        } else if self.engine.is_busy() {
            "Checking folders".into()
        } else {
            format!("Up to date, {online} device{} connected", if online == 1 { "" } else { "s" })
        };
        let (last, last_ms) = self.activity.lock().front().map(|a| (a.text.clone(), a.time_ms)).unwrap_or_default();
        TrayStatus { busy: !transfers.is_empty(), status, last, last_ms }
    }

    /// Pushes state changes to the web app and tray at most every 100 ms.
    pub async fn ui_pump(app: AppRef) {
        loop {
            tokio::select! {
                _ = app.ui_kick.notified() => {}
                _ = app.shutdown.cancelled() => return,
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let _ = app.ui_tx.send(UiEvent::State);
            app.update_tray();
        }
    }

    pub fn update_tray(&self) {
        let st = self.tray_status();
        if let Some(sink) = &*self.tray.lock() {
            sink(st);
        }
    }

    pub fn ui_url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }
}

pub fn new_token() -> String {
    format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

pub fn load_or_init(data_dir: &std::path::Path, name_override: Option<String>) -> Persisted {
    let path = data_dir.join("state.json");
    if let Some(mut p) = std::fs::read(&path).ok().and_then(|d| serde_json::from_slice::<Persisted>(&d).ok()) {
        if let Some(n) = name_override {
            p.name = n;
        }
        return p;
    }
    let name = name_override.unwrap_or_else(|| {
        hostname::get().ok().and_then(|h| h.into_string().ok()).unwrap_or_else(|| "My computer".into())
    });
    Persisted {
        node_id: uuid::Uuid::new_v4().to_string(),
        name,
        peers: BTreeMap::new(),
        shares: BTreeMap::new(),
        settings: Settings::default(),
        manual_addrs: Vec::new(),
    }
}
