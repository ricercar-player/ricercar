//! The plugin host: one supervised process per enabled plugin, typed calls,
//! URL resolution for the controller, playback reporting and remote control.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::rpc::{CallError, Incoming, Rpc, RpcError};
use super::settings::{self, Setting, Stored};
use super::{
    AuthBegin, AuthState, AuthStatus, Capabilities, Item, ItemDetails, ItemKind, LibraryList,
    OutputInfo, PROTOCOL, PluginError, PluginInfo, PluginLyrics, Purpose, Resolved, Resolver,
    items_of, parse_plugin_uri, valid_ref,
};
use crate::config::PluginConfig;
use crate::controller::{Controller, EnqueueAt, Origin, TrackInfo};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
const AUTH_COMPLETE_TIMEOUT: Duration = Duration::from_secs(30);
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
/// Lyrics show while the track plays: a slow plugin is passed over.
const LYRICS_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// Restart delays after a crash; the last one repeats.
const BACKOFF: [u64; 4] = [1, 2, 5, 30];
/// A process that ran this long starts the backoff over.
const STABLE: Duration = Duration::from_secs(60);
const PROGRESS_EVERY: Duration = Duration::from_secs(30);
/// Longest rate-limit block honoured, whatever the plugin asks for.
const MAX_RETRY_AFTER: u64 = 3600;
/// Tracks asked for per `radio.next` by continuous playback.
pub const RADIO_LIMIT: usize = 20;
const RADIO_MAX: usize = 50;
/// Refs sent in `radio.next`'s `exclude`, at most.
const RADIO_EXCLUDE: usize = 50;
/// Most tracks read for a "play" action.
pub const TRACKS_MAX: usize = 500;
const MAX_PLAYLIST_NAME: usize = 200;
const MAX_PLAYLIST_DESCRIPTION: usize = 2000;
/// Refs or entries sent in one `playlists.add` / `playlists.remove`.
const MAX_PLAYLIST_BATCH: usize = 500;

/// What a plugin is doing, for the settings page and the diagnostic report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunState {
    /// `enabled = false` in the config.
    Disabled,
    Starting,
    Running,
    /// Crashed or exited; restarting in that many seconds.
    Restarting {
        in_secs: u64,
    },
    /// Will not be restarted (bad protocol, cannot start…).
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PluginStatus {
    pub id: String,
    /// The plugin's own name once it answered, else its id.
    pub name: String,
    pub version: String,
    pub state: RunState,
    pub caps: Capabilities,
    pub auth: Option<AuthStatus>,
    /// Settings the plugin declares (empty until it has answered).
    pub settings: Vec<Setting>,
    /// The value in effect for each of them.
    pub values: serde_json::Map<String, Value>,
}

impl PluginStatus {
    fn new(id: &str, state: RunState) -> PluginStatus {
        PluginStatus {
            id: id.to_string(),
            name: id.to_string(),
            version: String::new(),
            state,
            caps: Capabilities::default(),
            auth: None,
            settings: Vec::new(),
            values: serde_json::Map::new(),
        }
    }

    pub fn signed_in(&self) -> bool {
        self.state == RunState::Running
            && (!self.caps.auth
                || self
                    .auth
                    .as_ref()
                    .is_some_and(|a| a.state == AuthState::SignedIn))
    }
}

struct Slot {
    /// Without its settings, which change live (`values`).
    cfg: PluginConfig,
    values: RwLock<Stored>,
    /// The running process declared its settings with `settings.declared`
    /// (which then wins over the declaration in `initialize`).
    declared: AtomicBool,
    stop: AtomicBool,
    rpc: RwLock<Option<Arc<Rpc>>>,
    status: RwLock<PluginStatus>,
    thread: Mutex<Option<JoinHandle<()>>>,
    blocked_until: Mutex<Option<Instant>>,
    /// Playlist refs the plugin marked `editable`, as last seen.
    editable: Mutex<std::collections::HashSet<String>>,
    /// `playlists.move` answered "method not found" since the start.
    no_move: AtomicBool,
}

struct Inner {
    configs: Mutex<Vec<PluginConfig>>,
    slots: Mutex<HashMap<String, Arc<Slot>>>,
    output: RwLock<OutputInfo>,
    rev: AtomicU64,
    ctl: RwLock<Weak<Controller>>,
    version: String,
    locale: RwLock<String>,
    data_root: PathBuf,
    cache_root: PathBuf,
    alive: AtomicBool,
}

/// Starts, supervises and talks to the declared plugins. Cheap to clone.
#[derive(Clone)]
pub struct PluginHost {
    inner: Arc<Inner>,
}

/// Methods that change something on the service: never retried.
fn changes_something(method: &str) -> bool {
    method.starts_with("playlists.") || method == "favorites.set"
}

/// The system locale (LC_ALL > LC_MESSAGES > LANG), as a BCP 47 tag.
fn system_locale() -> Option<String> {
    let lang = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|v| !v.is_empty() && v != "C" && v != "POSIX")?;
    lang.split(['.', '@'])
        .next()
        .filter(|l| !l.is_empty())
        .map(|l| l.replace('_', "-"))
}

/// The locale plugins get: the interface language chosen in ricercar
/// (`[ui] language`, "en", "fr"…), with the system's region when it is the
/// same language; the system locale when none is chosen; else "en-US".
pub fn plugin_locale(ui_language: &str, system: Option<&str>) -> String {
    let pref = ui_language.trim();
    match system {
        Some(sys) if pref.is_empty() => sys.to_string(),
        None if pref.is_empty() => "en-US".into(),
        Some(sys)
            if sys
                .split('-')
                .next()
                .is_some_and(|l| l.eq_ignore_ascii_case(pref)) =>
        {
            sys.to_string()
        }
        _ => pref.to_string(),
    }
}

/// Variables a plugin inherits; everything else in ricercar's environment
/// is left out.
fn plugin_env() -> Vec<(OsString, OsString)> {
    const KEEP: [&str; 17] = [
        "HOME",
        "USER",
        "LOGNAME",
        "PATH",
        "LANG",
        "TZ",
        "TMPDIR",
        "http_proxy",
        "https_proxy",
        "no_proxy",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
        "LANGUAGE",
    ];
    std::env::vars_os()
        .filter(|(k, _)| {
            k.to_str()
                .is_some_and(|k| KEEP.contains(&k) || k.starts_with("LC_") || k.starts_with("XDG_"))
        })
        .collect()
}

