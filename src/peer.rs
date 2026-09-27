//! Device-to-device protocol (HTTP under /peer). Everything except `hello` and
//! `pair/request` requires the per-pair token created when two devices pair.

use crate::index::{self, Delta};
use crate::model::*;
use crate::state::{AppRef, Incoming, PeerRec, Transfer, new_token};
use anyhow::{Result, anyhow, bail};
use axum::{
    Json, Router,
    body::Body,
    extract::{ConnectInfo, Path as AxPath, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::time::Duration;

// ---------------------------------------------------------------- client side

fn target(app: &AppRef, node: &str) -> Result<(String, String)> {
    let token = app.cfg.read().peers.get(node).map(|p| p.token.clone()).ok_or_else(|| anyhow!("not paired"))?;
    let addr = app
        .online
        .lock()
        .get(node)
        .cloned()
        .or_else(|| app.cfg.read().peers.get(node).and_then(|p| p.addrs.first().cloned()))
        .ok_or_else(|| anyhow!("no known address"))?;
    Ok((addr, token))
}

fn authed(app: &AppRef, rb: reqwest::RequestBuilder, token: &str) -> reqwest::RequestBuilder {
    rb.bearer_auth(token).header("x-syncme-node", app.me()).header("x-syncme-port", app.port.to_string())
}

async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    bail!("{status}: {text}")
}

pub async fn get_json<T: DeserializeOwned>(app: &AppRef, node: &str, path: &str) -> Result<T> {
    let (addr, token) = target(app, node)?;
    let rb = app.http.get(format!("http://{addr}{path}")).timeout(Duration::from_secs(60));
    Ok(check(authed(app, rb, &token).send().await?).await?.json().await?)
}

pub async fn post_json<B: Serialize, T: DeserializeOwned>(app: &AppRef, node: &str, path: &str, body: &B) -> Result<T> {
    let (addr, token) = target(app, node)?;
    let rb = app.http.post(format!("http://{addr}{path}")).json(body).timeout(Duration::from_secs(30));
    Ok(check(authed(app, rb, &token).send().await?).await?.json().await?)
}

pub async fn get_raw(app: &AppRef, node: &str, path: &str) -> Result<reqwest::Response> {
    let (addr, token) = target(app, node)?;
    let rb = app.http.get(format!("http://{addr}{path}"));
    check(authed(app, rb, &token).send().await?).await
}

/// Unauthenticated identity probe used by discovery.
pub async fn probe_hello(app: &AppRef, addr: &str) -> Option<Hello> {
    let resp = app.http.get(format!("http://{addr}/peer/hello")).timeout(Duration::from_secs(3)).send().await.ok()?;
    let h: Hello = resp.json().await.ok()?;
    (h.app == APP && h.id != app.me()).then_some(h)
}

async fn ping(app: &AppRef, addr: &str, token: &str) -> Option<Hello> {
    let rb = app.http.get(format!("http://{addr}/peer/ping")).timeout(Duration::from_secs(4));
    let resp = authed(app, rb, token).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json().await.ok()
}

// ------------------------------------------------------------ folder configs

/// Applies folder definitions received from `from`. Returns true if anything changed.
pub fn apply_shares(app: &AppRef, from: &str, shares: Vec<Share>) -> bool {
    let me = app.me();
    let mut changed = false;
    let mut messages = Vec::new();
    {
        let mut cfg = app.cfg.write();
        for s in shares {
            if !s.involves(from) && s.updated_by != from {
                continue;
            }
            let old = cfg.shares.get(&s.id).cloned();
            if !s.involves(&me) && old.is_none() {
                continue;
            }
            if old.as_ref().is_some_and(|o| !s.is_newer_than(o)) {
                continue;
            }
            let mine = |sh: &Share| sh.replicas.iter().filter(|r| r.node == me).map(|r| r.path.clone()).collect::<BTreeSet<_>>();
            let before = old.as_ref().map(mine).unwrap_or_default();
            let after = mine(&s);
            let who = cfg.peers.get(&s.updated_by).map(|p| p.name.clone()).unwrap_or_else(|| "another device".into());
            if before.is_empty() && !after.is_empty() {
                let paths: Vec<_> = after.iter().cloned().collect();
                messages.push(format!("{who} shared the folder {} with this computer. It syncs to {}", s.name, paths.join(" and ")));
            } else if !before.is_empty() && after.is_empty() {
                messages.push(format!("{} is no longer synced on this computer (changed by {who}). Your files were kept.", s.name));
            } else if before != after {
                let paths: Vec<_> = after.iter().cloned().collect();
                messages.push(format!("{who} changed where {} syncs on this computer: now {}", s.name, paths.join(" and ")));
            }
            cfg.shares.insert(s.id.clone(), s);
            changed = true;
        }
    }
    if changed {
        app.save();
        crate::sync::reconcile(app);
        for m in messages {
            app.event("info", m);
        }
        app.changed();
    }
    changed
}

/// Sends every folder definition involving `node` and merges what it sends back.
pub async fn exchange_shares(app: &AppRef, node: &str) {
    let ours: Vec<Share> = app.cfg.read().shares.values().filter(|s| s.involves(node)).cloned().collect();
    match post_json::<_, Vec<Share>>(app, node, "/peer/shares", &ours).await {
        Ok(theirs) => {
            apply_shares(app, node, theirs);
        }
        Err(e) => tracing::debug!("share exchange with {node}: {e:#}"),
    }
}

/// Sends a changed folder definition to every device that needs to know.
pub fn push_share(app: &AppRef, share: &Share, extra: &BTreeSet<NodeId>) {
    let me = app.me();
    let mut nodes: BTreeSet<NodeId> = share.nodes();
    nodes.extend(share.former_nodes.iter().cloned());
    nodes.extend(extra.iter().cloned());
    nodes.remove(&me);
    for node in nodes {
        if !app.online.lock().contains_key(&node) {
            continue; // will be exchanged when it connects
        }
        let app = app.clone();
        let share = share.clone();
        tokio::spawn(async move {
            match post_json::<_, Vec<Share>>(&app, &node, "/peer/shares", &vec![share]).await {
                Ok(theirs) => {
                    apply_shares(&app, &node, theirs);
                }
                Err(e) => tracing::warn!("sending folder settings to {node}: {e:#}"),
            }
        });
    }
}

// ------------------------------------------------------------ online monitor

async fn became_online(app: &AppRef, node: &str, addr: &str) {
    let was = app.online.lock().insert(node.to_string(), addr.to_string());
    {
        let mut cfg = app.cfg.write();
        if let Some(p) = cfg.peers.get_mut(node) {
            p.addrs.retain(|a| a != addr);
            p.addrs.insert(0, addr.to_string());
            p.addrs.truncate(8);
        }
    }
    if was.is_none() {
        app.save();
        app.event("info", format!("{} connected", app.node_name(node)));
        exchange_shares(app, node).await;
        let shares: Vec<String> = app.cfg.read().shares.values().filter(|s| s.nodes().contains(node)).map(|s| s.id.clone()).collect();
        for s in shares {
            app.engine.request_share(&s, None, false);
        }
    }
}

fn became_offline(app: &AppRef, node: &str) {
    if app.online.lock().remove(node).is_some() {
        app.event("info", format!("{} disconnected", app.node_name(node)));
    }
}

pub async fn monitor(app: AppRef) {
    loop {
        let peers: Vec<PeerRec> = app.cfg.read().peers.values().cloned().collect();
        let checks = peers.into_iter().map(|p| {
            let app = app.clone();
            async move {
                let mut cands: Vec<String> = Vec::new();
                if let Some(a) = app.online.lock().get(&p.id) {
                    cands.push(a.clone());
                }
                cands.extend(p.addrs.iter().cloned());
                if let Some(d) = app.discovered.lock().get(&p.id) {
                    cands.push(d.addr.clone());
                }
                cands.dedup();
                let mut seen = std::collections::HashSet::new();
                for addr in cands.into_iter().filter(|a| seen.insert(a.clone())) {
                    if let Some(h) = ping(&app, &addr, &p.token).await {
                        if h.id == p.id {
                            if h.name != p.name {
                                if let Some(pr) = app.cfg.write().peers.get_mut(&p.id) {
                                    pr.name = h.name.clone();
                                }
                                app.save();
                            }
                            became_online(&app, &p.id, &addr).await;
                            return;
                        }
                    }
                }
                became_offline(&app, &p.id);
            }
        });
        futures_util::future::join_all(checks).await;
        app.changed();
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(10)) => {}
            _ = app.shutdown.cancelled() => return,
        }
    }
}

