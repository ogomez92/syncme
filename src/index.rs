//! Per-replica file index with version vectors, local scanning and the
//! decision rules for applying remote changes.
//!
//! Safety rules (nothing is lost unless someone explicitly deleted it):
//! * A deletion is only applied if it *dominates* the local version, i.e. the
//!   deleting side had already seen exactly what we have. Files that exist
//!   locally but were never synced are always kept and spread to the others.
//! * Concurrent edits keep both versions: the loser is renamed to a
//!   `.sync-conflict-...` copy which then syncs like any other file.
//! * A folder whose root or `.syncme` marker disappeared (unplugged drive,
//!   deleted folder) is never scanned as "everything deleted".
//! * Files removed because of a remote deletion go to `.syncme/trash`.

use crate::model::{ReplicaId, Vv};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const META_DIR: &str = ".syncme";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    pub path: String,
    #[serde(default)]
    pub dir: bool,
    #[serde(default)]
    pub deleted: bool,
    pub size: u64,
    /// Modification time (ns since epoch) as set by the replica that wrote the content.
    pub mtime: i64,
    /// blake3 of the content (last known content for tombstones, empty for dirs).
    #[serde(default)]
    pub hash: String,
    pub vv: Vv,
    /// Local sequence number of the last change to this entry in this index.
    pub seq: u64,
    /// Replica that produced this content.
    pub by: ReplicaId,
    /// What the file looked like on this replica's disk when last indexed (size, mtime ns).
    #[serde(default)]
    pub local: Option<(u64, i64)>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Index {
    pub epoch: String,
    pub seq: u64,
    pub entries: BTreeMap<String, Entry>,
    /// For each source replica: (its epoch, last seq we fully applied).
    #[serde(default)]
    pub peer_seq: BTreeMap<ReplicaId, (String, u64)>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Delta {
    pub epoch: String,
    pub seq: u64,
    pub entries: Vec<Entry>,
}

impl Index {
    pub fn new() -> Self {
        Index { epoch: uuid::Uuid::new_v4().to_string(), seq: 0, entries: BTreeMap::new(), peer_seq: BTreeMap::new() }
    }

    pub fn load(path: &Path) -> Option<Self> {
        let data = std::fs::read(path).ok()?;
        serde_json::from_slice(&data).ok()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Changes since `since` (or everything if the caller knew another epoch).
    pub fn delta(&self, epoch: &str, since: u64) -> Delta {
        let since = if epoch == self.epoch { since } else { 0 };
        let mut entries: Vec<Entry> = self.entries.values().filter(|e| e.seq > since).cloned().collect();
        for e in &mut entries {
            e.local = None;
        }
        Delta { epoch: self.epoch.clone(), seq: self.seq, entries }
    }

    pub fn live_stats(&self) -> (u64, u64) {
        self.entries.values().filter(|e| !e.deleted && !e.dir).fold((0, 0), |(n, b), e| (n + 1, b + e.size))
    }

    /// Records a new local version of `path` authored by `me`.
    pub fn bump(&mut self, me: &str, mut e: Entry, prev_vv: Option<&Vv>) -> Entry {
        let mut vv = prev_vv.cloned().unwrap_or_default();
        let c = vv.get(me).copied().unwrap_or(0);
        // Time-based counter so a replica whose index was reset still produces
        // versions newer than everything it produced before.
        vv.insert(me.to_string(), (c + 1).max(now_ms()));
        e.vv = vv;
        e.by = me.to_string();
        e.seq = self.next_seq();
        self.entries.insert(e.path.clone(), e.clone());
        e
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum VvOrd {
    Equal,
    Greater,
    Less,
    Concurrent,
}

pub fn cmp_vv(a: &Vv, b: &Vv) -> VvOrd {
    let (mut gt, mut lt) = (false, false);
    for k in a.keys().chain(b.keys()) {
        let x = a.get(k).copied().unwrap_or(0);
        let y = b.get(k).copied().unwrap_or(0);
        if x > y {
            gt = true;
        }
        if x < y {
            lt = true;
        }
    }
    match (gt, lt) {
        (false, false) => VvOrd::Equal,
        (true, false) => VvOrd::Greater,
        (false, true) => VvOrd::Less,
        (true, true) => VvOrd::Concurrent,
    }
}

pub fn merge_vv(a: &Vv, b: &Vv) -> Vv {
    let mut out = a.clone();
    for (k, v) in b {
        let e = out.entry(k.clone()).or_insert(0);
        *e = (*e).max(*v);
    }
    out
}

/// What to do with a remote entry given our local entry for the same path.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Action {
    /// Nothing to do (we already have it, or ours wins and the other side will adopt it).
    Nothing,
    /// Same content/state; only record the merged version vector.
    Adopt,
    /// Fetch the remote content.
    Download,
    /// Fetch the remote content, first renaming our version to a conflict copy.
    ConflictDownload,
    /// Remove our copy (moved to trash).
    Delete,
    /// Create the directory.
    MkDir,
    /// Keep our copy and publish it as newer than the remote (e.g. edit vs delete).
    KeepLocal,
}

pub fn decide(local: Option<&Entry>, remote: &Entry) -> Action {
    let Some(l) = local else {
        return if remote.deleted {
            Action::Adopt
        } else if remote.dir {
            Action::MkDir
        } else {
            Action::Download
        };
    };
    match cmp_vv(&remote.vv, &l.vv) {
        VvOrd::Equal | VvOrd::Less => Action::Nothing,
        VvOrd::Greater => {
            if remote.deleted {
                if l.deleted { Action::Adopt } else { Action::Delete }
            } else if remote.dir {
                if l.dir && !l.deleted { Action::Adopt } else { Action::MkDir }
            } else if !l.deleted && !l.dir && l.hash == remote.hash {
                Action::Adopt
            } else {
                Action::Download
            }
        }
        VvOrd::Concurrent => {
            match (l.deleted, remote.deleted) {
                (true, true) => return Action::Adopt,
                // Edit beats delete, on either side.
                (false, true) => return Action::KeepLocal,
                (true, false) => return if remote.dir { Action::MkDir } else { Action::Download },
                _ => {}
            }
            match (l.dir, remote.dir) {
                (true, true) => Action::Adopt,
                (false, false) if l.hash == remote.hash => Action::Adopt,
                (false, false) => {
                    // Deterministic winner so every replica agrees; the loser's
                    // side keeps its version as a conflict copy.
                    if (remote.mtime, &remote.by) > (l.mtime, &l.by) { Action::ConflictDownload } else { Action::Nothing }
                }
                // File vs directory: keep what we have and let the directory side win later.
                (true, false) => Action::KeepLocal,
                (false, true) => Action::Nothing,
            }
        }
    }
}

pub fn is_ignored_name(name: &str) -> bool {
    name == META_DIR
        || name == ".DS_Store"
        || name.eq_ignore_ascii_case("Thumbs.db")
        || name.eq_ignore_ascii_case("desktop.ini")
        || name.starts_with("~$")
        || name.starts_with(".~lock.")
        || name.ends_with(".syncme-part")
}

/// Validates a relative path received from another replica and converts it to a local path.
pub fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() || rel.starts_with('/') || rel.contains('\\') || rel.contains('\0') {
        return None;
    }
    let mut p = root.to_path_buf();
    for (i, comp) in rel.split('/').enumerate() {
        if comp.is_empty() || comp == "." || comp == ".." {
            return None;
        }
        if i == 0 && comp == META_DIR {
            return None;
        }
        if cfg!(windows) && !valid_windows_name(comp) {
            return None;
        }
        p.push(comp);
    }
    Some(p)
}

fn valid_windows_name(comp: &str) -> bool {
    if comp.chars().any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || (c as u32) < 32) {
        return false;
    }
    if comp.ends_with('.') || comp.ends_with(' ') {
        return false;
    }
    let stem = comp.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"];
    if reserved.contains(&stem.as_str()) {
        return false;
    }
    if (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.len() == 4 && stem.as_bytes()[3].is_ascii_digit() {
        return false;
    }
    true
}

pub fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

pub fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().to_hex().to_string())
}

