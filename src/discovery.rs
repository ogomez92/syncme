//! Finds other SyncMe instances: mDNS and UDP broadcast on the local network,
//! `tailscale status` for tailnet devices, and addresses typed in by hand.

use crate::model::*;
use crate::peer::probe_hello;
use crate::state::{AppRef, Discovered};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

const SERVICE: &str = "_syncme._tcp.local.";

pub fn start(app: &AppRef) {
    tokio::spawn(mdns(app.clone()));
    tokio::spawn(broadcast(app.clone()));
    tokio::spawn(periodic(app.clone()));
}

pub async fn found(app: &AppRef, addr: String, via: &str) -> Option<Hello> {
    let h = probe_hello(app, &addr).await?;
    let is_new = {
        let mut d = app.discovered.lock();
        let is_new = !d.contains_key(&h.id);
        d.insert(h.id.clone(), Discovered { hello: h.clone(), addr, via: via.into(), last_seen: Some(Instant::now()) });
        is_new
    };
    if is_new {
        tracing::info!("discovered {} ({}) via {via}", h.name, h.id);
        app.changed();
    }
    Some(h)
}

fn fmt_addr(ip: IpAddr, port: u16) -> String {
    SocketAddr::new(ip, port).to_string()
}

async fn mdns(app: AppRef) {
    let daemon = match mdns_sd::ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("mDNS unavailable: {e}");
            return;
        }
    };
    let me = app.me();
    let host = format!("syncme-{}.local.", &me[..8]);
    let props = [("id", me.as_str()), ("name", &app.my_name())];
    match mdns_sd::ServiceInfo::new(SERVICE, &me, &host, "", app.port, &props[..]) {
        Ok(info) => {
            if let Err(e) = daemon.register(info.enable_addr_auto()) {
                tracing::warn!("mDNS register: {e}");
            }
        }
        Err(e) => tracing::warn!("mDNS service info: {e}"),
    }
    let rx = match daemon.browse(SERVICE) {
        Ok(rx) => rx,
        Err(e) => {
            tracing::warn!("mDNS browse: {e}");
            return;
        }
    };
    while let Ok(ev) = rx.recv_async().await {
        if let mdns_sd::ServiceEvent::ServiceResolved(info) = ev {
            if info.txt_properties.get_property_val_str("id") == Some(me.as_str()) {
                continue;
            }
            let mut ips: Vec<IpAddr> = info.addresses.iter().map(|a| a.to_ip_addr()).filter(|ip| !ip.is_loopback()).collect();
            ips.sort_by_key(|ip| !ip.is_ipv4());
            for ip in ips {
                if found(&app, fmt_addr(ip, info.port), "local network").await.is_some() {
                    break;
                }
            }
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Beacon {
    app: String,
    id: String,
    port: u16,
}

/// UDP broadcast beacon as a fallback for networks where mDNS is filtered.
async fn broadcast(app: AppRef) {
    let sock = match tokio::net::UdpSocket::bind(("0.0.0.0", DEFAULT_PORT)).await {
        Ok(s) => s,
        Err(_) => match tokio::net::UdpSocket::bind(("0.0.0.0", 0)).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("UDP discovery unavailable: {e}");
                return;
            }
        },
    };
    let _ = sock.set_broadcast(true);
    let me = app.me();
    let beacon = serde_json::to_vec(&Beacon { app: APP.into(), id: me.clone(), port: app.port }).unwrap();
    let mut buf = vec![0u8; 1024];
    let mut next_send = Instant::now();
    loop {
        if Instant::now() >= next_send {
            let _ = sock.send_to(&beacon, ("255.255.255.255", DEFAULT_PORT)).await;
            next_send = Instant::now() + Duration::from_secs(15);
        }
        let wait = next_send.saturating_duration_since(Instant::now());
        tokio::select! {
            r = sock.recv_from(&mut buf) => {
                if let Ok((n, from)) = r {
                    if let Ok(b) = serde_json::from_slice::<Beacon>(&buf[..n]) {
                        if b.app == APP && b.id != me {
                            let known = app.discovered.lock().get(&b.id).is_some_and(|d| d.last_seen.is_some_and(|t| t.elapsed() < Duration::from_secs(60)));
                            if !known {
                                let app = app.clone();
                                tokio::spawn(async move { found(&app, fmt_addr(from.ip(), b.port), "local network").await; });
                            }
                        }
                    }
                }
            }
            _ = tokio::time::sleep(wait) => {}
            _ = app.shutdown.cancelled() => return,
        }
    }
}

async fn periodic(app: AppRef) {
    loop {
        for addr in app.cfg.read().manual_addrs.clone() {
            let app = app.clone();
            tokio::spawn(async move { found(&app, addr, "address").await });
        }
        tailscale(&app).await;
        // Forget devices not seen for a while (paired devices stay listed via the config).
        let before = app.discovered.lock().len();
        app.discovered.lock().retain(|_, d| d.last_seen.is_some_and(|t| t.elapsed() < Duration::from_secs(180)));
        if app.discovered.lock().len() != before {
            app.changed();
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            _ = app.shutdown.cancelled() => return,
        }
    }
}

fn tailscale_bin() -> Option<std::path::PathBuf> {
    let candidates: &[&str] = if cfg!(windows) {
        &["C:\\Program Files\\Tailscale\\tailscale.exe", "C:\\Program Files (x86)\\Tailscale\\tailscale.exe"]
    } else if cfg!(target_os = "macos") {
        &["/Applications/Tailscale.app/Contents/MacOS/Tailscale", "/usr/local/bin/tailscale", "/opt/homebrew/bin/tailscale"]
    } else {
        &["/usr/bin/tailscale", "/usr/local/bin/tailscale"]
    };
    candidates.iter().map(std::path::PathBuf::from).find(|p| p.exists()).or_else(|| Some("tailscale".into()))
}

async fn tailscale(app: &AppRef) {
    let Some(bin) = tailscale_bin() else { return };
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(["status", "--json"]).kill_on_drop(true).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let Ok(Ok(out)) = tokio::time::timeout(Duration::from_secs(10), cmd.output()).await else { return };
    if !out.status.success() {
        return;
    }
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else { return };
    let Some(peers) = v.get("Peer").and_then(|p| p.as_object()) else { return };
    let mut probes = Vec::new();
    for p in peers.values() {
        if p.get("Online").and_then(|o| o.as_bool()) == Some(false) {
            continue;
        }
        let ips: Vec<IpAddr> = p
            .get("TailscaleIPs")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|i| i.as_str()?.parse().ok()).collect())
            .unwrap_or_default();
        if let Some(ip) = ips.iter().find(|i| i.is_ipv4()).or(ips.first()) {
            let addr = fmt_addr(*ip, DEFAULT_PORT);
            let app = app.clone();
            probes.push(async move { found(&app, addr, "Tailscale").await });
        }
    }
    futures_util::future::join_all(probes).await;
}