// ---------------------------------------------------------------- pairing

pub async fn request_pair(app: &AppRef, node: &str) -> Result<()> {
    let d = app.discovered.lock().get(node).cloned().ok_or_else(|| anyhow!("That device is no longer visible"))?;
    let token = new_token();
    app.outgoing.lock().insert(node.to_string(), token.clone());
    let req = PairRequest { hello: app.hello(), token };
    let resp = app.http.post(format!("http://{}/peer/pair/request", d.addr)).json(&req).timeout(Duration::from_secs(5)).send().await?;
    check(resp).await?;
    app.event("info", format!("Pairing request sent to {}. Accept it in SyncMe on that computer.", d.hello.name));
    Ok(())
}

pub async fn accept_pair(app: &AppRef, node: &str) -> Result<()> {
    let inc = app.incoming.lock().get(node).cloned().ok_or_else(|| anyhow!("No pending request from that device"))?;
    let rb = app.http.post(format!("http://{}/peer/pair/confirm", inc.addr)).json(&app.hello()).timeout(Duration::from_secs(5));
    check(authed(app, rb, &inc.token).send().await?).await?;
    app.incoming.lock().remove(node);
    app.cfg.write().peers.insert(
        node.to_string(),
        PeerRec { id: node.to_string(), name: inc.hello.name.clone(), os: inc.hello.os.clone(), token: inc.token.clone(), addrs: vec![inc.addr.clone()] },
    );
    app.save();
    app.event("info", format!("Paired with {}", inc.hello.name));
    became_online(app, node, &inc.addr).await;
    Ok(())
}

