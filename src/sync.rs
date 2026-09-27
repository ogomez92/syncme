//! Sync engine: one worker per local replica. A worker watches its folder,
//! indexes local changes, and pulls changes from every other replica of the
//! same folder (on this machine or on peers).

use crate::index::{self, Action, Delta, Entry, Index, RootState};
use crate::model::*;
use crate::state::{AppRef, Transfer};
use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Files fetched at the same time from one source. Enough to keep a network
/// link busy across per-file overheads (stat, fsync, rename) without
/// thrashing a spinning disk on either side.
const PARALLEL_FILES: usize = 4;
/// Write buffer per incoming file.
const WRITE_BUF: usize = 1024 * 1024;
/// How often every replica of a folder is asked for its changes.
const PULL_EVERY: Duration = Duration::from_secs(60);
/// Full re-walk of the folder as a safety net for events the watcher missed.
const FULL_SCAN_WATCHED: Duration = Duration::from_secs(5 * 60);
/// Full re-walk when there is no working watcher (the walk is the only way to notice changes).
const FULL_SCAN_UNWATCHED: Duration = Duration::from_secs(60);
/// Above this many watcher-reported paths one full walk is cheaper than many small ones.
const MAX_TARGETED_PATHS: usize = 1000;

#[derive(Clone, Debug, Default, Serialize)]
pub struct RtStatus {
    /// idle | scanning | syncing | error | missing
    pub state: String,
    pub error: Option<String>,
    pub last_sync_ms: u64,
    pub files: u64,
    pub bytes: u64,
}

pub struct ReplicaRt {
    pub share_id: String,
    pub replica: Replica,
    pub root: PathBuf,
    pub index: Mutex<Index>,
    index_path: PathBuf,
    op: tokio::sync::Mutex<()>,
    /// A full walk of the folder is due.
    dirty: AtomicBool,
    /// Paths (relative to `root`) the watcher reported since the last scan.
    pending: Mutex<BTreeSet<String>>,
    /// True once the watcher is delivering events for `root`.
    watching: AtomicBool,
    pull_all: AtomicBool,
    pull_from: Mutex<HashSet<ReplicaId>>,
    /// Last file system event; None until the first one.
    last_event: Mutex<Option<Instant>>,
    kick: Notify,
    pub status: Mutex<RtStatus>,
    cancel: CancellationToken,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
}

impl ReplicaRt {
    fn save_index(&self) {
        let idx = self.index.lock().clone();
        if let Err(e) = idx.save(&self.index_path) {
            tracing::error!("saving index {}: {e}", self.index_path.display());
        }
    }

    fn set_state(&self, state: &str, error: Option<String>) {
        let mut s = self.status.lock();
        s.state = state.into();
        s.error = error;
        let (files, bytes) = self.index.lock().live_stats();
        s.files = files;
        s.bytes = bytes;
    }

    pub fn request(&self, from: Option<&str>, rescan: bool) {
        match from {
            Some(f) => {
                self.pull_from.lock().insert(f.to_string());
            }
            None => self.pull_all.store(true, Ordering::SeqCst),
        }
        if rescan {
            self.dirty.store(true, Ordering::SeqCst);
        }
        self.kick.notify_one();
    }
}

#[derive(Default)]
pub struct Engine {
    rts: Mutex<HashMap<ReplicaId, Arc<ReplicaRt>>>,
}

impl Engine {
    pub fn get(&self, id: &str) -> Option<Arc<ReplicaRt>> {
        self.rts.lock().get(id).cloned()
    }

    pub fn of_share(&self, share_id: &str) -> Vec<Arc<ReplicaRt>> {
        self.rts.lock().values().filter(|r| r.share_id == share_id).cloned().collect()
    }


    pub fn error_count(&self) -> usize {
        self.rts.lock().values().filter(|r| matches!(r.status.lock().state.as_str(), "error" | "missing")).count()
    }

    pub fn is_busy(&self) -> bool {
        self.rts.lock().values().any(|r| matches!(r.status.lock().state.as_str(), "scanning" | "syncing"))
    }

    /// Asks every local replica of `share_id` to pull (from one replica, or all).
    pub fn request_share(&self, share_id: &str, from: Option<&str>, rescan: bool) {
        for rt in self.of_share(share_id) {
            if from != Some(rt.replica.id.as_str()) {
                rt.request(from, rescan);
            }
        }
    }

}