fn to_rel(root: &Path, p: &Path) -> Option<String> {
    let rel = p.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for c in rel.components() {
        parts.push(c.as_os_str().to_str()?.to_string());
    }
    if parts.is_empty() { None } else { Some(parts.join("/")) }
}

/// Result of checking the replica root before touching anything.
#[derive(Debug, PartialEq, Eq)]
pub enum RootState {
    Ok,
    /// Root or marker missing while we have history: refuse to scan/sync.
    Missing(String),
}

/// Ensures root + marker exist for a fresh replica, and refuses to operate on a
/// replica whose folder vanished (which would otherwise look like "delete everything").
pub fn check_root(root: &Path, idx: &Index) -> RootState {
    let marker = root.join(META_DIR);
    let has_history = !idx.entries.is_empty();
    if marker.is_dir() {
        return RootState::Ok;
    }
    if has_history {
        return RootState::Missing(if root.exists() {
            format!("The .syncme marker inside {} is gone. If you moved or recreated this folder, use Reset to sync it again safely.", root.display())
        } else {
            format!("{} is missing (drive unplugged or folder moved?). Syncing is paused for this location.", root.display())
        });
    }
    match std::fs::create_dir_all(&marker) {
        Ok(()) => {
            hide_marker(&marker);
            RootState::Ok
        }
        Err(e) => RootState::Missing(format!("Cannot create {}: {e}", root.display())),
    }
}