pub async fn unpair(app: &AppRef, node: &str) {
    let _ = post_json::<_, serde_json::Value>(app, node, "/peer/unpair", &serde_json::json!({})).await;
    let name = app.node_name(node);
    app.cfg.write().peers.remove(node);
    app.online.lock().remove(node);
    app.save();
    app.event("info", format!("Unpaired {name}"));
}

// ---------------------------------------------------------------- server side

pub fn routes() -> Router<AppRef> {
    Router::new()
        .route("/peer/hello", get(hello))
        .route("/peer/pair/request", post(pair_request))
        .route("/peer/pair/confirm", post(pair_confirm))
        .route("/peer/unpair", post(unpair_h))
        .route("/peer/ping", get(ping_h))
        .route("/peer/shares", post(shares_h))
        .route("/peer/notify", post(notify_h))
        .route("/peer/index/{replica}", get(index_h))
        .route("/peer/file/{replica}", get(file_h))
        .route("/peer/browse", get(browse_h))
}

type HResult<T> = std::result::Result<T, (StatusCode, String)>;

fn err(code: StatusCode, msg: impl Into<String>) -> (StatusCode, String) {
    (code, msg.into())
}

fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Authenticates a peer request and remembers the address it came from.
fn auth(app: &AppRef, headers: &HeaderMap, remote: SocketAddr) -> HResult<NodeId> {
    let node = headers.get("x-syncme-node").and_then(|v| v.to_str().ok()).ok_or_else(|| err(StatusCode::UNAUTHORIZED, "missing node"))?;
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "missing token"))?;
    let ok = app.cfg.read().peers.get(node).is_some_and(|p| ct_eq(&p.token, token));
    if !ok {
        return Err(err(StatusCode::UNAUTHORIZED, "not paired"));
    }
    if let Some(port) = headers.get("x-syncme-port").and_then(|v| v.to_str().ok()).and_then(|p| p.parse::<u16>().ok()) {
        let addr = SocketAddr::new(remote.ip(), port).to_string();
        let mut online = app.online.lock();
        if !online.contains_key(node) {
            // The monitor announces the connection on its next round.
            drop(online);
            let mut cfg = app.cfg.write();
            if let Some(p) = cfg.peers.get_mut(node) {
                if !p.addrs.contains(&addr) {
                    p.addrs.insert(0, addr);
                }
            }
        } else {
            online.entry(node.to_string()).or_insert(addr);
        }
    }
    Ok(node.to_string())
}

async fn hello(State(app): State<AppRef>) -> Json<Hello> {
    Json(app.hello())
}

async fn pair_request(State(app): State<AppRef>, ConnectInfo(remote): ConnectInfo<SocketAddr>, Json(req): Json<PairRequest>) -> HResult<Json<serde_json::Value>> {
    if req.hello.app != APP || req.hello.id == app.me() || req.token.len() < 32 {
        return Err(err(StatusCode::BAD_REQUEST, "invalid request"));
    }
    let addr = SocketAddr::new(remote.ip(), req.hello.port).to_string();
    let name = req.hello.name.clone();
    {
        let mut inc = app.incoming.lock();
        if inc.len() > 20 && !inc.contains_key(&req.hello.id) {
            return Err(err(StatusCode::TOO_MANY_REQUESTS, "too many pending requests"));
        }
        inc.insert(req.hello.id.clone(), Incoming { hello: req.hello, addr, token: req.token });
    }
    app.event("info", format!("{name} wants to pair with this computer. Open SyncMe to accept or decline."));
    Ok(Json(serde_json::json!({"status": "pending"})))
}