fn index_file(app: &AppRef, r: &Replica) -> PathBuf {
    let tag = &blake3::hash(r.path.as_bytes()).to_hex()[..12];
    app.data_dir.join("index").join(format!("{}-{tag}.json", r.id))
}

/// Starts/stops workers so they match the configured folders.
pub fn reconcile(app: &AppRef) {
    let me = app.me();
    let wanted: HashMap<ReplicaId, (String, Replica)> = app
        .cfg
        .read()
        .shares
        .values()
        .filter(|s| !s.is_deleted())
        .flat_map(|s| s.replicas.iter().filter(|r| r.node == me).map(|r| (r.id.clone(), (s.id.clone(), r.clone()))))
        .collect();

    let mut rts = app.engine.rts.lock();
    let stale: Vec<ReplicaId> = rts
        .iter()
        .filter(|(id, rt)| wanted.get(*id).is_none_or(|(_, r)| r.path != rt.replica.path))
        .map(|(id, _)| id.clone())
        .collect();
    for id in stale {
        if let Some(rt) = rts.remove(&id) {
            rt.cancel.cancel();
            rt.watcher.lock().take();
            let _ = std::fs::remove_file(&rt.index_path);
            tracing::info!("stopped replica {} ({})", id, rt.root.display());
        }
    }
    for (id, (share_id, replica)) in wanted {
        if rts.contains_key(&id) {
            continue;
        }
        let index_path = index_file(app, &replica);
        let index = Index::load(&index_path).unwrap_or_else(Index::new);
        let rt = Arc::new(ReplicaRt {
            share_id,
            root: PathBuf::from(&replica.path),
            replica,
            index: Mutex::new(index),
            index_path,
            op: tokio::sync::Mutex::new(()),
            dirty: AtomicBool::new(true),
            pending: Mutex::new(BTreeSet::new()),
            watching: AtomicBool::new(false),
            pull_all: AtomicBool::new(true),
            pull_from: Mutex::new(HashSet::new()),
            last_event: Mutex::new(None),
            kick: Notify::new(),
            status: Mutex::new(RtStatus { state: "idle".into(), ..Default::default() }),
            cancel: CancellationToken::new(),
            watcher: Mutex::new(None),
        });
        rt.set_state("idle", None);
        rts.insert(id, rt.clone());
        rt.kick.notify_one();
        tokio::spawn(worker(app.clone(), rt));
    }
}

/// Forgets the history of a replica (used after its folder was moved or recreated).
/// Safe: a fresh index only ever adds or merges, it never deletes anything elsewhere.
pub async fn reset_replica(app: &AppRef, id: &str) -> Result<()> {
    let rt = app.engine.get(id).ok_or_else(|| anyhow!("unknown location"))?;
    let _g = rt.op.lock().await;
    *rt.index.lock() = Index::new();
    rt.save_index();
    rt.request(None, true);
    Ok(())
}

async fn worker(app: AppRef, rt: Arc<ReplicaRt>) {
    let mut last_full = Instant::now();
    let mut last_pull = Instant::now();
    let mut last_purge: Option<Instant> = None;
    loop {
        tokio::select! {
            _ = rt.kick.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(60)) => {}
            _ = rt.cancel.cancelled() => return,
        }
        // Let bursts of file system events settle.
        let started = Instant::now();
        loop {
            let quiet = rt.last_event.lock().map_or(Duration::MAX, |t| t.elapsed());
            if quiet >= Duration::from_millis(400) || started.elapsed() > Duration::from_secs(3) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(400) - quiet).await;
        }
        if last_pull.elapsed() >= PULL_EVERY {
            last_pull = Instant::now();
            rt.pull_all.store(true, Ordering::SeqCst);
        }
        let full_every = if rt.watching.load(Ordering::Relaxed) { FULL_SCAN_WATCHED } else { FULL_SCAN_UNWATCHED };
        if last_full.elapsed() >= full_every {
            last_full = Instant::now();
            rt.dirty.store(true, Ordering::SeqCst);
        }
        if rt.cancel.is_cancelled() {
            return;
        }
        if let Err(e) = cycle(&app, &rt).await {
            tracing::warn!("{}: {e:#}", rt.root.display());
        }
        if last_purge.is_none_or(|t| t.elapsed() > Duration::from_secs(6 * 3600)) {
            last_purge = Some(Instant::now());
            let days = app.cfg.read().settings.trash_days;
            let root = rt.root.clone();
            tokio::task::spawn_blocking(move || index::purge_trash(&root, days));
        }
        app.changed();
    }
}

