//! Clipboard sync. With the setting on, text or an image copied on this
//! device goes to every connected device that also has it on, and what they
//! copy lands here. Files are never sent: copying files in a file manager
//! puts neither text nor an image on the clipboard, so nothing happens.
//!
//! All talking to the OS clipboard happens on one thread (`Service`), which
//! polls for changes and writes what other devices send. `Tracker` decides
//! what is new and keeps a device from sending back what it just received.
//! Text travels as UTF-8 inside JSON and is never re-encoded; images travel
//! as PNG so nothing is lost and the wire format is the same on every OS.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

/// Longest text that is synced, in bytes of UTF-8.
pub const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;
/// Largest image, as PNG bytes and as pixels (36 million is a bit over 8K).
pub const MAX_IMAGE_BYTES: usize = 24 * 1024 * 1024;
pub const MAX_IMAGE_PIXELS: u64 = 36_000_000;
/// Request body limit for `/peer/clipboard`: the PNG is base64 inside JSON.
pub const MAX_BODY_BYTES: usize = MAX_IMAGE_BYTES / 3 * 4 + 64 * 1024;
const POLL: Duration = Duration::from_millis(400);
/// Without a cheap change counter, reading a large image on every poll is
/// wasteful, so images are only looked for this often.
const IMAGE_POLL: Duration = Duration::from_secs(2);
/// How long a device that answered "clipboard sync is off" is left alone.
const DECLINED_FOR: Duration = Duration::from_secs(60);

#[derive(Clone, PartialEq, Eq)]
pub enum Content {
    Text(String),
    /// RGBA, 8 bits per channel, rows top to bottom, no padding.
    Image { width: u32, height: u32, rgba: Vec<u8> },
}

impl std::fmt::Debug for Content {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Content::Text(t) => write!(f, "Text({} bytes)", t.len()),
            Content::Image { width, height, .. } => write!(f, "Image({width}x{height})"),
        }
    }
}

impl Content {
    /// Short human description, e.g. for the activity list.
    pub fn describe(&self) -> String {
        match self {
            Content::Text(t) => {
                let n = t.chars().count();
                format!("text, {n} character{}", if n == 1 { "" } else { "s" })
            }
            Content::Image { width, height, .. } => format!("image, {width} by {height} pixels"),
        }
    }

    fn hash(&self) -> blake3::Hash {
        let mut h = blake3::Hasher::new();
        match self {
            Content::Text(t) => {
                h.update(b"text\0");
                h.update(t.as_bytes());
            }
            Content::Image { width, height, rgba } => {
                h.update(b"image\0");
                h.update(&width.to_le_bytes());
                h.update(&height.to_le_bytes());
                h.update(rgba);
            }
        }
        h.finalize()
    }

    /// Checks the limits and that the data matches its declared shape.
    pub fn validate(&self) -> Result<()> {
        match self {
            Content::Text(t) => {
                if t.is_empty() {
                    bail!("empty text");
                }
                if t.len() > MAX_TEXT_BYTES {
                    bail!("text is too long to sync ({} bytes)", t.len());
                }
            }
            Content::Image { width, height, rgba } => {
                let px = *width as u64 * *height as u64;
                if px == 0 {
                    bail!("empty image");
                }
                if px > MAX_IMAGE_PIXELS {
                    bail!("image is too large to sync ({width} by {height})");
                }
                if rgba.len() as u64 != px * 4 {
                    bail!("image data does not match its size");
                }
            }
        }
        Ok(())
    }
}

// ------------------------------------------------------------- wire format

/// What one device sends another. Exactly one of `text` and `image` is set.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Wire {
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<WireImage>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WireImage {
    pub width: u32,
    pub height: u32,
    /// PNG, base64 (standard alphabet, padded).
    pub png: String,
}

pub fn to_wire(c: &Content, seq: u64) -> Result<Wire> {
    c.validate()?;
    match c {
        Content::Text(t) => Ok(Wire { seq, text: Some(t.clone()), image: None }),
        Content::Image { width, height, rgba } => {
            let img = image::RgbaImage::from_raw(*width, *height, rgba.clone()).ok_or_else(|| anyhow!("image data does not match its size"))?;
            let mut png = Vec::new();
            img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).context("encoding image")?;
            if png.len() > MAX_IMAGE_BYTES {
                bail!("image is too large to sync ({} bytes as PNG)", png.len());
            }
            let png = base64::engine::general_purpose::STANDARD.encode(&png);
            Ok(Wire { seq, text: None, image: Some(WireImage { width: *width, height: *height, png }) })
        }
    }
}