async fn pair_confirm(State(app): State<AppRef>, ConnectInfo(remote): ConnectInfo<SocketAddr>, headers: HeaderMap, Json(h): Json<Hello>) -> HResult<Json<serde_json::Value>> {
    let token = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
    let expected = app.outgoing.lock().get(&h.id).cloned();
    if !expected.is_some_and(|t| ct_eq(&t, token)) {
        return Err(err(StatusCode::UNAUTHORIZED, "no matching pairing request"));
    }
    app.outgoing.lock().remove(&h.id);
    let addr = SocketAddr::new(remote.ip(), h.port).to_string();
    app.cfg.write().peers.insert(h.id.clone(), PeerRec { id: h.id.clone(), name: h.name.clone(), os: h.os.clone(), token: token.to_string(), addrs: vec![addr.clone()] });
    app.save();
    app.event("info", format!("Paired with {}", h.name));
    let app2 = app.clone();
    tokio::spawn(async move { became_online(&app2, &h.id, &addr).await });
    Ok(Json(serde_json::json!({"status": "paired"})))
}

async fn unpair_h(State(app): State<AppRef>, ConnectInfo(remote): ConnectInfo<SocketAddr>, headers: HeaderMap) -> HResult<Json<serde_json::Value>> {
    let node = auth(&app, &headers, remote)?;
    let name = app.node_name(&node);
    app.cfg.write().peers.remove(&node);
    app.online.lock().remove(&node);
    app.save();
    app.event("info", format!("{name} unpaired from this computer"));
    Ok(Json(serde_json::json!({})))
}

async fn ping_h(State(app): State<AppRef>, ConnectInfo(remote): ConnectInfo<SocketAddr>, headers: HeaderMap) -> HResult<Json<Hello>> {
    auth(&app, &headers, remote)?;
    Ok(Json(app.hello()))
}

async fn shares_h(State(app): State<AppRef>, ConnectInfo(remote): ConnectInfo<SocketAddr>, headers: HeaderMap, Json(shares): Json<Vec<Share>>) -> HResult<Json<Vec<Share>>> {
    let node = auth(&app, &headers, remote)?;
    let ids: BTreeSet<String> = shares.iter().map(|s| s.id.clone()).collect();
    apply_shares(&app, &node, shares);
    // Reply with our view of the same folders plus any others involving the caller.
    let back: Vec<Share> = app.cfg.read().shares.values().filter(|s| ids.contains(&s.id) || s.involves(&node)).cloned().collect();
    Ok(Json(back))
}

async fn notify_h(State(app): State<AppRef>, ConnectInfo(remote): ConnectInfo<SocketAddr>, headers: HeaderMap, Json(msg): Json<NotifyMsg>) -> HResult<Json<serde_json::Value>> {
    let node = auth(&app, &headers, remote)?;
    let member = app.cfg.read().shares.get(&msg.share_id).is_some_and(|s| s.replicas.iter().any(|r| r.id == msg.replica_id && r.node == node));
    if member {
        app.engine.request_share(&msg.share_id, Some(&msg.replica_id), false);
    }
    Ok(Json(serde_json::json!({})))
}

/// The caller must hold a replica of the same folder as `replica`, which must be ours.
fn authorize_replica(app: &AppRef, node: &str, replica: &str) -> HResult<std::sync::Arc<crate::sync::ReplicaRt>> {
    let rt = app.engine.get(replica).ok_or_else(|| err(StatusCode::NOT_FOUND, "unknown location"))?;
    let allowed = app.cfg.read().shares.get(&rt.share_id).is_some_and(|s| s.replicas.iter().any(|r| r.node == node));
    if !allowed {
        return Err(err(StatusCode::FORBIDDEN, "not a member of this folder"));
    }
    if rt.status.lock().state == "missing" {
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, "folder not available"));
    }
    Ok(rt)
}

#[derive(Deserialize)]
struct IndexQ {
    #[serde(default)]
    epoch: String,
    #[serde(default)]
    since: u64,
}

