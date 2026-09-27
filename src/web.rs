//! Local web app + JSON API. Only reachable from this computer (loopback),
//! with Host/Origin checks against DNS rebinding and cross-site requests.

use crate::model::*;
use crate::state::{AppRef, Settings, UiEvent};
use axum::{
    Json, Router,
    extract::{ConnectInfo, Path as AxPath, Query, Request, State, ws::{Message, WebSocket, WebSocketUpgrade}},
    http::{Method, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::Ordering;

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const APP_CSS: &str = include_str!("../web/app.css");

pub fn routes(app: AppRef) -> Router<AppRef> {
    Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route("/app.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], APP_JS) }))
        .route("/app.css", get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], APP_CSS) }))
        .route("/api/state", get(state_h))
        .route("/api/ws", get(ws_h))
        .route("/api/shares", post(save_share))
        .route("/api/shares/{id}/delete", post(delete_share))
        .route("/api/shares/{id}/rescan", post(rescan_share))
        .route("/api/replicas/{id}/reset", post(reset_replica))
        .route("/api/replicas/{id}/open", post(open_replica))
        .route("/api/pair", post(pair))
        .route("/api/pair/accept", post(pair_accept))
        .route("/api/pair/decline", post(pair_decline))
        .route("/api/peers/add", post(add_peer))
        .route("/api/peers/{id}/unpair", post(unpair))
        .route("/api/settings", post(save_settings))
        .route("/api/name", post(save_name))
        .route("/api/browse", get(browse))
        .layer(middleware::from_fn_with_state(app, local_only))
}

async fn local_only(State(app): State<AppRef>, ConnectInfo(remote): ConnectInfo<SocketAddr>, req: Request, next: Next) -> Response {
    if !remote.ip().is_loopback() {
        return (StatusCode::FORBIDDEN, "SyncMe's web app is only available on this computer").into_response();
    }
    let port = app.port;
    let allowed_hosts = [format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")];
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    if !allowed_hosts.iter().any(|h| h == host) {
        return (StatusCode::FORBIDDEN, "bad host").into_response();
    }
    if let Some(origin) = req.headers().get(header::ORIGIN).and_then(|h| h.to_str().ok()) {
        if !allowed_hosts.iter().any(|h| origin == format!("http://{h}")) {
            return (StatusCode::FORBIDDEN, "bad origin").into_response();
        }
    }
    if req.method() != Method::GET {
        let json = req.headers().get(header::CONTENT_TYPE).and_then(|h| h.to_str().ok()).is_some_and(|c| c.starts_with("application/json"));
        if !json {
            return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "expected JSON").into_response();
        }
    }
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    h.insert("x-frame-options", "DENY".parse().unwrap());
    h.insert("x-content-type-options", "nosniff".parse().unwrap());
    h.insert(header::CONTENT_SECURITY_POLICY, "default-src 'self'; connect-src 'self'; frame-ancestors 'none'".parse().unwrap());
    resp
}

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn bad(msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg.into() })))
}

async fn state_h(State(app): State<AppRef>) -> Json<Value> {
    Json(snapshot(&app))
}