fn share_name(app: &AppRef, share_id: &str) -> String {
    app.cfg.read().shares.get(share_id).map(|s| s.name.clone()).unwrap_or_else(|| "folder".into())
}

async fn cycle(app: &AppRef, rt: &Arc<ReplicaRt>) -> Result<()> {
    let _g = rt.op.lock().await;
    let name = share_name(app, &rt.share_id);

    let root_state = {
        let idx = rt.index.lock();
        index::check_root(&rt.root, &idx)
    };
    if let RootState::Missing(msg) = root_state {
        let was = rt.status.lock().state.clone();
        rt.set_state("missing", Some(msg.clone()));
        rt.watcher.lock().take();
        rt.watching.store(false, Ordering::Relaxed);
        if was != "missing" {
            app.event("warning", format!("{name}: {msg}"));
        }
        return Ok(());
    }
    if rt.status.lock().state == "missing" {
        app.event("info", format!("{name}: folder is available again at {}", rt.root.display()));
    }
    ensure_watcher(rt);

    // Take both before scanning: anything the watcher reports from now on
    // belongs to the next cycle, so nothing can fall between two scans.
    let full = rt.dirty.swap(false, Ordering::SeqCst);
    let pending = std::mem::take(&mut *rt.pending.lock());
    if full || !pending.is_empty() {
        rt.set_state("scanning", None);
        let full = full || pending.len() > MAX_TARGETED_PATHS;
        let rt2 = rt.clone();
        let me = rt.replica.id.clone();
        let started = Instant::now();
        let res = tokio::task::spawn_blocking(move || {
            let mut idx = rt2.index.lock().clone();
            let report = if full {
                index::scan(&rt2.root, &me, &mut idx)?
            } else {
                index::scan_paths(&rt2.root, &me, &mut idx, &index::collapse_paths(pending))?
            };
            if report.touched() {
                *rt2.index.lock() = idx;
            }
            anyhow::Ok(report)
        })
        .await?;
        match res {
            Ok(report) => {
                if report.touched() {
                    rt.save_index();
                }
                let changes = &report.changes;
                if !changes.is_empty() {
                    let dels = changes.iter().filter(|c| c.deleted).count();
                    let kind = if full { "full scan" } else { "targeted scan" };
                    tracing::info!(
                        "{name}: {} local change(s) ({dels} deletion(s)) in {} ({kind}, {:?}), first: {}",
                        changes.len(),
                        rt.root.display(),
                        started.elapsed(),
                        changes[0].path
                    );
                    notify_others(app, rt);
                }
            }
            Err(e) => {
                // Whatever we skipped gets picked up by a full walk next time.
                rt.dirty.store(true, Ordering::SeqCst);
                rt.set_state("error", Some(format!("{e:#}")));
                return Err(e);
            }
        }
    }

    let sources = available_sources(app, rt);
    let wanted: Vec<Replica> = if rt.pull_all.swap(false, Ordering::SeqCst) {
        rt.pull_from.lock().clear();
        sources
    } else {
        let ids: HashSet<ReplicaId> = std::mem::take(&mut *rt.pull_from.lock());
        sources.into_iter().filter(|s| ids.contains(&s.id)).collect()
    };
    let mut error = None;
    for src in wanted {
        rt.set_state("syncing", None);
        if let Err(e) = pull(app, rt, &src).await {
            tracing::warn!("{name}: pulling from {}: {e:#}", src.path);
            error = Some(format!("{e:#}"));
            // Try again soon.
            rt.pull_from.lock().insert(src.id.clone());
        }
    }
    rt.status.lock().last_sync_ms = index::now_ms();
    match error {
        Some(e) => rt.set_state("error", Some(e)),
        None => rt.set_state("idle", None),
    }
    Ok(())
}