pub fn from_wire(w: &Wire) -> Result<Content> {
    let c = match (&w.text, &w.image) {
        (Some(t), None) => {
            // Windows treats NUL as the end of clipboard text; never let one hide the rest.
            Content::Text(if t.contains('\0') { t.replace('\0', "") } else { t.clone() })
        }
        (None, Some(i)) => {
            if i.png.len() > MAX_IMAGE_BYTES / 3 * 4 + 4 {
                bail!("image is too large to sync");
            }
            let px = i.width as u64 * i.height as u64;
            if px == 0 || px > MAX_IMAGE_PIXELS {
                bail!("image is too large to sync ({} by {})", i.width, i.height);
            }
            let png = base64::engine::general_purpose::STANDARD.decode(&i.png).context("image data is not valid base64")?;
            let (dw, dh) = image::ImageReader::with_format(std::io::Cursor::new(&png), image::ImageFormat::Png).into_dimensions().context("reading image size")?;
            if (dw, dh) != (i.width, i.height) {
                bail!("image size does not match its data");
            }
            let img = image::load_from_memory_with_format(&png, image::ImageFormat::Png).context("decoding image")?.into_rgba8();
            Content::Image { width: i.width, height: i.height, rgba: img.into_raw() }
        }
        _ => bail!("message must hold exactly one of text or image"),
    };
    c.validate()?;
    Ok(c)
}

// ---------------------------------------------------------------- tracking

/// Decides which clipboard changes are new. One per device.
#[derive(Default)]
pub struct Tracker {
    /// What the local clipboard held when last looked at.
    seen: Option<blake3::Hash>,
    /// What was last written on behalf of another device; seeing it locally
    /// is not a change to send.
    applied: Option<blake3::Hash>,
    seq: u64,
}

impl Tracker {
    /// The local clipboard holds `c`. Returns a sequence number if the other
    /// devices should get it.
    pub fn observe_local(&mut self, c: &Content) -> Option<u64> {
        let h = c.hash();
        if self.seen == Some(h) {
            return None;
        }
        self.seen = Some(h);
        if self.applied == Some(h) {
            return None;
        }
        self.seq += 1;
        Some(self.seq)
    }

    /// Remembers `c` as current without sending it: what the clipboard held
    /// when sync was switched on is not news.
    pub fn prime(&mut self, c: &Content) {
        self.seen = Some(c.hash());
    }

    /// Another device sent `c`. Returns true if it should be written to the
    /// local clipboard. `seen` is left alone: the next poll updates it once
    /// the write has really happened, so a failed write never resends the
    /// old contents.
    pub fn accept_remote(&mut self, c: &Content) -> bool {
        let h = c.hash();
        if self.seen == Some(h) || self.applied == Some(h) {
            return false;
        }
        self.applied = Some(h);
        true
    }
}

// ------------------------------------------------------------ OS clipboard

pub trait Clipboard: Send {
    /// A number that changes whenever the clipboard changes, where the
    /// platform can tell cheaply. None means "read it and see".
    fn change_count(&mut self) -> Option<u64> {
        None
    }
    /// Text if there is any, otherwise an image if `want_image`, otherwise None.
    fn read(&mut self, want_image: bool) -> Result<Option<Content>>;
    fn write(&mut self, c: &Content) -> Result<()>;
}

pub struct SystemClipboard {
    inner: Option<arboard::Clipboard>,
    retry_at: Instant,
    warned: bool,
}

impl Default for SystemClipboard {
    fn default() -> Self {
        SystemClipboard { inner: None, retry_at: Instant::now(), warned: false }
    }
}

impl SystemClipboard {
    /// Headless machines may have no clipboard at all; keep trying quietly.
    fn open(&mut self) -> Option<&mut arboard::Clipboard> {
        if self.inner.is_none() && Instant::now() >= self.retry_at {
            match arboard::Clipboard::new() {
                Ok(c) => self.inner = Some(c),
                Err(e) => {
                    if !self.warned {
                        tracing::warn!("clipboard not available: {e}");
                        self.warned = true;
                    }
                    self.retry_at = Instant::now() + Duration::from_secs(30);
                }
            }
        }
        self.inner.as_mut()
    }
}