pub fn snapshot(app: &AppRef) -> Value {
    let cfg = app.cfg.read().clone();
    let online = app.online.lock().clone();
    let me = cfg.node_id.clone();
    let devices: Vec<Value> = cfg
        .peers
        .values()
        .map(|p| json!({ "id": p.id, "name": p.name, "os": p.os, "online": online.contains_key(&p.id), "addr": online.get(&p.id).or(p.addrs.first()) }))
        .collect();
    let discovered: Vec<Value> = app
        .discovered
        .lock()
        .values()
        .filter(|d| !cfg.peers.contains_key(&d.hello.id))
        .map(|d| json!({ "id": d.hello.id, "name": d.hello.name, "os": d.hello.os, "addr": d.addr, "via": d.via }))
        .collect();
    let incoming: Vec<Value> = app.incoming.lock().values().map(|i| json!({ "id": i.hello.id, "name": i.hello.name, "os": i.hello.os, "addr": i.addr })).collect();
    let outgoing: Vec<String> = app.outgoing.lock().keys().cloned().collect();
    let node_name = |id: &str| if id == me { cfg.name.clone() } else { cfg.peers.get(id).map(|p| p.name.clone()).unwrap_or_else(|| "Unpaired device".into()) };
    let shares: Vec<Value> = cfg
        .shares
        .values()
        .filter(|s| !s.is_deleted() && s.nodes().contains(&me))
        .map(|s| {
            let replicas: Vec<Value> = s
                .replicas
                .iter()
                .map(|r| {
                    let local = r.node == me;
                    let status = if local { app.engine.get(&r.id).map(|rt| serde_json::to_value(&*rt.status.lock()).unwrap()) } else { None };
                    json!({ "id": r.id, "node": r.node, "node_name": node_name(&r.node), "path": r.path, "local": local, "online": local || online.contains_key(&r.node), "status": status })
                })
                .collect();
            json!({ "id": s.id, "name": s.name, "rev": s.rev, "replicas": replicas })
        })
        .collect();
    let transfers: Vec<Value> = app.transfers.lock().values().map(|t| serde_json::to_value(t).unwrap()).collect();
    let activity: Vec<Value> = app.activity.lock().iter().take(50).map(|a| serde_json::to_value(a).unwrap()).collect();
    let tray = app.tray_status();
    json!({
        "me": { "id": me, "name": cfg.name, "os": std::env::consts::OS, "port": app.port, "sep": std::path::MAIN_SEPARATOR.to_string(),
                "home": dirs::home_dir().map(|h| h.to_string_lossy().into_owned()) },
        "settings": cfg.settings,
        "devices": devices,
        "discovered": discovered,
        "incoming": incoming,
        "outgoing": outgoing,
        "shares": shares,
        "transfers": transfers,
        "activity": activity,
        "status": { "text": tray.status, "busy": tray.busy },
    })
}

async fn ws_h(State(app): State<AppRef>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| ws_loop(app, socket))
}

async fn ws_loop(app: AppRef, mut socket: WebSocket) {
    let mut rx = app.ui_tx.subscribe();
    let mut visible = false;
    loop {
        tokio::select! {
            ev = rx.recv() => {
                let ev = match ev {
                    Ok(ev) => ev,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => UiEvent::State,
                    Err(_) => break,
                };
                let text = serde_json::to_string(&ev).unwrap();
                if socket.send(Message::Text(text.into())).await.is_err() { break; }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(t))) => {
                        if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            if let Some(vis) = v.get("visible").and_then(|v| v.as_bool()) {
                                if vis != visible {
                                    visible = vis;
                                    if vis { app.ui_visible.fetch_add(1, Ordering::SeqCst); } else { app.ui_visible.fetch_sub(1, Ordering::SeqCst); }
                                }
                            }
                        }
                    }
                    Some(Ok(_)) => {}
                    _ => break,
                }
            }
        }
    }
    if visible {
        app.ui_visible.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Deserialize)]
struct ReplicaIn {
    #[serde(default)]
    id: Option<String>,
    node: String,
    path: String,
}

#[derive(Deserialize)]
struct ShareIn {
    #[serde(default)]
    id: Option<String>,
    name: String,
    replicas: Vec<ReplicaIn>,
}

fn normalize_path(p: &str) -> String {
    let t = p.trim();
    if t.len() > 1 && (t.ends_with('/') || t.ends_with('\\')) && !t.ends_with(":\\") {
        t[..t.len() - 1].to_string()
    } else {
        t.to_string()
    }
}

fn is_within(a: &str, b: &str, windows: bool) -> bool {
    let norm = |s: &str| {
        let s = s.replace('\\', "/");
        let s = if windows { s.to_lowercase() } else { s };
        if s.ends_with('/') { s } else { format!("{s}/") }
    };
    let (a, b) = (norm(a), norm(b));
    a.starts_with(&b) || b.starts_with(&a)
}