async fn index_h(State(app): State<AppRef>, ConnectInfo(remote): ConnectInfo<SocketAddr>, headers: HeaderMap, AxPath(replica): AxPath<String>, Query(q): Query<IndexQ>) -> HResult<Json<Delta>> {
    let node = auth(&app, &headers, remote)?;
    let rt = authorize_replica(&app, &node, &replica)?;
    let d = rt.index.lock().delta(&q.epoch, q.since);
    Ok(Json(d))
}

#[derive(Deserialize)]
struct FileQ {
    path: String,
}

/// Ends the upload entry in the status when the response body is dropped.
struct UploadGuard(AppRef, u64);
impl Drop for UploadGuard {
    fn drop(&mut self) {
        self.0.transfer_end(self.1);
    }
}

async fn file_h(State(app): State<AppRef>, ConnectInfo(remote): ConnectInfo<SocketAddr>, headers: HeaderMap, AxPath(replica): AxPath<String>, Query(q): Query<FileQ>) -> HResult<Response> {
    let node = auth(&app, &headers, remote)?;
    let rt = authorize_replica(&app, &node, &replica)?;
    let abs = index::safe_join(&rt.root, &q.path).ok_or_else(|| err(StatusCode::BAD_REQUEST, "bad path"))?;
    let f = tokio::fs::File::open(&abs).await.map_err(|e| err(StatusCode::NOT_FOUND, e.to_string()))?;
    let len = f.metadata().await.map(|m| m.len()).unwrap_or(0);
    let share = app.cfg.read().shares.get(&rt.share_id).map(|s| s.name.clone()).unwrap_or_default();
    let tid = app.transfer_start(Transfer { share, path: q.path.clone(), peer: app.node_name(&node), done: 0, total: len, upload: true });
    let guard = UploadGuard(app.clone(), tid);
    let stream = tokio_util::io::ReaderStream::with_capacity(f, 256 * 1024);
    let stream = futures_util::StreamExt::map(stream, move |c| {
        let _keep = &guard;
        c
    });
    Ok(([(header::CONTENT_LENGTH, len.to_string())], Body::from_stream(stream)).into_response())
}

#[derive(Deserialize)]
pub struct BrowseQ {
    #[serde(default)]
    pub path: String,
}

async fn browse_h(State(app): State<AppRef>, ConnectInfo(remote): ConnectInfo<SocketAddr>, headers: HeaderMap, Query(q): Query<BrowseQ>) -> HResult<Json<BrowseResult>> {
    auth(&app, &headers, remote)?;
    Ok(Json(browse_local(&q.path)))
}

pub fn roots() -> Vec<BrowseDir> {
    let mut out = Vec::new();
    if cfg!(windows) {
        for l in b'A'..=b'Z' {
            let p = format!("{}:\\", l as char);
            if std::path::Path::new(&p).exists() {
                out.push(BrowseDir { name: p.clone(), path: p });
            }
        }
    } else {
        out.push(BrowseDir { name: "/".into(), path: "/".into() });
        if let Ok(rd) = std::fs::read_dir("/Volumes") {
            for e in rd.flatten() {
                let p = e.path();
                out.push(BrowseDir { name: e.file_name().to_string_lossy().into_owned(), path: p.to_string_lossy().into_owned() });
            }
        }
    }
    out
}

pub fn browse_local(path: &str) -> BrowseResult {
    let home = dirs::home_dir().map(|h| h.to_string_lossy().into_owned()).unwrap_or_default();
    let mut res = BrowseResult { path: path.to_string(), home: home.clone(), roots: roots(), sep: std::path::MAIN_SEPARATOR.to_string(), ..Default::default() };
    let target = if path.is_empty() { home } else if cfg!(windows) { path.replace('/', "\\") } else { path.to_string() };
    res.path = target.clone();
    let p = std::path::Path::new(&target);
    res.parent = p.parent().map(|x| x.to_string_lossy().into_owned()).filter(|s| !s.is_empty());
    match std::fs::read_dir(p) {
        Ok(rd) => {
            let mut dirs: Vec<BrowseDir> = rd
                .flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .filter_map(|e| {
                    let name = e.file_name().to_str()?.to_string();
                    (name != index::META_DIR && !name.starts_with('$')).then(|| BrowseDir { path: e.path().to_string_lossy().into_owned(), name })
                })
                .collect();
            dirs.sort_by_key(|d| d.name.to_lowercase());
            res.dirs = dirs;
        }
        Err(e) => res.error = Some(format!("Cannot open {target}: {e}")),
    }
    res
}