impl Clipboard for SystemClipboard {
    fn change_count(&mut self) -> Option<u64> {
        #[cfg(windows)]
        {
            // Cheap and needs no clipboard lock.
            Some(unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() } as u64)
        }
        #[cfg(target_os = "macos")]
        {
            Some(objc2_app_kit::NSPasteboard::generalPasteboard().changeCount() as u64)
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            None
        }
    }

    fn read(&mut self, want_image: bool) -> Result<Option<Content>> {
        let Some(cb) = self.open() else { return Ok(None) };
        match cb.get_text() {
            Ok(t) if !t.is_empty() => return Ok(Some(Content::Text(t))),
            Ok(_) | Err(arboard::Error::ContentNotAvailable) => {}
            // Another program holds it right now; next poll.
            Err(arboard::Error::ClipboardOccupied) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        if !want_image {
            return Ok(None);
        }
        match cb.get_image() {
            Ok(img) => Ok(Some(Content::Image { width: img.width as u32, height: img.height as u32, rgba: img.bytes.into_owned() })),
            Err(arboard::Error::ContentNotAvailable | arboard::Error::ClipboardOccupied | arboard::Error::ConversionFailure) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn write(&mut self, c: &Content) -> Result<()> {
        let cb = self.open().ok_or_else(|| anyhow!("clipboard not available"))?;
        match c {
            Content::Text(t) => cb.set_text(t.as_str())?,
            Content::Image { width, height, rgba } => {
                cb.set_image(arboard::ImageData { width: *width as usize, height: *height as usize, bytes: std::borrow::Cow::Borrowed(rgba) })?
            }
        }
        Ok(())
    }
}

/// In-memory stand-in for tests.
#[cfg(test)]
#[derive(Clone, Default)]
pub struct MemClipboard(pub Arc<Mutex<(Option<Content>, u64)>>);

#[cfg(test)]
impl MemClipboard {
    pub fn set(&self, c: Option<Content>) {
        let mut g = self.0.lock();
        g.0 = c;
        g.1 += 1;
    }
    pub fn get(&self) -> Option<Content> {
        self.0.lock().0.clone()
    }
}

#[cfg(test)]
impl Clipboard for MemClipboard {
    fn change_count(&mut self) -> Option<u64> {
        Some(self.0.lock().1)
    }
    fn read(&mut self, want_image: bool) -> Result<Option<Content>> {
        Ok(self.get().filter(|c| want_image || matches!(c, Content::Text(_))))
    }
    fn write(&mut self, c: &Content) -> Result<()> {
        self.set(Some(c.clone()));
        Ok(())
    }
}

// ------------------------------------------------------------------ service

enum Cmd {
    Write(Content),
}

/// Owns the clipboard thread. Lives in `App`.
pub struct Service {
    tx: mpsc::Sender<Cmd>,
    enabled: Arc<AtomicBool>,
    tracker: Arc<Mutex<Tracker>>,
    local_rx: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<(u64, Content)>>>,
    /// Devices that said their clipboard sync is off, and until when to skip them.
    declined: Mutex<HashMap<String, Instant>>,
}

impl Service {
    pub fn start(enabled: bool, make: impl FnOnce() -> Box<dyn Clipboard> + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let (ltx, lrx) = tokio::sync::mpsc::unbounded_channel();
        let enabled = Arc::new(AtomicBool::new(enabled));
        let tracker = Arc::new(Mutex::new(Tracker::default()));
        let (en, tr) = (enabled.clone(), tracker.clone());
        std::thread::Builder::new().name("clipboard".into()).spawn(move || poll_loop(make(), rx, ltx, en, tr)).expect("spawn clipboard thread");
        Service { tx, enabled, tracker, local_rx: Mutex::new(Some(lrx)), declined: Mutex::new(HashMap::new()) }
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::SeqCst);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// Content from another device. Returns true if it is going to the clipboard.
    pub fn accept_remote(&self, c: Content) -> bool {
        if !self.enabled() || !self.tracker.lock().accept_remote(&c) {
            return false;
        }
        self.tx.send(Cmd::Write(c)).is_ok()
    }

    /// Local changes worth sending, with their sequence numbers. Once.
    pub fn take_local_changes(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<(u64, Content)>> {
        self.local_rx.lock().take()
    }

    fn declined(&self, node: &str) -> bool {
        self.declined.lock().get(node).is_some_and(|until| *until > Instant::now())
    }

    fn decline(&self, node: &str) {
        self.declined.lock().insert(node.to_string(), Instant::now() + DECLINED_FOR);
    }
}

fn poll_loop(mut clip: Box<dyn Clipboard>, rx: mpsc::Receiver<Cmd>, out: tokio::sync::mpsc::UnboundedSender<(u64, Content)>, enabled: Arc<AtomicBool>, tracker: Arc<Mutex<Tracker>>) {
    let mut last_count: Option<u64> = None;
    let mut was_enabled = false;
    let mut last_image_probe = Instant::now() - IMAGE_POLL;
    let mut next_poll = Instant::now();
    loop {
        match rx.recv_timeout(next_poll.saturating_duration_since(Instant::now())) {
            Ok(Cmd::Write(c)) => {
                if let Err(e) = clip.write(&c) {
                    tracing::warn!("writing clipboard: {e:#}");
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        next_poll = Instant::now() + POLL;
        if !enabled.load(Ordering::SeqCst) {
            was_enabled = false;
            continue;
        }
        let count = clip.change_count();
        if count.is_some() && count == last_count && was_enabled {
            continue;
        }
        let want_image = count.is_some() || last_image_probe.elapsed() >= IMAGE_POLL;
        if want_image {
            last_image_probe = Instant::now();
        }
        match clip.read(want_image) {
            Ok(Some(c)) => {
                last_count = count;
                let mut t = tracker.lock();
                if !was_enabled {
                    t.prime(&c);
                } else if let Some(seq) = t.observe_local(&c) {
                    drop(t);
                    match c.validate() {
                        Ok(()) => {
                            if out.send((seq, c)).is_err() {
                                return;
                            }
                        }
                        Err(e) => tracing::info!("clipboard not synced: {e}"),
                    }
                }
            }
            Ok(None) => last_count = count,
            // Leave last_count so the next poll tries again.
            Err(e) => tracing::debug!("reading clipboard: {e:#}"),
        }
        was_enabled = true;
    }
}

/// Sends local clipboard changes to every connected device. Runs for the
/// life of the app.
pub async fn run(app: crate::state::AppRef) {
    let Some(mut rx) = app.clipboard.take_local_changes() else { return };
    loop {
        let (mut seq, mut content) = tokio::select! {
            Some(x) = rx.recv() => x,
            _ = app.shutdown.cancelled() => return,
            else => return,
        };
        // Several copies in quick succession: only the newest matters.
        while let Ok(x) = rx.try_recv() {
            (seq, content) = x;
        }
        let describe = content.describe();
        let wire = match tokio::task::spawn_blocking(move || to_wire(&content, seq)).await {
            Ok(Ok(w)) => Arc::new(w),
            Ok(Err(e)) => {
                tracing::info!("clipboard not synced: {e:#}");
                continue;
            }
            Err(_) => continue,
        };
        let peers: Vec<String> = app.online.lock().keys().filter(|n| !app.clipboard.declined(n)).cloned().collect();
        if peers.is_empty() {
            continue;
        }
        tracing::debug!("clipboard #{seq} ({describe}) to {} device(s)", peers.len());
        let sends = peers.into_iter().map(|node| {
            let (app, wire) = (app.clone(), wire.clone());
            async move {
                match crate::peer::post_json::<_, serde_json::Value>(&app, &node, "/peer/clipboard", &*wire).await {
                    Ok(_) => {}
                    Err(e) => {
                        let msg = e.to_string();
                        if msg.starts_with("403") {
                            app.clipboard.decline(&node);
                        }
                        tracing::debug!("clipboard to {}: {msg}", app.node_name(&node));
                    }
                }
            }
        });
        futures_util::future::join_all(sends).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Content {
        Content::Text(s.to_string())
    }

    fn image(w: u32, h: u32) -> Content {
        let rgba = (0..w * h).flat_map(|i| [(i % 256) as u8, (i / 256 % 256) as u8, (i * 7 % 256) as u8, 255]).collect();
        Content::Image { width: w, height: h, rgba }
    }

    #[test]
    fn tracker_sends_new_local_content_once() {
        let mut t = Tracker::default();
        assert_eq!(t.observe_local(&text("a")), Some(1));
        assert_eq!(t.observe_local(&text("a")), None, "same content again is not a change");
        assert_eq!(t.observe_local(&text("b")), Some(2));
        assert_eq!(t.observe_local(&text("a")), Some(3), "going back to earlier content is a change");
    }

    #[test]
    fn tracker_does_not_echo_remote_content() {
        let mut t = Tracker::default();
        assert!(t.accept_remote(&text("from peer")));
        // The poll sees what was just written: not news for the peers.
        assert_eq!(t.observe_local(&text("from peer")), None);
        // A copy of the same content from a second peer is ignored too.
        assert!(!t.accept_remote(&text("from peer")));
        // The user copies something else: sent.
        assert_eq!(t.observe_local(&text("mine")), Some(1));
        // The peer sends back what we already have: nothing to write.
        assert!(!t.accept_remote(&text("mine")));
    }

    #[test]
    fn tracker_prime_marks_existing_content_as_old() {
        let mut t = Tracker::default();
        t.prime(&text("already here"));
        assert_eq!(t.observe_local(&text("already here")), None);
        assert_eq!(t.observe_local(&text("new")), Some(1));
    }

    #[test]
    fn text_and_image_hash_differently() {
        let mut t = Tracker::default();
        assert!(t.observe_local(&text("x")).is_some());
        assert!(t.observe_local(&image(1, 1)).is_some());
        assert!(t.observe_local(&Content::Image { width: 1, height: 1, rgba: vec![9, 9, 9, 255] }).is_some(), "different pixels");
        assert!(t.observe_local(&Content::Image { width: 1, height: 1, rgba: vec![9, 9, 9, 255] }).is_none());
    }

    #[test]
    fn text_survives_the_wire_unchanged() {
        let samples = [
            "plain ascii",
            "acentos: canción, niño, ¿qué? ¡así!",
            "emoji 🙂🇪🇸👨‍👩‍👧‍👦 and astral 𝄞",
            "CJK 日本語 한국어 中文",
            "RTL עברית العربية",
            "combining e\u{301} and zero width \u{200b}joiner\u{200d}",
            "windows\r\nline\r\nendings and unix\nones and old mac\r",
            "tabs\tand  double  spaces and trailing space ",
            "\u{feff}starts with a BOM",
            "quotes \" and backslashes \\ and control \u{1} chars",
        ];
        for s in samples {
            let wire = to_wire(&text(s), 1).unwrap();
            let json = serde_json::to_string(&wire).unwrap();
            let back: Wire = serde_json::from_str(&json).unwrap();
            assert_eq!(from_wire(&back).unwrap(), text(s), "{s:?}");
        }
    }

    #[test]
    fn nul_is_removed_on_receive() {
        let wire = Wire { seq: 1, text: Some("before\0after".into()), image: None };
        assert_eq!(from_wire(&wire).unwrap(), text("beforeafter"));
    }

    #[test]
    fn invalid_utf8_is_rejected_by_json() {
        let bad = b"{\"seq\":1,\"text\":\"\xff\xfe\"}";
        assert!(serde_json::from_slice::<Wire>(bad).is_err());
    }

    #[test]
    fn text_limits() {
        assert!(to_wire(&text(""), 1).is_err());
        let big = "é".repeat(MAX_TEXT_BYTES / 2 + 1); // 2 bytes each: just over the limit
        assert!(to_wire(&text(&big), 1).is_err());
        assert!(from_wire(&Wire { seq: 1, text: Some(big), image: None }).is_err());
        let ok = "é".repeat(MAX_TEXT_BYTES / 2);
        assert!(to_wire(&text(&ok), 1).is_ok());
    }

    #[test]
    fn image_survives_the_wire_unchanged() {
        let img = image(37, 21);
        let wire = to_wire(&img, 5).unwrap();
        let wi = wire.image.as_ref().unwrap();
        assert_eq!((wi.width, wi.height), (37, 21));
        let json = serde_json::to_string(&wire).unwrap();
        let back: Wire = serde_json::from_str(&json).unwrap();
        assert_eq!(from_wire(&back).unwrap(), img);
    }

    #[test]
    fn image_with_wrong_size_or_data_is_rejected() {
        let wire = to_wire(&image(4, 4), 1).unwrap();
        let mut lied = wire.clone();
        lied.image.as_mut().unwrap().width = 5;
        assert!(from_wire(&lied).is_err(), "declared size must match the PNG");
        let mut garbage = wire.clone();
        garbage.image.as_mut().unwrap().png = base64::engine::general_purpose::STANDARD.encode(b"not a png");
        assert!(from_wire(&garbage).is_err());
        let mut not_b64 = wire.clone();
        not_b64.image.as_mut().unwrap().png = "***".into();
        assert!(from_wire(&not_b64).is_err());
        let huge = Wire { seq: 1, text: None, image: Some(WireImage { width: 100_000, height: 100_000, png: String::new() }) };
        assert!(from_wire(&huge).is_err());
        assert!(to_wire(&Content::Image { width: 2, height: 2, rgba: vec![0; 3] }, 1).is_err(), "rgba length must match");
        assert!(to_wire(&Content::Image { width: 0, height: 5, rgba: vec![] }, 1).is_err());
    }

    #[test]
    fn wire_needs_exactly_one_kind() {
        assert!(from_wire(&Wire { seq: 1, text: None, image: None }).is_err());
        let both = Wire { seq: 1, text: Some("x".into()), image: to_wire(&image(1, 1), 1).unwrap().image };
        assert!(from_wire(&both).is_err());
        // Unknown fields from a newer version are fine.
        let w: Wire = serde_json::from_str(r#"{"seq":3,"text":"hi","future":true}"#).unwrap();
        assert_eq!(from_wire(&w).unwrap(), text("hi"));
    }

    fn wait_for(mut f: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn service_reports_local_changes_and_ignores_its_own_writes() {
        let mem = MemClipboard::default();
        mem.set(Some(text("there before sync was on")));
        let svc = Service::start(true, {
            let m = mem.clone();
            move || Box::new(m)
        });
        let mut rx = svc.take_local_changes().unwrap();
        assert!(svc.take_local_changes().is_none());

        // What was on the clipboard when sync started is not sent.
        std::thread::sleep(POLL * 3);
        assert!(rx.try_recv().is_err());

        mem.set(Some(text("copied locally")));
        let (seq, c) = rx.blocking_recv().unwrap();
        assert_eq!((seq, c), (1, text("copied locally")));

        // A remote message is written and not reported back.
        assert!(svc.accept_remote(text("from a peer")));
        assert!(wait_for(|| mem.get() == Some(text("from a peer"))));
        std::thread::sleep(POLL * 3);
        assert!(rx.try_recv().is_err(), "remote content must not be echoed");
        assert!(!svc.accept_remote(text("from a peer")), "duplicate is ignored");

        // An image, then the same image again.
        mem.set(Some(image(3, 2)));
        assert_eq!(rx.blocking_recv().unwrap(), (2, image(3, 2)));
        mem.set(Some(image(3, 2)));
        std::thread::sleep(POLL * 3);
        assert!(rx.try_recv().is_err());

        // Switched off: nothing is sent or accepted. Switched on again: the
        // content that appeared meanwhile is not sent, only later changes are.
        svc.set_enabled(false);
        std::thread::sleep(POLL * 2);
        mem.set(Some(text("while off")));
        assert!(!svc.accept_remote(text("ignored while off")));
        std::thread::sleep(POLL * 3);
        assert!(rx.try_recv().is_err());
        svc.set_enabled(true);
        std::thread::sleep(POLL * 3);
        assert!(rx.try_recv().is_err());
        mem.set(Some(text("after on again")));
        assert_eq!(rx.blocking_recv().unwrap(), (3, text("after on again")));
    }

    #[test]
    fn service_skips_content_over_the_limit() {
        let mem = MemClipboard::default();
        let svc = Service::start(true, {
            let m = mem.clone();
            move || Box::new(m)
        });
        let mut rx = svc.take_local_changes().unwrap();
        std::thread::sleep(POLL * 2);
        mem.set(Some(text(&"x".repeat(MAX_TEXT_BYTES + 1))));
        std::thread::sleep(POLL * 3);
        assert!(rx.try_recv().is_err());
        mem.set(Some(text("small")));
        assert_eq!(rx.blocking_recv().unwrap().1, text("small"));
    }
}