fn looks_absolute(path: &str, os: &str) -> bool {
    if os == "windows" {
        let b = path.as_bytes();
        (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')) || path.starts_with("\\\\")
    } else {
        path.starts_with('/')
    }
}

async fn save_share(State(app): State<AppRef>, Json(input): Json<ShareIn>) -> ApiResult {
    let me = app.me();
    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err(bad("Give the folder a name."));
    }
    let (peers, old) = {
        let cfg = app.cfg.read();
        (cfg.peers.clone(), input.id.as_ref().and_then(|id| cfg.shares.get(id).cloned()))
    };
    if input.id.is_some() && old.is_none() {
        return Err(bad("That folder no longer exists."));
    }
    let mut replicas: Vec<Replica> = Vec::new();
    for r in input.replicas {
        let path = normalize_path(&r.path);
        let device = if r.node == me { app.my_name() } else { peers.get(&r.node).map(|p| p.name.clone()).ok_or_else(|| bad("One of the devices is not paired anymore."))? };
        if path.is_empty() {
            return Err(bad(format!("Enter a folder path on {device}.")));
        }
        let os = if r.node == me { std::env::consts::OS.to_string() } else { peers[&r.node].os.clone() };
        let path = if os == "windows" { path.replace('/', "\\") } else { path };
        if !looks_absolute(&path, &os) {
            return Err(bad(format!("The path {path} on {device} must be a full path, for example {}.", if os == "windows" { "C:\\Users\\name\\Documents" } else { "/Users/name/Documents" })));
        }
        for other in replicas.iter().filter(|o| o.node == r.node) {
            if is_within(&other.path, &path, os == "windows") {
                return Err(bad(format!("{path} and {} on {device} overlap. Locations of the same folder cannot be inside each other.", other.path)));
            }
        }
        // Keep the replica id (and so its sync history) when the location is unchanged.
        let id = r
            .id
            .filter(|id| old.as_ref().is_some_and(|o| o.replicas.iter().any(|x| &x.id == id && x.node == r.node && x.path == path)))
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        replicas.push(Replica { id, node: r.node, path });
    }
    if !replicas.iter().any(|r| r.node == me) {
        return Err(bad("Choose at least one folder on this computer."));
    }
    if replicas.len() < 2 {
        return Err(bad("Choose at least one other place to sync with: another device, or a second folder on this computer."));
    }
    // Refuse to nest inside another synced folder on this machine.
    {
        let cfg = app.cfg.read();
        for s in cfg.shares.values().filter(|s| !s.is_deleted() && Some(&s.id) != input.id.as_ref()) {
            for a in s.replicas.iter().filter(|x| x.node == me) {
                for b in replicas.iter().filter(|x| x.node == me) {
                    if is_within(&a.path, &b.path, cfg!(windows)) {
                        return Err(bad(format!("{} overlaps with {} which is already synced as {}.", b.path, a.path, s.name)));
                    }
                }
            }
        }
    }
    let share = match old.clone() {
        Some(o) => {
            let new_nodes: BTreeSet<NodeId> = replicas.iter().map(|r| r.node.clone()).collect();
            let mut former: BTreeSet<NodeId> = o.nodes().union(&o.former_nodes).cloned().collect();
            former.retain(|n| !new_nodes.contains(n));
            Share { id: o.id, name, replicas, former_nodes: former, rev: o.rev + 1, updated_by: me.clone() }
        }
        None => Share { id: uuid::Uuid::new_v4().to_string(), name, replicas, former_nodes: BTreeSet::new(), rev: 1, updated_by: me.clone() },
    };
    app.cfg.write().shares.insert(share.id.clone(), share.clone());
    app.save();
    crate::sync::reconcile(&app);
    crate::peer::push_share(&app, &share, &BTreeSet::new());
    let offline: HashSet<String> = share.nodes().into_iter().filter(|n| *n != me && !app.online.lock().contains_key(n)).map(|n| app.node_name(&n)).collect();
    let mut msg = format!("Saved {}.", share.name);
    if !offline.is_empty() {
        let mut v: Vec<_> = offline.into_iter().collect();
        v.sort();
        msg.push_str(&format!(" {} will get the change when connected.", v.join(", ")));
    }
    app.event("info", msg);
    Ok(Json(json!({ "id": share.id })))
}

async fn delete_share(State(app): State<AppRef>, AxPath(id): AxPath<String>) -> ApiResult {
    let me = app.me();
    let share = {
        let mut cfg = app.cfg.write();
        let s = cfg.shares.get_mut(&id).ok_or_else(|| bad("Unknown folder"))?;
        let mut former = s.former_nodes.clone();
        former.extend(s.nodes());
        s.replicas.clear();
        s.former_nodes = former;
        s.rev += 1;
        s.updated_by = me;
        s.clone()
    };
    app.save();
    crate::sync::reconcile(&app);
    crate::peer::push_share(&app, &share, &BTreeSet::new());
    app.event("info", format!("Stopped syncing {} on all devices. No files were deleted.", share.name));
    Ok(Json(json!({})))
}

async fn rescan_share(State(app): State<AppRef>, AxPath(id): AxPath<String>) -> ApiResult {
    app.engine.request_share(&id, None, true);
    Ok(Json(json!({})))
}