/// SIGKILL the plugin's process group (it runs in its own), so helpers it
/// started go too.
fn kill_group(child: &Child) {
    if let Ok(pid) = libc::pid_t::try_from(child.id())
        && pid > 1
    {
        // SAFETY: plain syscall; a negative pid names the process group.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

fn map_call_error(e: CallError) -> PluginError {
    match e {
        CallError::Timeout => PluginError::Timeout,
        CallError::Closed => PluginError::NotRunning,
        CallError::Busy => PluginError::Timeout,
        CallError::Rpc(RpcError {
            code,
            message,
            data,
        }) => PluginError::from_code(
            code,
            &message,
            data.as_ref()
                .and_then(|d| d.get("retry_after"))
                .and_then(Value::as_u64),
        ),
    }
}

impl PluginHost {
    /// `data_root` / `cache_root`: parents of the per-plugin directories
    /// (`<root>/plugins/<id>/`).
    pub fn new(version: &str, data_root: PathBuf, cache_root: PathBuf) -> PluginHost {
        PluginHost {
            inner: Arc::new(Inner {
                configs: Mutex::new(Vec::new()),
                slots: Mutex::new(HashMap::new()),
                output: RwLock::new(OutputInfo::default()),
                rev: AtomicU64::new(0),
                ctl: RwLock::new(Weak::new()),
                version: version.into(),
                locale: RwLock::new(plugin_locale("", system_locale().as_deref())),
                data_root,
                cache_root,
                alive: AtomicBool::new(true),
            }),
        }
    }

    /// Hook the host to the player: remote control, reporting, and metadata
    /// refresh of restored queues. Call once.
    pub fn attach(&self, ctl: &Arc<Controller>) {
        *self.inner.ctl.write().unwrap() = Arc::downgrade(ctl);
        let events = ctl.subscribe();
        let host = self.clone();
        std::thread::Builder::new()
            .name("ricercar-plugin-events".into())
            .spawn(move || host.event_loop(events))
            .expect("spawn plugin events");
    }

    /// Bumped whenever a status changes (cheap to poll).
    pub fn revision(&self) -> u64 {
        self.inner.rev.load(Ordering::SeqCst)
    }

    fn touch(&self) {
        self.inner.rev.fetch_add(1, Ordering::SeqCst);
    }

    /// Every declared plugin, in config order.
    pub fn statuses(&self) -> Vec<PluginStatus> {
        let configs = self.inner.configs.lock().unwrap().clone();
        let slots = self.inner.slots.lock().unwrap();
        configs
            .iter()
            .map(|c| match slots.get(&c.id) {
                Some(s) => {
                    let mut st = s.status.read().unwrap().clone();
                    st.values = settings::effective(&st.settings, &c.settings);
                    st
                }
                None => PluginStatus::new(
                    &c.id,
                    if c.enabled {
                        RunState::Failed("invalid id".into())
                    } else {
                        RunState::Disabled
                    },
                ),
            })
            .collect()
    }

    pub fn status(&self, id: &str) -> Option<PluginStatus> {
        self.statuses().into_iter().find(|s| s.id == id)
    }

    /// Start, stop or restart processes to match `configs` (applied live).
    /// A change of `settings` alone keeps the process and sends it
    /// `settings.changed`, or restarts it when a changed setting says
    /// `restart`.
    pub fn reconcile(&self, configs: &[PluginConfig]) {
        let mut seen = std::collections::HashSet::new();
        let configs: Vec<PluginConfig> = configs
            .iter()
            .filter(|c| {
                let ok = PluginConfig::valid_id(&c.id) && seen.insert(c.id.clone());
                if !ok {
                    tracing::warn!("plugin \"{}\": invalid or duplicate id, ignored", c.id);
                }
                ok
            })
            .cloned()
            .collect();
        *self.inner.configs.lock().unwrap() = configs.clone();
        let mut to_stop = Vec::new();
        let mut to_notify = Vec::new();
        {
            let mut slots = self.inner.slots.lock().unwrap();
            let ids: Vec<String> = slots.keys().cloned().collect();
            for id in ids {
                let slot = slots[&id].clone();
                let cfg = configs
                    .iter()
                    .find(|c| c.enabled && c.id == id && slot.cfg.same_process(c));
                let mut keep = cfg.is_some();
                if let Some(c) = cfg {
                    let changed = settings::changed_keys(&slot.values.read().unwrap(), &c.settings);
                    if !changed.is_empty() {
                        let restart = slot
                            .status
                            .read()
                            .unwrap()
                            .settings
                            .iter()
                            .any(|s| s.restart && changed.contains(&s.key));
                        if restart {
                            tracing::info!("plugin[{id}]: settings changed, restarting");
                            keep = false;
                        } else {
                            *slot.values.write().unwrap() = c.settings.clone();
                            to_notify.push(slot);
                        }
                    }
                }
                if !keep && let Some(s) = slots.remove(&id) {
                    to_stop.push(s);
                }
            }
            for c in configs.iter().filter(|c| c.enabled) {
                if !slots.contains_key(&c.id) {
                    let mut cfg = c.clone();
                    let values = std::mem::take(&mut cfg.settings);
                    let slot = Arc::new(Slot {
                        cfg,
                        values: RwLock::new(values),
                        declared: AtomicBool::new(false),
                        stop: AtomicBool::new(false),
                        rpc: RwLock::new(None),
                        status: RwLock::new(PluginStatus::new(&c.id, RunState::Starting)),
                        thread: Mutex::new(None),
                        blocked_until: Mutex::new(None),
                        editable: Mutex::new(Default::default()),
                        no_move: AtomicBool::new(false),
                    });
                    let host = self.clone();
                    let s2 = slot.clone();
                    let t = std::thread::Builder::new()
                        .name(format!("ricercar-plugin-{}", c.id))
                        .spawn(move || host.supervise(s2))
                        .expect("spawn plugin supervisor");
                    *slot.thread.lock().unwrap() = Some(t);
                    slots.insert(c.id.clone(), slot);
                }
            }
        }
        for s in to_notify {
            Self::notify_settings(&s);
        }
        for s in to_stop {
            Self::stop_slot(&s);
        }
        self.touch();
    }

    /// `settings.changed` with every declared setting's value in effect.
    fn notify_settings(slot: &Slot) {
        let Some(rpc) = slot.rpc.read().unwrap().clone() else {
            // Not running: `initialize` carries them.
            return;
        };
        let values = settings::effective(
            &slot.status.read().unwrap().settings,
            &slot.values.read().unwrap(),
        );
        rpc.notify("settings.changed", json!({ "settings": values }));
    }

    fn stop_slot(slot: &Slot) {
        slot.stop.store(true, Ordering::SeqCst);
        if let Some(t) = slot.thread.lock().unwrap().take() {
            let _ = t.join();
        }
    }

    /// Stop every plugin (`shutdown`, 2 s, then kill).
    pub fn shutdown(&self) {
        self.inner.alive.store(false, Ordering::SeqCst);
        let slots: Vec<Arc<Slot>> = self
            .inner
            .slots
            .lock()
            .unwrap()
            .drain()
            .map(|(_, s)| s)
            .collect();
        for s in &slots {
            s.stop.store(true, Ordering::SeqCst);
        }
        for s in slots {
            Self::stop_slot(&s);
        }
    }

    /// The output changed (device switch, new capabilities).
    /// Follow the interface language (`[ui] language`, empty = the
    /// system's): the next `initialize` carries it, and running plugins get
    /// `locale.changed`.
    pub fn set_language(&self, ui_language: &str) {
        let locale = plugin_locale(ui_language, system_locale().as_deref());
        if *self.inner.locale.read().unwrap() == locale {
            return;
        }
        *self.inner.locale.write().unwrap() = locale.clone();
        let rpcs: Vec<Arc<Rpc>> = self
            .inner
            .slots
            .lock()
            .unwrap()
            .values()
            .filter_map(|s| s.rpc.read().unwrap().clone())
            .collect();
        for rpc in rpcs {
            rpc.notify("locale.changed", json!({ "locale": locale }));
        }
    }

    pub fn set_output(&self, output: OutputInfo) {
        if *self.inner.output.read().unwrap() == output {
            return;
        }
        *self.inner.output.write().unwrap() = output.clone();
        let params = serde_json::to_value(&output).unwrap_or(Value::Null);
        let rpcs: Vec<Arc<Rpc>> = self
            .inner
            .slots
            .lock()
            .unwrap()
            .values()
            .filter_map(|s| s.rpc.read().unwrap().clone())
            .collect();
        for rpc in rpcs {
            rpc.notify("output.changed", json!({ "output": params }));
        }
    }

    // ------------------------------------------------------------ supervision

    fn set_state(&self, slot: &Slot, state: RunState) {
        slot.status.write().unwrap().state = state;
        self.touch();
    }

    fn supervise(&self, slot: Arc<Slot>) {
        let mut attempt = 0usize;
        while !slot.stop.load(Ordering::SeqCst) {
            self.set_state(&slot, RunState::Starting);
            let started = Instant::now();
            match self.run_once(&slot) {
                Ok(()) => {}
                Err(Fatal(msg)) => {
                    tracing::warn!("plugin[{}]: {msg}", slot.cfg.id);
                    self.set_state(&slot, RunState::Failed(msg));
                    return;
                }
            }
            *slot.rpc.write().unwrap() = None;
            if slot.stop.load(Ordering::SeqCst) {
                break;
            }
            if started.elapsed() >= STABLE {
                attempt = 0;
            }
            let wait = BACKOFF[attempt.min(BACKOFF.len() - 1)];
            attempt += 1;
            tracing::warn!("plugin[{}] stopped; restarting in {wait} s", slot.cfg.id);
            self.set_state(&slot, RunState::Restarting { in_secs: wait });
            let until = Instant::now() + Duration::from_secs(wait);
            while Instant::now() < until && !slot.stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        self.set_state(&slot, RunState::Disabled);
    }

    /// One process lifetime: spawn, handshake, serve until it exits or we
    /// stop it. `Err` means do not restart.
    fn run_once(&self, slot: &Arc<Slot>) -> Result<(), Fatal> {
        let id = slot.cfg.id.clone();
        let data_dir = self.inner.data_root.join("plugins").join(&id);
        let cache_dir = self.inner.cache_root.join("plugins").join(&id);
        let _ = std::fs::create_dir_all(&data_dir);
        let _ = std::fs::create_dir_all(&cache_dir);
        let mut child = match Command::new(&slot.cfg.command)
            .args(&slot.cfg.args)
            .env_clear()
            .envs(plugin_env())
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    "plugin[{id}]: cannot start {}: {e}",
                    slot.cfg.command.display()
                );
                // Missing executable: keep retrying slowly, it may appear.
                return Ok(());
            }
        };
        let (stdin, stdout, stderr) = (
            child.stdin.take().expect("piped"),
            child.stdout.take().expect("piped"),
            child.stderr.take().expect("piped"),
        );
        {
            let id = id.clone();
            let _ = std::thread::Builder::new()
                .name(format!("ricercar-plugin-{id}-log"))
                .spawn(move || {
                    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                        // Plugins may log the URLs they resolve.
                        let line = ricercar_audio::redact_urls(&line);
                        tracing::info!("plugin[{id}] {line}");
                    }
                });
        }
        slot.declared.store(false, Ordering::SeqCst);
        slot.no_move.store(false, Ordering::SeqCst);
        slot.editable.lock().unwrap().clear();
        let (tx, rx) = mpsc::channel::<Incoming>();
        let rpc = Rpc::start(&id, stdout, stdin, move |m| {
            let _ = tx.send(m);
        });
        // Messages the plugin starts (notifications, remote control).
        {
            let host = self.clone();
            let slot = slot.clone();
            let rpc = rpc.clone();
            let _ = std::thread::Builder::new()
                .name(format!("ricercar-plugin-{id}-in"))
                .spawn(move || {
                    for m in rx {
                        host.handle_incoming(&slot, &rpc, m);
                    }
                });
        }

        let output = self.inner.output.read().unwrap().clone();
        let sent = slot.values.read().unwrap().clone();
        let init = rpc.call(
            "initialize",
            json!({
                "protocol": PROTOCOL,
                "host": {"name": "ricercar", "version": self.inner.version},
                "data_dir": data_dir,
                "cache_dir": cache_dir,
                "locale": self.inner.locale.read().unwrap().clone(),
                "output": output,
                "settings": settings::stored_json(&sent),
            }),
            DEFAULT_TIMEOUT,
        );
        let init = match init {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("plugin[{id}]: initialize failed: {e:?}");
                Self::kill(&mut child, &rpc, false);
                return Ok(());
            }
        };
        let protocol = init.get("protocol").and_then(Value::as_u64).unwrap_or(0);
        if protocol != PROTOCOL as u64 {
            Self::kill(&mut child, &rpc, false);
            return Err(Fatal(format!(
                "speaks protocol {protocol}, ricercar speaks {PROTOCOL}: disabled"
            )));
        }
        let info: PluginInfo = init
            .get("plugin")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        let caps: Capabilities = init
            .get("capabilities")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        {
            let mut st = slot.status.write().unwrap();
            st.name = if info.name.trim().is_empty() {
                id.clone()
            } else {
                info.name.chars().take(80).collect()
            };
            st.version = info.version.chars().take(40).collect();
            st.caps = caps.clone();
            st.auth = None;
            // A `settings.declared` already handled is newer.
            if !slot.declared.load(Ordering::SeqCst) {
                st.settings = settings::parse_schema(&id, init.get("settings"));
            }
        }
        *slot.rpc.write().unwrap() = Some(rpc.clone());
        // Changed while it was starting.
        if *slot.values.read().unwrap() != sent {
            Self::notify_settings(slot);
        }
        tracing::info!("plugin[{id}] ready ({} {})", info.name, info.version);
        if caps.auth {
            self.refresh_auth(slot);
        }
        self.set_state(slot, RunState::Running);
        self.refresh_queue_items(&id);

        loop {
            if slot.stop.load(Ordering::SeqCst) || !self.inner.alive.load(Ordering::SeqCst) {
                Self::kill(&mut child, &rpc, true);
                return Ok(());
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    tracing::warn!("plugin[{id}] exited: {status}");
                    kill_group(&child);
                    return Ok(());
                }
                Ok(None) => {}
                Err(_) => return Ok(()),
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// `shutdown` (when `polite`), up to 2 s, then kill the whole group.
    /// Never waits longer: the request is only queued, not written here.
    fn kill(child: &mut Child, rpc: &Rpc, polite: bool) {
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        if polite {
            let _ = rpc.call("shutdown", json!({}), SHUTDOWN_GRACE);
        }
        while Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                kill_group(child);
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        kill_group(child);
        let _ = child.kill();
        let _ = child.wait();
    }

    // ------------------------------------------------------------ calls

    fn slot(&self, id: &str) -> Option<Arc<Slot>> {
        self.inner.slots.lock().unwrap().get(id).cloned()
    }

    /// Raw call with the protocol's error mapping: a network error is
    /// retried once, a rate limit blocks further calls for a while, and
    /// `auth_required` marks the plugin signed out.
    pub fn call(&self, id: &str, method: &str, params: Value) -> Result<Value, PluginError> {
        let timeout = match method {
            "auth.complete" => AUTH_COMPLETE_TIMEOUT,
            "track.resolve" => RESOLVE_TIMEOUT,
            "lyrics.get" => LYRICS_TIMEOUT,
            _ => DEFAULT_TIMEOUT,
        };
        let slot = self.slot(id).ok_or(PluginError::NotRunning)?;
        if let Some(until) = *slot.blocked_until.lock().unwrap() {
            let now = Instant::now();
            if now < until {
                return Err(PluginError::RateLimited {
                    retry_after: (until - now).as_secs().max(1),
                });
            }
        }
        let rpc = slot
            .rpc
            .read()
            .unwrap()
            .clone()
            .ok_or(PluginError::NotRunning)?;
        let mut r = rpc
            .call(method, params.clone(), timeout)
            .map_err(map_call_error);
        // A second try only for reads: a failed write may still have
        // happened on the service, and sending it again could duplicate it.
        if r == Err(PluginError::Network) && !changes_something(method) {
            r = rpc.call(method, params, timeout).map_err(map_call_error);
        }
        match &r {
            Err(PluginError::RateLimited { retry_after }) => {
                let wait = Duration::from_secs((*retry_after).min(MAX_RETRY_AFTER));
                *slot.blocked_until.lock().unwrap() = Instant::now().checked_add(wait);
            }
            Err(PluginError::AuthRequired) => {
                slot.status.write().unwrap().auth = Some(AuthStatus {
                    state: AuthState::SignedOut,
                    account: None,
                });
                self.touch();
            }
            _ => {}
        }
        r
    }

    fn call_as<T: DeserializeOwned>(
        &self,
        id: &str,
        method: &str,
        params: Value,
    ) -> Result<T, PluginError> {
        let v = self.call(id, method, params)?;
        serde_json::from_value(v).map_err(|e| PluginError::Other {
            code: -32603,
            message: format!("bad answer to {method}: {e}"),
        })
    }

    fn refresh_auth(&self, slot: &Slot) {
        let st: Option<AuthStatus> = self.call_as(&slot.cfg.id, "auth.status", json!({})).ok();
        slot.status.write().unwrap().auth = st;
        self.touch();
    }

    pub fn auth_status(&self, id: &str) -> Result<AuthStatus, PluginError> {
        let st: AuthStatus = self.call_as(id, "auth.status", json!({}))?;
        self.store_auth(id, st.clone());
        Ok(st)
    }

    fn store_auth(&self, id: &str, st: AuthStatus) {
        if let Some(s) = self.slot(id) {
            s.status.write().unwrap().auth = Some(st);
            self.touch();
        }
    }

    pub fn auth_begin(&self, id: &str) -> Result<AuthBegin, PluginError> {
        self.call_as(id, "auth.begin", json!({}))
    }

    pub fn auth_complete(&self, id: &str, input: &str) -> Result<AuthStatus, PluginError> {
        let st: AuthStatus = self.call_as(id, "auth.complete", json!({ "input": input }))?;
        self.store_auth(id, st.clone());
        Ok(st)
    }

    pub fn auth_sign_out(&self, id: &str) -> Result<(), PluginError> {
        self.call(id, "auth.sign_out", json!({}))?;
        self.store_auth(
            id,
            AuthStatus {
                state: AuthState::SignedOut,
                account: None,
            },
        );
        Ok(())
    }

    pub fn browse_root(&self, id: &str) -> Result<Vec<Item>, PluginError> {
        Ok(self.browse_root_full(id)?.0)
    }

    /// `browse.root` with its optional `home` entries: (sections, home).
    pub fn browse_root_full(
        &self,
        id: &str,
    ) -> Result<(Vec<Item>, Option<Vec<Item>>), PluginError> {
        let v = self.call(id, "browse.root", json!({}))?;
        let home = v.get("home").map(|h| items_of(Some(h)));
        let sections = items_of(v.get("sections"));
        self.learn(id, &sections);
        Ok((sections, home))
    }

    /// Children of a browsable item: (items, total, has_more).
    pub fn browse_list(
        &self,
        id: &str,
        reference: &str,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<Item>, Option<u64>, bool), PluginError> {
        let v = self.call(
            id,
            "browse.list",
            json!({"ref": reference, "offset": offset, "limit": limit.min(200)}),
        )?;
        let items = items_of(v.get("items"));
        self.learn(id, &items);
        Ok((
            items,
            v.get("total").and_then(Value::as_u64),
            v.get("has_more").and_then(Value::as_bool).unwrap_or(false),
        ))
    }

    /// Search results grouped by kind: (kind, items, has_more).
    pub fn search(
        &self,
        id: &str,
        query: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<(super::ItemKind, Vec<Item>, bool)>, PluginError> {
        let v = self.call(
            id,
            "search",
            json!({"query": query, "offset": offset, "limit": limit.min(200)}),
        )?;
        let groups = v
            .get("groups")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let groups: Vec<_> = groups
            .into_iter()
            .filter_map(|g| {
                let kind = serde_json::from_value(g.get("kind")?.clone()).ok()?;
                Some((
                    kind,
                    items_of(g.get("items")),
                    g.get("has_more").and_then(Value::as_bool).unwrap_or(false),
                ))
            })
            .collect();
        for g in &groups {
            self.learn(id, &g.1);
        }
        Ok(groups)
    }

    /// A whole `library.*` list, page by page, up to `max` items.
    pub fn library_all(
        &self,
        id: &str,
        list: super::LibraryList,
        max: usize,
    ) -> Result<Vec<Item>, PluginError> {
        let mut out = Vec::new();
        loop {
            let v = self.call(
                id,
                list.method(),
                json!({"offset": out.len(), "limit": 200}),
            )?;
            let items = items_of(v.get("items"));
            if list == LibraryList::Playlists {
                self.learn(id, &items);
            }
            let more = v.get("has_more").and_then(Value::as_bool).unwrap_or(false);
            let got = items.len();
            out.extend(items);
            if !more || got == 0 || out.len() >= max {
                out.truncate(max);
                return Ok(out);
            }
        }
    }

    pub fn item_get(&self, id: &str, reference: &str) -> Result<Item, PluginError> {
        let v = self.call(id, "item.get", json!({ "ref": reference }))?;
        let items = items_of(Some(&json!([v])));
        self.learn(id, &items);
        items.into_iter().next().ok_or(PluginError::NotFound)
    }

    pub fn favorites_set(&self, id: &str, reference: &str, on: bool) -> Result<(), PluginError> {
        self.require(id, |c| c.favorites, "favorites")?;
        self.call(id, "favorites.set", json!({"ref": reference, "on": on}))
            .map(|_| ())
    }

    /// Refuse a call the plugin did not declare, without asking it.
    fn require(
        &self,
        id: &str,
        cap: impl Fn(&Capabilities) -> bool,
        what: &str,
    ) -> Result<(), PluginError> {
        match self.status(id) {
            Some(s) if cap(&s.caps) => Ok(()),
            Some(s) if s.state != RunState::Running => Err(PluginError::NotRunning),
            None => Err(PluginError::NotRunning),
            Some(_) => Err(PluginError::Other {
                code: -32601,
                message: format!("{what} not supported"),
            }),
        }
    }

    /// Remember which playlists the plugin says the user may edit.
    fn learn(&self, id: &str, items: &[Item]) {
        let Some(slot) = self.slot(id) else { return };
        let mut set = slot.editable.lock().unwrap();
        for i in items.iter().filter(|i| i.kind == ItemKind::Playlist) {
            if i.editable {
                set.insert(i.reference.clone());
            } else {
                set.remove(&i.reference);
            }
        }
    }

    // ------------------------------------------------------------ lyrics, details

    /// `lyrics.get` (capability `lyrics`). An answer with nothing usable
    /// is `NotFound`, like the plugin's own `not_found`.
    pub fn lyrics_get(&self, id: &str, reference: &str) -> Result<PluginLyrics, PluginError> {
        self.require(id, |c| c.lyrics, "lyrics")?;
        let v = self.call(id, "lyrics.get", json!({ "ref": reference }))?;
        PluginLyrics::parse(&v).ok_or(PluginError::NotFound)
    }

    /// `item.details` (capability `details`): biography, related shelves
    /// and facts, cut to the host's limits.
    pub fn item_details(&self, id: &str, reference: &str) -> Result<ItemDetails, PluginError> {
        self.require(id, |c| c.details, "details")?;
        let v = self.call(id, "item.details", json!({ "ref": reference }))?;
        let d = ItemDetails::parse(&v);
        for s in &d.related {
            self.learn(id, &s.items);
        }
        Ok(d)
    }

    // ------------------------------------------------------------ actions, radio

    /// The playable tracks under `reference` (`browse.list`, page by page,
    /// up to `max` and at most [`TRACKS_MAX`]) as queue entries: what a
    /// "play" action puts in the queue.
    pub fn playable_tracks(
        &self,
        id: &str,
        reference: &str,
        max: usize,
    ) -> Result<Vec<TrackInfo>, PluginError> {
        let max = max.min(TRACKS_MAX);
        let mut out = Vec::new();
        let mut offset = 0;
        // Bounded even for a plugin that always says `has_more`.
        for _ in 0..(TRACKS_MAX / 50 + 5) {
            let (items, _, more) = self.browse_list(id, reference, offset, 200)?;
            offset += items.len();
            let got = items.len();
            out.extend(
                items
                    .iter()
                    .filter(|i| i.is_playable())
                    .map(|i| i.to_track_info(id)),
            );
            if !more || got == 0 || out.len() >= max {
                break;
            }
        }
        out.truncate(max);
        Ok(out)
    }

    /// `radio.next` (capability `radio`): playable tracks following
    /// `seed` (a track, album or artist ref), none of `exclude` (refs).
    pub fn radio_next_items(
        &self,
        id: &str,
        seed: &str,
        exclude: &[String],
        limit: usize,
    ) -> Result<Vec<Item>, PluginError> {
        self.require(id, |c| c.radio, "radio")?;
        let limit = limit.clamp(1, RADIO_MAX);
        let exclude: Vec<&String> = exclude
            .iter()
            .filter(|r| valid_ref(r))
            .take(RADIO_EXCLUDE)
            .collect();
        let v = self.call(
            id,
            "radio.next",
            json!({"seed": seed, "exclude": exclude, "limit": limit}),
        )?;
        let mut items: Vec<Item> = items_of(v.get("items"))
            .into_iter()
            .filter(Item::is_playable)
            .filter(|i| !exclude.contains(&&i.reference))
            .collect();
        items.truncate(limit);
        Ok(items)
    }

    // ------------------------------------------------------------ playlists

    /// Whether `playlists.move` can be offered: the plugin declares
    /// `playlist_edit` and has not answered "method not found" to it.
    pub fn playlist_move_supported(&self, id: &str) -> bool {
        self.require(id, |c| c.playlist_edit, "playlist editing")
            .is_ok()
            && self
                .slot(id)
                .is_some_and(|s| !s.no_move.load(Ordering::SeqCst))
    }

    /// Whether the plugin marked this playlist `editable` (as last seen in
    /// a list, or asked with `item.get`).
    pub fn playlist_editable(&self, id: &str, playlist: &str) -> bool {
        if self
            .slot(id)
            .is_some_and(|s| s.editable.lock().unwrap().contains(playlist))
        {
            return true;
        }
        self.item_get(id, playlist)
            .is_ok_and(|i| i.kind == ItemKind::Playlist && i.editable)
    }

    fn check_editable(&self, id: &str, playlist: &str) -> Result<(), PluginError> {
        self.require(id, |c| c.playlist_edit, "playlist editing")?;
        if !valid_ref(playlist) || !self.playlist_editable(id, playlist) {
            return Err(bad_params("this playlist cannot be edited"));
        }
        Ok(())
    }

    /// After a successful edit: the playlists read again.
    fn edited<T>(&self, id: &str, value: T) -> PlaylistEdited<T> {
        let playlists = self
            .library_all(id, LibraryList::Playlists, super::LIBRARY_MAX)
            .map_err(|e| tracing::info!("plugin[{id}] library.playlists after an edit: {e}"))
            .ok();
        PlaylistEdited { value, playlists }
    }

    /// `playlists.create`: the new playlist.
    pub fn playlist_create(
        &self,
        id: &str,
        name: &str,
        description: Option<&str>,
        public: Option<bool>,
    ) -> Result<PlaylistEdited<Item>, PluginError> {
        self.require(id, |c| c.playlist_edit, "playlist editing")?;
        let name = playlist_name(name)?;
        let mut p = json!({ "name": name });
        if let Some(d) = description.map(str::trim).filter(|d| !d.is_empty()) {
            p["description"] = json!(d.chars().take(MAX_PLAYLIST_DESCRIPTION).collect::<String>());
        }
        if let Some(public) = public {
            p["public"] = json!(public);
        }
        let v = self.call(id, "playlists.create", p)?;
        let item = items_of(Some(&json!([v])))
            .into_iter()
            .next()
            .ok_or_else(|| PluginError::Other {
                code: -32603,
                message: "bad answer to playlists.create".into(),
            })?;
        self.learn(id, std::slice::from_ref(&item));
        Ok(self.edited(id, item))
    }

    /// `playlists.rename`.
    pub fn playlist_rename(
        &self,
        id: &str,
        playlist: &str,
        name: &str,
    ) -> Result<PlaylistEdited<()>, PluginError> {
        let name = playlist_name(name)?;
        self.check_editable(id, playlist)?;
        self.call(
            id,
            "playlists.rename",
            json!({"ref": playlist, "name": name}),
        )?;
        Ok(self.edited(id, ()))
    }

    /// `playlists.delete`. Irreversible on the service: confirm first.
    pub fn playlist_delete(
        &self,
        id: &str,
        playlist: &str,
    ) -> Result<PlaylistEdited<()>, PluginError> {
        self.check_editable(id, playlist)?;
        self.call(id, "playlists.delete", json!({ "ref": playlist }))?;
        if let Some(s) = self.slot(id) {
            s.editable.lock().unwrap().remove(playlist);
        }
        Ok(self.edited(id, ()))
    }

    /// `playlists.add`: `tracks` are `plugin://` URIs, all of plugin `id`
    /// (a track of another plugin, or a local one, is refused).
    pub fn playlist_add(
        &self,
        id: &str,
        playlist: &str,
        tracks: &[String],
    ) -> Result<PlaylistEdited<()>, PluginError> {
        let mut refs = Vec::with_capacity(tracks.len());
        for uri in tracks {
            match parse_plugin_uri(uri) {
                Some((pid, r)) if pid == id && valid_ref(&r) => refs.push(r),
                _ => return Err(bad_params("tracks must come from the same plugin")),
            }
        }
        if refs.is_empty() || refs.len() > MAX_PLAYLIST_BATCH {
            return Err(bad_params("no tracks, or too many at once"));
        }
        self.check_editable(id, playlist)?;
        self.call(id, "playlists.add", json!({"ref": playlist, "items": refs}))?;
        Ok(self.edited(id, ()))
    }

    /// `playlists.remove`: `entries` are the `entry_id`s of the tracks
    /// listed by the playlist's `browse.list`.
    pub fn playlist_remove(
        &self,
        id: &str,
        playlist: &str,
        entries: &[String],
    ) -> Result<PlaylistEdited<()>, PluginError> {
        if entries.is_empty()
            || entries.len() > MAX_PLAYLIST_BATCH
            || !entries.iter().all(|e| valid_ref(e))
        {
            return Err(bad_params("no entries, or too many at once"));
        }
        self.check_editable(id, playlist)?;
        self.call(
            id,
            "playlists.remove",
            json!({"ref": playlist, "entries": entries}),
        )?;
        Ok(self.edited(id, ()))
    }

    /// `playlists.move` (optional): the entry goes to position `to`
    /// (0-based, in the playlist as it is before the move). A plugin
    /// without it answers "method not found", remembered until it
    /// restarts: see [`PluginHost::playlist_move_supported`].
    pub fn playlist_move(
        &self,
        id: &str,
        playlist: &str,
        entry: &str,
        to: usize,
    ) -> Result<PlaylistEdited<()>, PluginError> {
        if !valid_ref(entry) {
            return Err(bad_params("bad entry"));
        }
        self.check_editable(id, playlist)?;
        if !self.playlist_move_supported(id) {
            return Err(unsupported("playlists.move"));
        }
        let r = self.call(
            id,
            "playlists.move",
            json!({"ref": playlist, "entry": entry, "to": to}),
        );
        if let Err(PluginError::Other { code: -32601, .. }) = r {
            if let Some(s) = self.slot(id) {
                s.no_move.store(true, Ordering::SeqCst);
            }
            self.touch();
            return Err(unsupported("playlists.move"));
        }
        r?;
        Ok(self.edited(id, ()))
    }

    /// Update queue entries of a plugin from `item.get` once it is back
    /// (restored sessions carry saved metadata only).
    fn refresh_queue_items(&self, id: &str) {
        let Some(ctl) = self.inner.ctl.read().unwrap().upgrade() else {
            return;
        };
        let items: Vec<(u64, String)> = ctl
            .lock()
            .queue
            .iter()
            .filter_map(|q| {
                let (pid, r) = parse_plugin_uri(&q.info.uri)?;
                (pid == id).then_some((q.id, r))
            })
            .take(200)
            .collect();
        if items.is_empty() {
            return;
        }
        let host = self.clone();
        let id = id.to_string();
        std::thread::spawn(move || {
            for (qid, r) in items {
                match host.item_get(&id, &r) {
                    Ok(item) => ctl.update_item_info(qid, item.to_track_info(&id)),
                    Err(PluginError::AuthRequired | PluginError::NotRunning) => break,
                    Err(_) => {}
                }
            }
        });
    }

    // ------------------------------------------------------------ incoming

    fn handle_incoming(&self, slot: &Arc<Slot>, rpc: &Rpc, m: Incoming) {
        match m {
            Incoming::Notification { method, params } => {
                if method == "settings.declared" {
                    let schema = settings::parse_schema(&slot.cfg.id, params.get("settings"));
                    let mut st = slot.status.write().unwrap();
                    st.settings = schema;
                    slot.declared.store(true, Ordering::SeqCst);
                    drop(st);
                    self.touch();
                } else if method == "auth.changed" {
                    match serde_json::from_value::<AuthStatus>(params.clone()) {
                        Ok(st) if params.get("state").is_some() => {
                            self.store_auth(&slot.cfg.id, st)
                        }
                        _ => self.refresh_auth(slot),
                    }
                }
            }
            Incoming::Request { id, method, params } => {
                let remote = slot.status.read().unwrap().caps.remote_control;
                let r = if remote && method.starts_with("player.") {
                    self.remote(&slot.cfg.id, &method, &params)
                } else {
                    Err(RpcError {
                        code: -32601,
                        message: format!("method not found: {method}"),
                        data: None,
                    })
                };
                rpc.respond(id, r);
            }
        }
    }

    /// Plugin → host `player.*` requests, all through the Controller.
    fn remote(&self, id: &str, method: &str, p: &Value) -> Result<Value, RpcError> {
        let bad = |m: &str| RpcError {
            code: -32602,
            message: m.into(),
            data: None,
        };
        let ctl = self
            .inner
            .ctl
            .read()
            .unwrap()
            .upgrade()
            .ok_or_else(|| bad("player unavailable"))?;
        let infos = |p: &Value| -> Vec<TrackInfo> {
            items_of(p.get("items"))
                .into_iter()
                .filter(Item::is_playable)
                .map(|i| i.to_track_info(id))
                .collect()
        };
        match method {
            "player.play" => {
                let items = infos(p);
                if items.is_empty() {
                    return Err(bad("no playable items"));
                }
                let start = p.get("start").and_then(Value::as_u64).unwrap_or(0) as usize;
                ctl.play_from_plugin(id, items, start);
            }
            "player.enqueue" => {
                let at = match p.get("at").and_then(Value::as_str) {
                    Some("next") => EnqueueAt::Next,
                    _ => EnqueueAt::End,
                };
                ctl.enqueue(infos(p), at);
            }
            "player.pause" => ctl.pause(),
            "player.resume" => ctl.resume(),
            "player.stop" => ctl.stop(),
            "player.seek" => ctl.seek_ms(
                p.get("ms")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| bad("ms"))?,
            ),
            "player.next" => ctl.next(),
            "player.previous" => ctl.prev(),
            "player.set_volume" => ctl.set_volume(
                p.get("percent")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| bad("percent"))? as u32,
            ),
            "player.set_mute" => ctl.set_muted(
                p.get("on")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| bad("on"))?,
            ),
            _ => {
                return Err(RpcError {
                    code: -32601,
                    message: format!("method not found: {method}"),
                    data: None,
                });
            }
        }
        Ok(Value::Null)
    }

    // ------------------------------------------------------------ events

    fn running_with(&self, cap: impl Fn(&Capabilities) -> bool) -> Vec<(String, Arc<Rpc>)> {
        self.inner
            .slots
            .lock()
            .unwrap()
            .values()
            .filter(|s| cap(&s.status.read().unwrap().caps))
            .filter_map(|s| Some((s.cfg.id.clone(), s.rpc.read().unwrap().clone()?)))
            .collect()
    }

    /// Reporting (`playback.*`) and remote-control state (`player.*`).
    fn event_loop(&self, events: mpsc::Receiver<crate::controller::CtlEvent>) {
        use ricercar_audio::TransportStatus;
        // (queue item id, plugin id, ref) of the item being reported.
        let mut reported: Option<(u64, String, String)> = None;
        let mut listened_ms = 0u64;
        // Duration and furthest position of the reported item: a queue that
        // plays to its end stops the transport, which is still an end.
        let (mut reported_dur, mut reported_pos) = (0u64, 0u64);
        let mut last_tick = Instant::now();
        let mut last_progress = Instant::now();
        let mut last_state = Instant::now();
        let mut last_origin: Option<Origin> = None;
        while self.inner.alive.load(Ordering::SeqCst) {
            let ev = match events.recv_timeout(Duration::from_secs(1)) {
                Ok(e) => Some(e),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            };
            let Some(ctl) = self.inner.ctl.read().unwrap().upgrade() else {
                return;
            };
            let (status, cur, origin, pos, dur, vol, muted, finished) = {
                let st = ctl.lock();
                (
                    st.status,
                    st.current_item().map(|q| (q.id, q.info.uri.clone())),
                    st.origin.clone(),
                    st.pos_ms,
                    st.dur_ms,
                    st.volume,
                    st.muted,
                    st.last_finished,
                )
            };
            let playing = status == TransportStatus::Playing;
            if playing {
                listened_ms += last_tick.elapsed().as_millis() as u64;
            }
            last_tick = Instant::now();

            // ---- reporting
            let now_item = cur
                .as_ref()
                .filter(|_| status != TransportStatus::Stopped)
                .and_then(|(qid, uri)| {
                    let (pid, r) = parse_plugin_uri(uri)?;
                    Some((*qid, pid, r))
                });
            let changed = reported.as_ref().map(|r| r.0) != now_item.as_ref().map(|n| n.0);
            if !changed && reported.is_some() {
                reported_dur = reported_dur.max(dur);
                reported_pos = reported_pos.max(pos);
            }
            if changed {
                if let Some((qid, pid, r)) = reported.take() {
                    let near_end = |ms: u64| reported_dur > 0 && ms + 3_000 >= reported_dur;
                    let reason =
                        if finished == Some(qid) || near_end(listened_ms) || near_end(reported_pos)
                        {
                            "ended"
                        } else if status == TransportStatus::Stopped {
                            "stopped"
                        } else {
                            "skipped"
                        };
                    self.report(
                        &pid,
                        "playback.ended",
                        json!({"ref": r, "listened_ms": listened_ms, "reason": reason}),
                    );
                }
                listened_ms = 0;
                (reported_dur, reported_pos) = (0, 0);
                last_progress = Instant::now();
                if let Some((qid, pid, r)) = now_item {
                    self.report(&pid, "playback.started", json!({ "ref": r }));
                    reported = Some((qid, pid, r));
                }
            } else if playing
                && last_progress.elapsed() >= PROGRESS_EVERY
                && let Some((_, pid, r)) = &reported
            {
                last_progress = Instant::now();
                self.report(pid, "playback.progress", json!({"ref": r, "pos_ms": pos}));
            }

            // ---- remote control
            if let Some(Origin::Plugin(pid)) = &last_origin
                && origin != Origin::Plugin(pid.clone())
                && let Some(slot) = self.slot(pid)
                && let Some(rpc) = slot.rpc.read().unwrap().clone()
            {
                rpc.notify("player.taken_over", json!({}));
            }
            last_origin = Some(origin);
            if ev.is_some() || (playing && last_state.elapsed() >= Duration::from_secs(1)) {
                last_state = Instant::now();
                let status_name = match status {
                    TransportStatus::Playing => "playing",
                    TransportStatus::Paused => "paused",
                    TransportStatus::Stopped => "stopped",
                };
                for (pid, rpc) in self.running_with(|c| c.remote_control) {
                    let item_ref = cur
                        .as_ref()
                        .and_then(|(_, u)| parse_plugin_uri(u))
                        .filter(|(p, _)| *p == pid)
                        .map(|(_, r)| r);
                    rpc.notify(
                        "player.state",
                        json!({
                            "status": status_name,
                            "item_ref": item_ref,
                            "pos_ms": pos,
                            "dur_ms": dur,
                            "volume": vol,
                            "muted": muted,
                        }),
                    );
                }
            }
        }
    }

    fn report(&self, id: &str, method: &str, params: Value) {
        if let Some(slot) = self.slot(id)
            && slot.status.read().unwrap().caps.reporting
            && let Some(rpc) = slot.rpc.read().unwrap().clone()
        {
            rpc.notify(method, params);
        }
    }
}