fn ensure_watcher(rt: &Arc<ReplicaRt>) {
    if rt.watcher.lock().is_some() {
        return;
    }
    let weak = Arc::downgrade(rt);
    let root = rt.root.clone();
    let meta = root.join(index::META_DIR);
    let handler = move |res: notify::Result<notify::Event>| {
        let Some(rt) = weak.upgrade() else { return };
        // Anything we cannot pin to paths inside the folder (an error, an
        // overflow asking for a rescan, the root itself) means a full walk.
        let mut full = false;
        let mut rels = Vec::new();
        match &res {
            Err(_) => full = true,
            Ok(ev) => {
                if ev.need_rescan() || ev.paths.is_empty() {
                    full = true;
                }
                for p in ev.paths.iter().filter(|p| !p.starts_with(&meta)) {
                    match index::to_rel(&root, p) {
                        Some(rel) => rels.push(rel),
                        None => full = true,
                    }
                }
            }
        }
        if !full && rels.is_empty() {
            return; // only our own .syncme bookkeeping
        }
        if full {
            rt.dirty.store(true, Ordering::SeqCst);
        } else {
            rt.pending.lock().extend(rels);
        }
        *rt.last_event.lock() = Some(Instant::now());
        rt.kick.notify_one();
    };
    match notify::recommended_watcher(handler) {
        Ok(mut w) => {
            use notify::Watcher;
            match w.watch(&rt.root, notify::RecursiveMode::Recursive) {
                Ok(()) => rt.watching.store(true, Ordering::Relaxed),
                Err(e) => tracing::warn!("watching {}: {e}; relying on periodic scans", rt.root.display()),
            }
            *rt.watcher.lock() = Some(w);
        }
        Err(e) => tracing::warn!("creating watcher: {e}"),
    }
}

/// Other replicas of this folder that are reachable right now.
fn available_sources(app: &AppRef, rt: &ReplicaRt) -> Vec<Replica> {
    let me = app.me();
    let Some(share) = app.cfg.read().shares.get(&rt.share_id).cloned() else { return vec![] };
    let online = app.online.lock().clone();
    share
        .replicas
        .into_iter()
        .filter(|r| r.id != rt.replica.id)
        .filter(|r| if r.node == me { app.engine.get(&r.id).is_some() } else { online.contains_key(&r.node) })
        .collect()
}

/// Tells every other replica of this folder that we have news.
fn notify_others(app: &AppRef, rt: &ReplicaRt) {
    let me = app.me();
    for other in app.engine.of_share(&rt.share_id) {
        if other.replica.id != rt.replica.id {
            other.request(Some(&rt.replica.id), false);
        }
    }
    let Some(share) = app.cfg.read().shares.get(&rt.share_id).cloned() else { return };
    let nodes: HashSet<NodeId> = share.replicas.iter().filter(|r| r.node != me).map(|r| r.node.clone()).collect();
    for node in nodes {
        let app = app.clone();
        let msg = NotifyMsg { share_id: rt.share_id.clone(), replica_id: rt.replica.id.clone() };
        tokio::spawn(async move {
            if let Err(e) = crate::peer::post_json::<_, serde_json::Value>(&app, &node, "/peer/notify", &msg).await {
                tracing::debug!("notify {node}: {e:#}");
            }
        });
    }
}

#[derive(Default)]
struct Summary {
    received: Vec<String>,
    removed: Vec<String>,
    conflicts: Vec<String>,
    skipped: Vec<String>,
}

fn source_label(app: &AppRef, src: &Replica) -> String {
    if src.node == app.me() { src.path.clone() } else { app.node_name(&src.node) }
}

async fn fetch_delta(app: &AppRef, src: &Replica, epoch: &str, since: u64) -> Result<Delta> {
    if src.node == app.me() {
        let other = app.engine.get(&src.id).ok_or_else(|| anyhow!("location not running"))?;
        if other.status.lock().state == "missing" {
            bail!("{} is not available", src.path);
        }
        let d = other.index.lock().delta(epoch, since);
        return Ok(d);
    }
    let q = format!(
        "/peer/index/{}?epoch={}&since={since}",
        src.id,
        percent_encoding::utf8_percent_encode(epoch, percent_encoding::NON_ALPHANUMERIC)
    );
    crate::peer::get_json(app, &src.node, &q).await
}

