//! The plugin host: one supervised process per enabled plugin, typed calls,
//! URL resolution for the controller, playback reporting and remote control.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
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
use super::{
    AuthBegin, AuthState, AuthStatus, Capabilities, Item, OutputInfo, PROTOCOL, PluginError,
    PluginInfo, Purpose, Resolved, Resolver, parse_plugin_uri,
};
use crate::config::PluginConfig;
use crate::controller::{Controller, EnqueueAt, Origin, TrackInfo};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
const AUTH_COMPLETE_TIMEOUT: Duration = Duration::from_secs(30);
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// Restart delays after a crash; the last one repeats.
const BACKOFF: [u64; 4] = [1, 2, 5, 30];
/// A process that ran this long starts the backoff over.
const STABLE: Duration = Duration::from_secs(60);
const PROGRESS_EVERY: Duration = Duration::from_secs(30);

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
}

impl PluginStatus {
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
    cfg: PluginConfig,
    stop: AtomicBool,
    rpc: RwLock<Option<Arc<Rpc>>>,
    status: RwLock<PluginStatus>,
    thread: Mutex<Option<JoinHandle<()>>>,
    blocked_until: Mutex<Option<Instant>>,
}

struct Inner {
    configs: Mutex<Vec<PluginConfig>>,
    slots: Mutex<HashMap<String, Arc<Slot>>>,
    output: RwLock<OutputInfo>,
    rev: AtomicU64,
    ctl: RwLock<Weak<Controller>>,
    version: String,
    locale: String,
    data_root: PathBuf,
    cache_root: PathBuf,
    alive: AtomicBool,
}

/// Starts, supervises and talks to the declared plugins. Cheap to clone.
#[derive(Clone)]
pub struct PluginHost {
    inner: Arc<Inner>,
}

fn locale() -> String {
    let lang = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|v| !v.is_empty() && v != "C" && v != "POSIX")
        .unwrap_or_else(|| "en_US".into());
    lang.split(['.', '@'])
        .next()
        .unwrap_or("en_US")
        .replace('_', "-")
}

fn map_call_error(e: CallError) -> PluginError {
    match e {
        CallError::Timeout => PluginError::Timeout,
        CallError::Closed => PluginError::NotRunning,
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
                locale: locale(),
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
                Some(s) => s.status.read().unwrap().clone(),
                None => PluginStatus {
                    id: c.id.clone(),
                    name: c.id.clone(),
                    version: String::new(),
                    state: if c.enabled {
                        RunState::Failed("invalid id".into())
                    } else {
                        RunState::Disabled
                    },
                    caps: Capabilities::default(),
                    auth: None,
                },
            })
            .collect()
    }

    pub fn status(&self, id: &str) -> Option<PluginStatus> {
        self.statuses().into_iter().find(|s| s.id == id)
    }

    /// Start, stop or restart processes to match `configs` (applied live).
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
        {
            let mut slots = self.inner.slots.lock().unwrap();
            let ids: Vec<String> = slots.keys().cloned().collect();
            for id in ids {
                let keep = configs
                    .iter()
                    .any(|c| c.enabled && c.id == id && slots[&id].cfg == *c);
                if !keep && let Some(s) = slots.remove(&id) {
                    to_stop.push(s);
                }
            }
            for c in configs.iter().filter(|c| c.enabled) {
                if !slots.contains_key(&c.id) {
                    let slot = Arc::new(Slot {
                        cfg: c.clone(),
                        stop: AtomicBool::new(false),
                        rpc: RwLock::new(None),
                        status: RwLock::new(PluginStatus {
                            id: c.id.clone(),
                            name: c.id.clone(),
                            version: String::new(),
                            state: RunState::Starting,
                            caps: Capabilities::default(),
                            auth: None,
                        }),
                        thread: Mutex::new(None),
                        blocked_until: Mutex::new(None),
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
        for s in to_stop {
            Self::stop_slot(&s);
        }
        self.touch();
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
    pub fn set_output(&self, output: OutputInfo) {
        if *self.inner.output.read().unwrap() == output {
            return;
        }
        *self.inner.output.write().unwrap() = output.clone();
        let params = serde_json::to_value(&output).unwrap_or(Value::Null);
        for s in self.inner.slots.lock().unwrap().values() {
            if let Some(rpc) = s.rpc.read().unwrap().clone() {
                rpc.notify("output.changed", json!({ "output": params }));
            }
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
                        tracing::info!("plugin[{id}] {line}");
                    }
                });
        }
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
        let init = rpc.call(
            "initialize",
            json!({
                "protocol": PROTOCOL,
                "host": {"name": "ricercar", "version": self.inner.version},
                "data_dir": data_dir,
                "cache_dir": cache_dir,
                "locale": self.inner.locale,
                "output": output,
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
        }
        *slot.rpc.write().unwrap() = Some(rpc.clone());
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
                    return Ok(());
                }
                Ok(None) => {}
                Err(_) => return Ok(()),
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// `shutdown` (when `polite`), up to 2 s, then kill.
    fn kill(child: &mut Child, rpc: &Rpc, polite: bool) {
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        if polite {
            let _ = rpc.call("shutdown", json!({}), SHUTDOWN_GRACE);
        }
        while Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
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
        if r == Err(PluginError::Network) {
            r = rpc.call(method, params, timeout).map_err(map_call_error);
        }
        match &r {
            Err(PluginError::RateLimited { retry_after }) => {
                *slot.blocked_until.lock().unwrap() =
                    Some(Instant::now() + Duration::from_secs(*retry_after));
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
        Ok((items_of(v.get("sections")), home))
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
        Ok((
            items_of(v.get("items")),
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
        Ok(groups
            .into_iter()
            .filter_map(|g| {
                let kind = serde_json::from_value(g.get("kind")?.clone()).ok()?;
                Some((
                    kind,
                    items_of(g.get("items")),
                    g.get("has_more").and_then(Value::as_bool).unwrap_or(false),
                ))
            })
            .collect())
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
        items_of(Some(&json!([v])))
            .pop()
            .ok_or(PluginError::NotFound)
    }

    pub fn favorites_set(&self, id: &str, reference: &str, on: bool) -> Result<(), PluginError> {
        if !self.status(id).is_some_and(|s| s.caps.favorites) {
            return Err(PluginError::Other {
                code: -32601,
                message: "favorites not supported".into(),
            });
        }
        self.call(id, "favorites.set", json!({"ref": reference, "on": on}))
            .map(|_| ())
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
                if method == "auth.changed" {
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
            let (status, cur, origin, pos, dur, vol, muted) = {
                let st = ctl.lock();
                (
                    st.status,
                    st.current_item().map(|q| (q.id, q.info.uri.clone())),
                    st.origin.clone(),
                    st.pos_ms,
                    st.dur_ms,
                    st.volume,
                    st.muted,
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
            if changed {
                if let Some((_, pid, r)) = reported.take() {
                    let reason = if status == TransportStatus::Stopped {
                        "stopped"
                    } else if dur > 0 && listened_ms + 3_000 >= dur {
                        "ended"
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

/// Items of a JSON array; entries that do not parse, or with a missing or
/// oversized ref, are dropped.
fn items_of(v: Option<&Value>) -> Vec<Item> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|i| serde_json::from_value::<Item>(i.clone()).ok())
                .filter(|i| !i.reference.is_empty() && i.reference.len() <= super::MAX_REF)
                .collect()
        })
        .unwrap_or_default()
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
}