#[cfg(windows)]
fn hide_marker(p: &Path) {
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new("attrib").arg("+h").arg(p).creation_flags(0x0800_0000).status();
}
#[cfg(not(windows))]
fn hide_marker(_: &Path) {}

pub struct ScanChange {
    pub path: String,
    pub deleted: bool,
}

/// Walks the replica folder and records local changes. Blocking; run on a blocking thread.
pub fn scan(root: &Path, me: &str, idx: &mut Index) -> Result<Vec<ScanChange>> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut unreadable: Vec<String> = Vec::new();
    let mut changes = Vec::new();

    let walker = walkdir::WalkDir::new(root).follow_links(false).into_iter().filter_entry(|e| {
        e.depth() == 0 || !e.file_name().to_str().map(is_ignored_name).unwrap_or(true)
    });
    for item in walker {
        let item = match item {
            Ok(i) => i,
            Err(err) => {
                if let Some(p) = err.path().and_then(|p| to_rel(root, p)) {
                    unreadable.push(p);
                } else if err.path() == Some(root) {
                    anyhow::bail!("cannot read {}: {err}", root.display());
                }
                continue;
            }
        };
        if item.depth() == 0 {
            continue;
        }
        let Some(rel) = to_rel(root, item.path()) else { continue };
        let ft = item.file_type();
        if ft.is_symlink() {
            continue;
        }
        seen.insert(rel.clone());
        let prev = idx.entries.get(&rel).cloned();
        if ft.is_dir() {
            if prev.as_ref().is_some_and(|p| p.dir && !p.deleted) {
                continue;
            }
            let e = Entry { path: rel.clone(), dir: true, deleted: false, size: 0, mtime: 0, hash: String::new(), vv: Vv::new(), seq: 0, by: String::new(), local: None };
            idx.bump(me, e, prev.as_ref().map(|p| &p.vv));
            changes.push(ScanChange { path: rel, deleted: false });
            continue;
        }
        let meta = match item.metadata() {
            Ok(m) => m,
            Err(_) => {
                unreadable.push(rel);
                continue;
            }
        };
        let stat = (meta.len(), mtime_ns(&meta));
        if let Some(p) = &prev {
            if !p.deleted && !p.dir && p.local == Some(stat) {
                continue;
            }
        }
        let hash = match hash_file(item.path()) {
            Ok(h) => h,
            Err(_) => {
                // Locked or unreadable right now: keep whatever we had, try again later.
                unreadable.push(rel);
                continue;
            }
        };
        if let Some(p) = idx.entries.get_mut(&rel) {
            if !p.deleted && !p.dir && p.hash == hash {
                p.local = Some(stat);
                continue;
            }
        }
        let e = Entry { path: rel.clone(), dir: false, deleted: false, size: stat.0, mtime: stat.1, hash, vv: Vv::new(), seq: 0, by: String::new(), local: Some(stat) };
        idx.bump(me, e, prev.as_ref().map(|p| &p.vv));
        changes.push(ScanChange { path: rel, deleted: false });
    }

    let gone: Vec<Entry> = idx
        .entries
        .values()
        .filter(|e| !e.deleted && !seen.contains(&e.path))
        .filter(|e| !unreadable.iter().any(|u| e.path == *u || e.path.starts_with(&format!("{u}/"))))
        .cloned()
        .collect();
    for mut e in gone {
        let vv = e.vv.clone();
        e.deleted = true;
        e.local = None;
        idx.bump(me, e.clone(), Some(&vv));
        changes.push(ScanChange { path: e.path, deleted: true });
    }
    Ok(changes)
}