async fn reset_replica(State(app): State<AppRef>, AxPath(id): AxPath<String>) -> ApiResult {
    crate::sync::reset_replica(&app, &id).await.map_err(|e| bad(e.to_string()))?;
    app.event("info", "Location reset. It will merge with the other copies without deleting anything.");
    Ok(Json(json!({})))
}

async fn open_replica(State(app): State<AppRef>, AxPath(id): AxPath<String>) -> ApiResult {
    let rt = app.engine.get(&id).ok_or_else(|| bad("Unknown location"))?;
    open::that_detached(&rt.root).map_err(|e| bad(e.to_string()))?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct IdIn {
    id: String,
}

async fn pair(State(app): State<AppRef>, Json(i): Json<IdIn>) -> ApiResult {
    crate::peer::request_pair(&app, &i.id).await.map_err(|e| bad(format!("Could not send the pairing request: {e:#}")))?;
    Ok(Json(json!({})))
}

async fn pair_accept(State(app): State<AppRef>, Json(i): Json<IdIn>) -> ApiResult {
    crate::peer::accept_pair(&app, &i.id).await.map_err(|e| bad(format!("Could not pair: {e:#}")))?;
    Ok(Json(json!({})))
}

async fn pair_decline(State(app): State<AppRef>, Json(i): Json<IdIn>) -> ApiResult {
    app.incoming.lock().remove(&i.id);
    app.changed();
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct AddrIn {
    address: String,
}

async fn add_peer(State(app): State<AppRef>, Json(i): Json<AddrIn>) -> ApiResult {
    let a = i.address.trim().trim_start_matches("http://").trim_end_matches('/').to_string();
    if a.is_empty() {
        return Err(bad("Enter an address, for example 192.168.1.20 or my-mac.tailnet.ts.net."));
    }
    let with_port = if a.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) && !a.ends_with(']') { a.clone() } else { format!("{a}:{DEFAULT_PORT}") };
    // Resolve names so the stored address is stable.
    let resolved = tokio::net::lookup_host(with_port.clone()).await.ok().and_then(|mut it| it.find(|x| x.is_ipv4())).map(|x| x.to_string()).unwrap_or(with_port.clone());
    let h = crate::discovery::found(&app, resolved.clone(), "address").await.ok_or_else(|| bad(format!("No SyncMe found at {with_port}. Check that it is running there and that the firewall allows it.")))?;
    {
        let mut cfg = app.cfg.write();
        if !cfg.manual_addrs.contains(&resolved) {
            cfg.manual_addrs.push(resolved);
        }
    }
    app.save();
    app.event("info", format!("Found {} at {with_port}", h.name));
    Ok(Json(json!({ "id": h.id, "name": h.name })))
}

async fn unpair(State(app): State<AppRef>, AxPath(id): AxPath<String>) -> ApiResult {
    crate::peer::unpair(&app, &id).await;
    Ok(Json(json!({})))
}

async fn save_settings(State(app): State<AppRef>, Json(s): Json<Settings>) -> ApiResult {
    let login_changed = app.cfg.read().settings.start_at_login != s.start_at_login;
    if login_changed {
        crate::autostart::set(s.start_at_login).map_err(|e| bad(format!("Could not change start at login: {e:#}")))?;
    }
    app.cfg.write().settings = s;
    app.save();
    app.changed();
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct NameIn {
    name: String,
}

async fn save_name(State(app): State<AppRef>, Json(n): Json<NameIn>) -> ApiResult {
    let name = n.name.trim();
    if name.is_empty() || name.len() > 60 {
        return Err(bad("Enter a name up to 60 characters."));
    }
    app.cfg.write().name = name.to_string();
    app.save();
    app.changed();
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct BrowseQ {
    node: String,
    #[serde(default)]
    path: String,
}

async fn browse(State(app): State<AppRef>, Query(q): Query<BrowseQ>) -> ApiResult {
    let res = if q.node == app.me() {
        let path = q.path.clone();
        tokio::task::spawn_blocking(move || crate::peer::browse_local(&path)).await.map_err(|e| bad(e.to_string()))?
    } else {
        let url = format!("/peer/browse?path={}", percent_encoding::utf8_percent_encode(&q.path, percent_encoding::NON_ALPHANUMERIC));
        crate::peer::get_json::<BrowseResult>(&app, &q.node, &url).await.map_err(|e| bad(format!("Could not list folders on that device: {e:#}")))?
    };
    Ok(Json(serde_json::to_value(res).unwrap()))
}
