//! Screen reader announcements through Prism.
//!
//! All Prism calls happen on one dedicated thread (backend instances are not
//! thread-safe). Messages are only sent to real screen readers (NVDA, JAWS,
//! VoiceOver, Narrator via UI Automation, ...) unless the user opts into the
//! system voice fallback.

use std::sync::mpsc;

#[derive(Debug)]
#[cfg_attr(not(has_prism), allow(dead_code))]
pub enum Msg {
    Say { text: String, allow_tts: bool },
}

#[derive(Clone)]
pub struct Announcer {
    tx: mpsc::Sender<Msg>,
}

impl Announcer {
    pub fn start() -> Self {
        let (tx, rx) = mpsc::channel::<Msg>();
        std::thread::Builder::new()
            .name("prism".into())
            .spawn(move || run(rx))
            .expect("spawn prism thread");
        Announcer { tx }
    }

    pub fn say(&self, text: impl Into<String>, allow_tts: bool) {
        let _ = self.tx.send(Msg::Say { text: text.into(), allow_tts });
    }
}

#[cfg(has_prism)]
mod ffi {
    use std::ffi::{c_char, c_void};

    #[repr(C)]
    pub struct PrismConfig {
        pub version: u8,
        pub registry: *mut c_void,
        pub availability_callback: Option<extern "C" fn(*mut c_void, u64, *const c_char, bool)>,
        pub availability_userdata: *mut c_void,
        pub availability_poll_interval_ms: u32,
        pub availability_debounce_samples: u32,
        pub availability_backoff_max_ms: u32,
        pub availability_auto_power_manage: bool,
    }

    pub const OK: i32 = 0;

    pub const BACKEND_SAPI: u64 = 0x1D6DF72422CEEE66;
    pub const BACKEND_AV_SPEECH: u64 = 0x28E3429577805C24;
    pub const BACKEND_SPEECH_DISPATCHER: u64 = 0xE3D6F895D949EBFE;
    pub const BACKEND_ONE_CORE: u64 = 0x6797D32F0D994CB4;
    pub const BACKEND_ANDROID_TTS: u64 = 0xBC175831BFE4E5CC;
    pub const BACKEND_WEB_SPEECH: u64 = 0x3572538D44D44A8F;

    unsafe extern "C" {
        pub fn prism_config_init() -> PrismConfig;
        pub fn prism_init(cfg: *mut PrismConfig) -> *mut c_void;
        pub fn prism_registry_count(ctx: *mut c_void) -> usize;
        pub fn prism_registry_id_at(ctx: *mut c_void, index: usize) -> u64;
        pub fn prism_registry_create(ctx: *mut c_void, id: u64) -> *mut c_void;
        pub fn prism_backend_free(backend: *mut c_void);
        pub fn prism_backend_name(backend: *mut c_void) -> *const c_char;
        pub fn prism_backend_initialize(backend: *mut c_void) -> i32;
        pub fn prism_backend_output(backend: *mut c_void, text: *const c_char, interrupt: bool) -> i32;
    }

    pub fn is_tts(id: u64) -> bool {
        matches!(
            id,
            BACKEND_SAPI | BACKEND_AV_SPEECH | BACKEND_SPEECH_DISPATCHER | BACKEND_ONE_CORE | BACKEND_ANDROID_TTS | BACKEND_WEB_SPEECH
        )
    }
}

#[cfg(has_prism)]
struct Backend {
    ptr: *mut std::ffi::c_void,
    tts: bool,
    name: String,
}

#[cfg(has_prism)]
fn run(rx: mpsc::Receiver<Msg>) {
    use ffi::*;
    use std::ffi::{CStr, CString};

    let ctx = unsafe {
        let mut cfg = prism_config_init();
        prism_init(&mut cfg)
    };
    if ctx.is_null() {
        tracing::warn!("prism_init failed; announcements disabled");
        for _ in rx {}
        return;
    }
    let ids: Vec<u64> = unsafe { (0..prism_registry_count(ctx)).map(|i| prism_registry_id_at(ctx, i)).collect() };

    // Picks the highest-priority backend that initializes; screen readers first.
    let pick = |allow_tts: bool| -> Option<Backend> {
        for &id in &ids {
            let tts = is_tts(id);
            if tts && !allow_tts {
                continue;
            }
            unsafe {
                let b = prism_registry_create(ctx, id);
                if b.is_null() {
                    continue;
                }
                if prism_backend_initialize(b) == OK {
                    let n = prism_backend_name(b);
                    let name = if n.is_null() { String::new() } else { CStr::from_ptr(n).to_string_lossy().into_owned() };
                    return Some(Backend { ptr: b, tts, name });
                }
                prism_backend_free(b);
            }
        }
        None
    };

    let mut current: Option<Backend> = None;
    for msg in rx {
        let Msg::Say { text, allow_tts } = msg;
        let Ok(c) = CString::new(text.replace('\0', " ")) else { continue };
        // A screen reader may have been started or closed since the last message,
        // so a TTS fallback or a failing backend is re-picked every time.
        if current.as_ref().is_some_and(|b| b.tts) {
            if let Some(b) = current.take() {
                unsafe { prism_backend_free(b.ptr) };
            }
        }
        for attempt in 0..2 {
            if current.is_none() {
                current = pick(allow_tts);
                if let Some(b) = &current {
                    tracing::debug!("prism backend: {}", b.name);
                }
            }
            let Some(b) = &current else { break };
            if b.tts && !allow_tts {
                break;
            }
            let rc = unsafe { prism_backend_output(b.ptr, c.as_ptr(), false) };
            if rc == OK {
                break;
            }
            tracing::debug!("prism output via {} failed ({rc}), attempt {attempt}", b.name);
            if let Some(b) = current.take() {
                unsafe { prism_backend_free(b.ptr) };
            }
        }
    }
}

#[cfg(not(has_prism))]
fn run(rx: mpsc::Receiver<Msg>) {
    for msg in rx {
        tracing::info!("announce (no prism): {msg:?}");
    }
}