/// Re-indexes one path right before we overwrite or delete it, so an edit the
/// watcher has not reported yet is never lost. Returns true if something changed.
pub fn refresh_one(root: &Path, me: &str, idx: &mut Index, rel: &str) -> Result<bool> {
    let Some(abs) = safe_join(root, rel) else { return Ok(false) };
    let prev = idx.entries.get(rel).cloned();
    let meta = match std::fs::symlink_metadata(&abs) {
        Ok(m) => Some(m),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).context("stat"),
    };
    match (meta, prev) {
        (None, None) => Ok(false),
        (None, Some(p)) if p.deleted => Ok(false),
        (None, Some(mut p)) => {
            let vv = p.vv.clone();
            p.deleted = true;
            p.local = None;
            idx.bump(me, p, Some(&vv));
            Ok(true)
        }
        (Some(m), prev) if m.is_dir() => {
            if prev.as_ref().is_some_and(|p| p.dir && !p.deleted) {
                return Ok(false);
            }
            let e = Entry { path: rel.to_string(), dir: true, deleted: false, size: 0, mtime: 0, hash: String::new(), vv: Vv::new(), seq: 0, by: String::new(), local: None };
            idx.bump(me, e, prev.as_ref().map(|p| &p.vv));
            Ok(true)
        }
        (Some(m), prev) => {
            let stat = (m.len(), mtime_ns(&m));
            if let Some(p) = &prev {
                if !p.deleted && !p.dir && p.local == Some(stat) {
                    return Ok(false);
                }
            }
            let hash = hash_file(&abs).context("hash")?;
            if let Some(p) = idx.entries.get_mut(rel) {
                if !p.deleted && !p.dir && p.hash == hash {
                    p.local = Some(stat);
                    return Ok(false);
                }
            }
            let e = Entry { path: rel.to_string(), dir: false, deleted: false, size: stat.0, mtime: stat.1, hash, vv: Vv::new(), seq: 0, by: String::new(), local: Some(stat) };
            idx.bump(me, e, prev.as_ref().map(|p| &p.vv));
            Ok(true)
        }
    }
}

/// `report.docx` -> `report.sync-conflict-20260927-101500-NitroPC.docx`
pub fn conflict_name(abs: &Path, device: &str) -> PathBuf {
    let stamp = chrono_like_stamp();
    let device: String = device.chars().map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' }).collect();
    let file = abs.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let (stem, ext) = match file.rfind('.') {
        Some(i) if i > 0 => (&file[..i], &file[i..]),
        _ => (file, ""),
    };
    let mut n = 0;
    loop {
        let extra = if n == 0 { String::new() } else { format!("-{n}") };
        let cand = abs.with_file_name(format!("{stem}.sync-conflict-{stamp}-{device}{extra}{ext}"));
        if !cand.exists() {
            return cand;
        }
        n += 1;
    }
}

/// Local-time-free UTC stamp YYYYMMDD-HHMMSS without pulling in a date crate.
pub fn chrono_like_stamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", rem / 3600, (rem % 3600) / 60, rem % 60)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Moves a file or directory into `<root>/.syncme/trash/<stamp>/<rel>` instead of deleting it.
pub fn move_to_trash(root: &Path, rel: &str, abs: &Path) -> std::io::Result<()> {
    let dest = root.join(META_DIR).join("trash").join(chrono_like_stamp()).join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
    if let Some(p) = dest.parent() {
        std::fs::create_dir_all(p)?;
    }
    let dest = if dest.exists() { conflict_name(&dest, "dup") } else { dest };
    std::fs::rename(abs, &dest)
}