async fn pull(app: &AppRef, rt: &Arc<ReplicaRt>, src: &Replica) -> Result<()> {
    let (epoch, since) = rt.index.lock().peer_seq.get(&src.id).cloned().unwrap_or_default();
    let delta = fetch_delta(app, src, &epoch, since).await?;
    if delta.entries.is_empty() {
        rt.index.lock().peer_seq.insert(src.id.clone(), (delta.epoch, delta.seq));
        return Ok(());
    }
    let mut entries = delta.entries;
    // Directories first (parents before children), then files, then deletions
    // with the deepest paths first so directories are emptied before removal.
    entries.sort_by(|a, b| {
        let rank = |e: &Entry| match (e.deleted, e.dir) {
            (false, true) => 0,
            (false, false) => 1,
            (true, false) => 2,
            (true, true) => 3,
        };
        rank(a).cmp(&rank(b)).then_with(|| {
            if a.deleted { b.path.len().cmp(&a.path.len()) } else { a.path.cmp(&b.path) }
        })
    });

    let label = source_label(app, src);
    let name = share_name(app, &rt.share_id);
    let mut summary = Summary::default();
    // e.g. a Mac file name with characters Windows doesn't allow: skip it
    // for good instead of retrying forever. Other copies keep it.
    entries.retain(|r| {
        let ok = index::safe_join(&rt.root, &r.path).is_some();
        if !ok && !r.deleted {
            summary.skipped.push(r.path.clone());
        }
        ok
    });

    let mut failed = 0usize;
    let mut last_err = None;
    let mut changed = false;
    let mut tally = |r: &Entry, res: Result<Outcome>| match res {
        Ok(outcome) => {
            changed |= outcome.changed;
            match outcome.kind {
                Some(Kind::Received) => summary.received.push(r.path.clone()),
                Some(Kind::Removed) => summary.removed.push(r.path.clone()),
                Some(Kind::Conflict) => summary.conflicts.push(r.path.clone()),
                None => {}
            }
        }
        Err(e) => {
            failed += 1;
            tracing::warn!("{name}: {}: {e:#}", r.path);
            last_err = Some(e);
        }
    };

    // Directories and deletions depend on order; files are independent of each
    // other (paths are unique within a delta), so those are fetched in parallel.
    let files_start = entries.partition_point(|e| !e.deleted && e.dir);
    let files_end = entries.partition_point(|e| !e.deleted);
    let (dirs, files, dels) = (&entries[..files_start], &entries[files_start..files_end], &entries[files_end..]);
    for r in dirs {
        if rt.cancel.is_cancelled() {
            bail!("stopped");
        }
        tally(r, apply(app, rt, src, r, &name, &label).await);
    }
    let (name_ref, label_ref) = (name.as_str(), label.as_str());
    // A loop rather than a closure: rustc cannot prove a closure returning a
    // future that borrows its argument is Send inside this async fn.
    let mut fetches = Vec::with_capacity(files.len());
    for r in files {
        fetches.push(async move {
            if rt.cancel.is_cancelled() {
                return (r, Err(anyhow!("stopped")));
            }
            (r, apply(app, rt, src, r, name_ref, label_ref).await)
        });
    }
    let mut stream = futures_util::stream::iter(fetches).buffer_unordered(PARALLEL_FILES);
    while let Some((r, res)) = stream.next().await {
        tally(r, res);
    }
    if rt.cancel.is_cancelled() {
        bail!("stopped");
    }
    for r in dels {
        if rt.cancel.is_cancelled() {
            bail!("stopped");
        }
        tally(r, apply(app, rt, src, r, &name, &label).await);
    }
    if failed == 0 {
        rt.index.lock().peer_seq.insert(src.id.clone(), (delta.epoch, delta.seq));
    }
    rt.save_index();
    announce(app, &name, &label, &summary);
    if changed {
        notify_others(app, rt);
    }
    match last_err {
        Some(e) => Err(e.context(format!("{failed} item(s) could not be synced yet, will retry"))),
        None => Ok(()),
    }
}

fn plural_items(n: usize) -> String {
    if n == 1 { "1 item".into() } else { format!("{n} items") }
}