struct Fatal(String);

/// A playlist edit that succeeded, with the plugin's playlists read again
/// right after it.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaylistEdited<T> {
    pub value: T,
    /// `library.playlists` after the edit; `None` when that read failed
    /// (the edit itself went through).
    pub playlists: Option<Vec<Item>>,
}

fn bad_params(message: &str) -> PluginError {
    PluginError::Other {
        code: -32602,
        message: message.into(),
    }
}

fn unsupported(what: &str) -> PluginError {
    PluginError::Other {
        code: -32601,
        message: format!("{what} not supported"),
    }
}

fn playlist_name(name: &str) -> Result<String, PluginError> {
    let name: String = name
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_PLAYLIST_NAME)
        .collect();
    if name.is_empty() {
        return Err(bad_params("empty playlist name"));
    }
    Ok(name)
}

impl Resolver for PluginHost {
    fn resolve(&self, uri: &str, purpose: Purpose) -> Result<Resolved, PluginError> {
        let (id, reference) = parse_plugin_uri(uri).ok_or(PluginError::NotFound)?;
        let r: Resolved = self.call_as(
            &id,
            "track.resolve",
            json!({"ref": reference, "purpose": purpose}),
        )?;
        if !r.url_is_acceptable() {
            return Err(PluginError::Other {
                code: -32603,
                message: "resolved URL must be http(s):// or file://".into(),
            });
        }
        Ok(r)
    }