/// Removes trash older than `days`.
pub fn purge_trash(root: &Path, days: u64) {
    let trash = root.join(META_DIR).join("trash");
    let Ok(rd) = std::fs::read_dir(&trash) else { return };
    let cutoff = SystemTime::now() - std::time::Duration::from_secs(days * 86400);
    for e in rd.flatten() {
        if e.metadata().and_then(|m| m.modified()).map(|t| t < cutoff).unwrap_or(false) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vv(pairs: &[(&str, u64)]) -> Vv {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn file(path: &str, hash: &str, v: Vv, mtime: i64, by: &str) -> Entry {
        Entry { path: path.into(), dir: false, deleted: false, size: 1, mtime, hash: hash.into(), vv: v, seq: 1, by: by.into(), local: None }
    }

    #[test]
    fn vv_ordering() {
        assert_eq!(cmp_vv(&vv(&[("a", 1)]), &vv(&[("a", 1)])), VvOrd::Equal);
        assert_eq!(cmp_vv(&vv(&[("a", 2)]), &vv(&[("a", 1)])), VvOrd::Greater);
        assert_eq!(cmp_vv(&vv(&[("a", 1)]), &vv(&[("a", 1), ("b", 1)])), VvOrd::Less);
        assert_eq!(cmp_vv(&vv(&[("a", 2)]), &vv(&[("b", 1)])), VvOrd::Concurrent);
    }

    #[test]
    fn preexisting_same_file_is_adopted_without_transfer() {
        let l = file("x", "H", vv(&[("B", 5)]), 10, "B");
        let r = file("x", "H", vv(&[("A", 7)]), 20, "A");
        assert_eq!(decide(Some(&l), &r), Action::Adopt);
    }

    #[test]
    fn preexisting_different_file_keeps_both() {
        let l = file("x", "L", vv(&[("B", 5)]), 10, "B");
        let r = file("x", "R", vv(&[("A", 7)]), 20, "A");
        assert_eq!(decide(Some(&l), &r), Action::ConflictDownload);
        // And the other side agrees the remote (newer mtime) wins, so it does nothing.
        assert_eq!(decide(Some(&r), &l), Action::Nothing);
    }

    #[test]
    fn unseen_tombstone_never_deletes_local_file() {
        let l = file("x", "L", vv(&[("B", 1)]), 10, "B");
        let mut r = file("x", "OLD", vv(&[("A", 9)]), 5, "A");
        r.deleted = true;
        assert_eq!(decide(Some(&l), &r), Action::KeepLocal);
    }

    #[test]
    fn explicit_delete_of_synced_file_applies() {
        let l = file("x", "H", vv(&[("A", 3)]), 10, "A");
        let mut r = file("x", "H", vv(&[("A", 4)]), 10, "A");
        r.deleted = true;
        assert_eq!(decide(Some(&l), &r), Action::Delete);
    }

    #[test]
    fn delete_vs_edit_edit_wins() {
        let mut l = file("x", "H", vv(&[("A", 4)]), 10, "A");
        l.deleted = true;
        let r = file("x", "H2", vv(&[("A", 3), ("B", 8)]), 30, "B");
        assert_eq!(decide(Some(&l), &r), Action::Download);
    }

    #[test]
    fn newer_remote_edit_downloads() {
        let l = file("x", "H", vv(&[("A", 3)]), 10, "A");
        let r = file("x", "H2", vv(&[("A", 3), ("B", 1)]), 30, "B");
        assert_eq!(decide(Some(&l), &r), Action::Download);
        assert_eq!(decide(Some(&r), &l), Action::Nothing);
    }

    #[test]
    fn path_validation() {
        let root = Path::new("root");
        assert!(safe_join(root, "a/b.txt").is_some());
        assert!(safe_join(root, "../x").is_none());
        assert!(safe_join(root, "/x").is_none());
        assert!(safe_join(root, "a//b").is_none());
        assert!(safe_join(root, ".syncme/x").is_none());
        #[cfg(windows)]
        {
            assert!(safe_join(root, "a:b").is_none());
            assert!(safe_join(root, "CON.txt").is_none());
        }
    }

    #[test]
    fn stamp_format() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20723), (2026, 9, 27));
    }
}
