//! Subcommands that talk to the running SyncMe through its local web API,
//! so a headless server can be managed from a terminal.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::Path;

pub const HELP: &str = "Commands (they talk to the SyncMe already running on this computer):
  devices            list paired devices and devices found nearby
  requests           list pairing requests waiting for an answer
  accept [device]    accept a pairing request (name or id; optional if only one is waiting)
  decline [device]   decline a pairing request
  pair <device>      ask a device to pair (name or id from 'devices', or an address)
";

pub fn run(cmd: &[String], data_dir: &Path, port: Option<u16>) -> Result<()> {
    let port = port.unwrap_or_else(|| running_port(data_dir));
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let api = Api { http: reqwest::Client::new(), base: format!("http://127.0.0.1:{port}") };
        let arg = cmd.get(1).map(String::as_str);
        match cmd[0].as_str() {
            "devices" => devices(&api).await,
            "requests" => requests(&api).await,
            "accept" => answer(&api, arg, true).await,
            "decline" => answer(&api, arg, false).await,
            "pair" => pair(&api, arg).await,
            other => bail!("unknown command '{other}'\n\n{HELP}"),
        }
    })
}

/// The port the instance using `data_dir` listens on, or the default when none is running there.
fn running_port(data_dir: &Path) -> u16 {
    let held = std::fs::OpenOptions::new()
        .write(true)
        .open(data_dir.join("syncme.lock"))
        .is_ok_and(|f| f.try_lock().is_err());
    let recorded = std::fs::read_to_string(data_dir.join("port")).ok().and_then(|p| p.trim().parse().ok());
    match recorded {
        Some(p) if held => p,
        _ => crate::model::DEFAULT_PORT,
    }
}

struct Api {
    http: reqwest::Client,
    base: String,
}

impl Api {
    async fn state(&self) -> Result<Value> {
        let resp = self.http.get(format!("{}/api/state", self.base)).send().await.with_context(|| self.unreachable())?;
        check(resp).await
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        let resp = self.http.post(format!("{}{path}", self.base)).json(&body).send().await.with_context(|| self.unreachable())?;
        check(resp).await
    }

    fn unreachable(&self) -> String {
        format!("SyncMe isn't answering at {}. Is it running? (use --port or --data-dir to pick another instance)", self.base)
    }
}

async fn check(resp: reqwest::Response) -> Result<Value> {
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("{}", body["error"].as_str().map(String::from).unwrap_or_else(|| status.to_string()));
    }
    Ok(body)
}

fn list(state: &Value, key: &str) -> Vec<Value> {
    state[key].as_array().cloned().unwrap_or_default()
}

fn describe(d: &Value) -> String {
    format!("{}  ({}, {})  id {}", s(&d["name"]), s(&d["os"]), s(&d["addr"]), s(&d["id"]))
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("?")
}

/// Finds the one entry whose name (any case), id, or id prefix matches.
fn find<'a>(items: &'a [Value], query: Option<&str>, what: &str) -> Result<&'a Value> {
    let Some(q) = query else {
        return match items {
            [one] => Ok(one),
            [] => bail!("there are no {what}"),
            _ => bail!("more than one of the {what}; say which one:\n{}", items.iter().map(|d| format!("  {}", describe(d))).collect::<Vec<_>>().join("\n")),
        };
    };
    let exact: Vec<&Value> = items.iter().filter(|d| s(&d["id"]) == q || s(&d["name"]).eq_ignore_ascii_case(q)).collect();
    let hits = if exact.is_empty() { items.iter().filter(|d| s(&d["id"]).starts_with(q)).collect() } else { exact };
    match hits[..] {
        [one] => Ok(one),
        [] => bail!("'{q}' is not among the {what}"),
        _ => bail!("'{q}' matches more than one of the {what}; use the id instead"),
    }
}

async fn devices(api: &Api) -> Result<()> {
    let st = api.state().await?;
    println!("This computer: {}  id {}", s(&st["me"]["name"]), s(&st["me"]["id"]));
    let paired = list(&st, "devices");
    println!("\nPaired ({}):", paired.len());
    for d in &paired {
        let online = if d["online"].as_bool() == Some(true) { "connected" } else { "offline" };
        println!("  {}  [{online}]", describe(d));
    }
    let nearby = list(&st, "discovered");
    println!("\nFound nearby, not paired ({}):", nearby.len());
    for d in &nearby {
        println!("  {}  via {}", describe(d), s(&d["via"]));
    }
    let incoming = list(&st, "incoming");
    if !incoming.is_empty() {
        println!("\n{} pairing request(s) waiting; see 'syncme requests'.", incoming.len());
    }
    Ok(())
}

async fn requests(api: &Api) -> Result<()> {
    let incoming = list(&api.state().await?, "incoming");
    if incoming.is_empty() {
        println!("No pairing requests waiting.");
    }
    for d in &incoming {
        println!("{}", describe(d));
    }
    Ok(())
}

async fn answer(api: &Api, query: Option<&str>, accept: bool) -> Result<()> {
    let incoming = list(&api.state().await?, "incoming");
    let d = find(&incoming, query, "pairing requests waiting")?;
    let path = if accept { "/api/pair/accept" } else { "/api/pair/decline" };
    api.post(path, json!({ "id": d["id"] })).await?;
    println!("{} {}.", if accept { "Paired with" } else { "Declined" }, s(&d["name"]));
    Ok(())
}

async fn pair(api: &Api, query: Option<&str>) -> Result<()> {
    let Some(q) = query else { bail!("usage: syncme pair <device>") };
    let st = api.state().await?;
    if list(&st, "devices").iter().any(|d| s(&d["id"]) == q || s(&d["name"]).eq_ignore_ascii_case(q)) {
        bail!("already paired with {q}");
    }
    let nearby = list(&st, "discovered");
    let d = match find(&nearby, Some(q), "devices found nearby") {
        Ok(d) => d.clone(),
        // Not discovered: treat it as an address, like "Add a device by address" in the web app.
        Err(_) => api.post("/api/peers/add", json!({ "address": q })).await?,
    };
    let id = d["id"].as_str().context("no device id returned")?;
    api.post("/api/pair", json!({ "id": id })).await?;
    println!("Pairing request sent to {}. Accept it on that device.", s(&d["name"]));
    Ok(())
}