    fn plugin_name(&self, uri: &str) -> Option<String> {
        let (id, _) = parse_plugin_uri(uri)?;
        Some(self.status(&id).map(|s| s.name).unwrap_or(id))
    }

    fn has_radio(&self, uri: &str) -> bool {
        parse_plugin_uri(uri).is_some_and(|(id, _)| {
            self.status(&id)
                .is_some_and(|s| s.state == RunState::Running && s.caps.radio)
        })
    }

    fn radio_next(
        &self,
        seed: &str,
        exclude: &[String],
        limit: usize,
    ) -> Result<Vec<TrackInfo>, PluginError> {
        let (id, seed) = parse_plugin_uri(seed).ok_or(PluginError::NotFound)?;
        let exclude: Vec<String> = exclude
            .iter()
            .filter_map(|u| {
                parse_plugin_uri(u)
                    .filter(|(p, _)| *p == id)
                    .map(|(_, r)| r)
            })
            .collect();
        Ok(self
            .radio_next_items(&id, &seed, &exclude, limit)?
            .iter()
            .map(|i| i.to_track_info(&id))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::plugin_locale;

    #[test]
    fn locale_follows_the_interface_language() {
        assert_eq!(plugin_locale("", Some("fr-CA")), "fr-CA");
        assert_eq!(plugin_locale("", None), "en-US");
        assert_eq!(plugin_locale("fr", Some("en-US")), "fr");
        assert_eq!(plugin_locale("fr", Some("fr-BE")), "fr-BE");
        assert_eq!(plugin_locale("en", None), "en");
        assert_eq!(plugin_locale("de", Some("de-AT")), "de-AT");
        assert_eq!(plugin_locale("es", Some("fr-FR")), "es");
        assert_eq!(plugin_locale("it", None), "it");
        assert_eq!(plugin_locale("ja", Some("ja-JP")), "ja-JP");
    }
}