fn file_label(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn announce(app: &AppRef, name: &str, from: &str, s: &Summary) {
    let mut parts = Vec::new();
    match s.received.len() {
        0 => {}
        1 => parts.push(format!("received {}", file_label(&s.received[0]))),
        n => parts.push(format!("received {n} files")),
    }
    match s.removed.len() {
        0 => {}
        1 => parts.push(format!("removed {}", file_label(&s.removed[0]))),
        n => parts.push(format!("removed {n} items")),
    }
    if !parts.is_empty() {
        app.event("info", format!("{name}: {} from {from}", parts.join(" and ")));
    }
    if !s.skipped.is_empty() {
        let names: Vec<&str> = s.skipped.iter().take(3).map(|p| file_label(p)).collect();
        app.event("warning", format!("{name}: skipped {} from {from} because the name is not allowed on this computer: {}{}", plural_items(s.skipped.len()), names.join(", "), if s.skipped.len() > 3 { " and more" } else { "" }));
    }
    for c in &s.conflicts {
        app.event("warning", format!("{name}: {} was changed on both sides; both versions kept as a sync-conflict copy", file_label(c)));
    }
}

enum Kind {
    Received,
    Removed,
    Conflict,
}

struct Outcome {
    kind: Option<Kind>,
    /// Our index changed in a way other replicas should hear about.
    changed: bool,
}

impl Outcome {
    fn quiet(changed: bool) -> Self {
        Outcome { kind: None, changed }
    }
}

async fn refresh(rt: &Arc<ReplicaRt>, rel: &str) -> Result<bool> {
    let rt2 = rt.clone();
    let rel = rel.to_string();
    tokio::task::spawn_blocking(move || {
        let mut idx = rt2.index.lock();
        index::refresh_one(&rt2.root, &rt2.replica.id, &mut idx, &rel)
    })
    .await?
}

fn record(rt: &ReplicaRt, r: &Entry, prev: Option<&Entry>, local: Option<(u64, i64)>) {
    let mut idx = rt.index.lock();
    let mut e = r.clone();
    if let Some(p) = prev {
        e.vv = index::merge_vv(&p.vv, &r.vv);
    }
    e.local = local;
    e.seq = idx.next_seq();
    idx.entries.insert(e.path.clone(), e);
}

async fn apply(app: &AppRef, rt: &Arc<ReplicaRt>, src: &Replica, r: &Entry, name: &str, from: &str) -> Result<Outcome> {
    let abs = index::safe_join(&rt.root, &r.path).ok_or_else(|| anyhow!("unsafe or unsupported file name: {}", r.path))?;
    // Pick up edits the watcher hasn't reported yet before deciding anything.
    refresh(rt, &r.path).await?;
    let local = rt.index.lock().entries.get(&r.path).cloned();
    let action = index::decide(local.as_ref(), r);
    match action {
        Action::Nothing => Ok(Outcome::quiet(false)),
        Action::Adopt => {
            let stat = local.as_ref().and_then(|l| l.local);
            let keep = if r.deleted || r.dir { None } else { stat };
            record(rt, r, local.as_ref(), keep);
            Ok(Outcome::quiet(true))
        }
        Action::KeepLocal => {
            let mut idx = rt.index.lock();
            if let Some(mut l) = idx.entries.get(&r.path).cloned() {
                let merged = index::merge_vv(&l.vv, &r.vv);
                l.vv = merged.clone();
                idx.bump(&rt.replica.id, l, Some(&merged));
            }
            tracing::info!("{name}: kept {} (changed here, deleted or replaced elsewhere)", r.path);
            Ok(Outcome::quiet(true))
        }
        Action::MkDir => {
            if abs.is_file() {
                index::move_to_trash(&rt.root, &r.path, &abs)?;
            }
            std::fs::create_dir_all(&abs).with_context(|| format!("creating {}", abs.display()))?;
            record(rt, r, local.as_ref(), None);
            Ok(Outcome::quiet(true))
        }
        Action::Delete => {
            if abs.is_dir() {
                match std::fs::remove_dir(&abs) {
                    Ok(()) => {}
                    Err(_) if abs.read_dir().map(|mut d| d.next().is_some()).unwrap_or(false) => {
                        // Still has content we must not lose: keep the directory.
                        let mut idx = rt.index.lock();
                        if let Some(mut l) = idx.entries.get(&r.path).cloned() {
                            let merged = index::merge_vv(&l.vv, &r.vv);
                            l.vv = merged.clone();
                            idx.bump(&rt.replica.id, l, Some(&merged));
                        }
                        return Ok(Outcome::quiet(true));
                    }
                    Err(e) => return Err(e).context("removing folder"),
                }
            } else if abs.exists() {
                index::move_to_trash(&rt.root, &r.path, &abs).with_context(|| format!("moving {} to trash", abs.display()))?;
            }
            record(rt, r, local.as_ref(), None);
            Ok(Outcome { kind: if r.dir { None } else { Some(Kind::Removed) }, changed: true })
        }
        Action::Download | Action::ConflictDownload => {
            let tmp = download(app, rt, src, r, name, from).await?;
            // The file might have been edited while we downloaded.
            if refresh(rt, &r.path).await? {
                let _ = tokio::fs::remove_file(&tmp).await;
                bail!("{} changed locally during download; will re-evaluate", r.path);
            }
            let mut kind = Kind::Received;
            if action == Action::ConflictDownload && abs.exists() {
                let dest = index::conflict_name(&abs, &app.my_name());
                std::fs::rename(&abs, &dest).with_context(|| format!("keeping conflict copy {}", dest.display()))?;
                kind = Kind::Conflict;
                rt.dirty.store(true, Ordering::SeqCst);
            }
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if abs.is_dir() {
                let _ = tokio::fs::remove_file(&tmp).await;
                bail!("{} is a folder here; not replacing it with a file", r.path);
            }
            std::fs::rename(&tmp, &abs).with_context(|| format!("replacing {} (is it open in another program?)", abs.display()))?;
            let meta = std::fs::metadata(&abs)?;
            record(rt, r, local.as_ref(), Some((meta.len(), index::mtime_ns(&meta))));
            Ok(Outcome { kind: Some(kind), changed: true })
        }
    }
}

/// Fetches the content of `r` from `src` into a temp file inside our `.syncme`
/// folder (same volume, so the final rename is atomic) and verifies its hash.
async fn download(app: &AppRef, rt: &Arc<ReplicaRt>, src: &Replica, r: &Entry, name: &str, from: &str) -> Result<PathBuf> {
    let tmp_dir = rt.root.join(index::META_DIR).join("tmp");
    tokio::fs::create_dir_all(&tmp_dir).await?;
    let tmp = tmp_dir.join(format!("{}.syncme-part", uuid::Uuid::new_v4().simple()));
    let tid = app.transfer_start(Transfer { share: name.into(), path: r.path.clone(), peer: from.into(), done: 0, total: r.size, upload: false });
    let res = download_inner(app, src, r, &tmp, tid).await;
    app.transfer_end(tid);
    match res {
        Ok(()) => Ok(tmp),
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(e)
        }
    }
}

async fn download_inner(app: &AppRef, src: &Replica, r: &Entry, tmp: &Path, tid: u64) -> Result<()> {
    let mut hasher = blake3::Hasher::new();
    // Every write on a tokio file is a hop to the blocking pool, and network
    // chunks are small; buffer so the disk sees large writes.
    let mut out = tokio::io::BufWriter::with_capacity(WRITE_BUF, tokio::fs::File::create(tmp).await?);
    let mut done = 0u64;
    if src.node == app.me() {
        let other = app.engine.get(&src.id).ok_or_else(|| anyhow!("location not running"))?;
        let from = index::safe_join(&other.root, &r.path).ok_or_else(|| anyhow!("bad path"))?;
        let mut f = tokio::fs::File::open(&from).await.with_context(|| format!("opening {}", from.display()))?;
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            use tokio::io::AsyncReadExt;
            let n = f.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n]).await?;
            done += n as u64;
            app.transfer_progress(tid, done);
        }
    } else {
        let q = format!("/peer/file/{}?path={}", src.id, percent_encoding::utf8_percent_encode(&r.path, percent_encoding::NON_ALPHANUMERIC));
        let resp = crate::peer::get_raw(app, &src.node, &q).await?;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            hasher.update(&chunk);
            out.write_all(&chunk).await?;
            done += chunk.len() as u64;
            app.transfer_progress(tid, done);
        }
    }
    out.flush().await?;
    // Content must be on disk before the rename: a crash in between would
    // otherwise leave a truncated file that the next scan spreads as an edit.
    out.get_ref().sync_all().await?;
    drop(out);
    let got = hasher.finalize().to_hex().to_string();
    if got != r.hash {
        bail!("{} changed at the source while copying; will retry", r.path);
    }
    let mt = filetime::FileTime::from_unix_time(r.mtime.div_euclid(1_000_000_000), r.mtime.rem_euclid(1_000_000_000) as u32);
    filetime::set_file_mtime(tmp, mt)?;
    Ok(())
}
