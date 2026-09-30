//! Shared startup for the headless daemon and the desktop app: config,
//! library, engine, UPnP, MPRIS.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use ricercar_audio::device::list_devices;
use ricercar_core::config::{self, Config};
use ricercar_core::covers::CoverCache;
use ricercar_core::plugin::settings;
use ricercar_core::{Controller, Library, watcher::WatcherHandle};

pub mod diag;
pub mod logging;
mod scrobble;

/// Up, non-loopback IPv4 interfaces (name, address) the UPnP services can
/// be limited to.
pub use ricercar_upnp::interfaces as network_interfaces;

/// Command-line overrides on top of the config file.
#[derive(Debug, Clone, Default)]
pub struct Args {
    pub config: Option<PathBuf>,
    pub device: Option<String>,
    pub name: Option<String>,
    /// Network interface for UPnP (`eth0`); overrides the config.
    pub interface: Option<String>,
    pub db: Option<PathBuf>,
    pub roots: Vec<PathBuf>,
    pub no_mpris: bool,
    pub no_upnp: bool,
    /// Do not touch the saved session (tests, one-off runs).
    pub no_session: bool,
    pub headless: bool,
    /// Files or URIs to play right away.
    pub open: Vec<String>,
}

impl Args {
    pub fn parse(args: impl Iterator<Item = String>) -> Result<Args, String> {
        let mut out = Args::default();
        let mut args = args.peekable();
        let value = |args: &mut std::iter::Peekable<_>, flag: &str| -> Result<String, String> {
            args.next().ok_or_else(|| format!("{flag} needs a value"))
        };
        while let Some(a) = args.next() {
            match a.as_str() {
                "--config" => out.config = Some(value(&mut args, "--config")?.into()),
                "--device" => out.device = Some(value(&mut args, "--device")?),
                "--name" => out.name = Some(value(&mut args, "--name")?),
                "--interface" => out.interface = Some(value(&mut args, "--interface")?),
                "--db" => out.db = Some(value(&mut args, "--db")?.into()),
                "--library" => out.roots.push(value(&mut args, "--library")?.into()),
                "--no-mpris" => out.no_mpris = true,
                "--no-upnp" => out.no_upnp = true,
                "--no-session" => out.no_session = true,
                "--headless" => out.headless = true,
                "-h" | "--help" => {
                    println!("{}", usage());
                    std::process::exit(0);
                }
                "-V" | "--version" => {
                    println!("{}", version());
                    std::process::exit(0);
                }
                other if !other.starts_with('-') => out.open.push(other.to_string()),
                other => return Err(format!("unknown argument: {other}\n{}", usage())),
            }
        }
        Ok(out)
    }
}

pub fn version() -> String {
    format!("ricercar {}", env!("CARGO_PKG_VERSION"))
}

pub fn usage() -> String {
    "usage: ricercar [FILE|URI]... [--headless] [--config FILE] [--device NAME] [--name NAME] [--db PATH]
                [--library DIR]... [--interface IFACE] [--no-mpris] [--no-upnp] [--no-session]
       ricercar --print-devices
       ricercar --help | --version

  --interface IFACE   serve UPnP on this network interface only (eth0, wlan0…)

Settings live in ~/.config/ricercar/config.toml; flags override them for this run."
        .into()
}

pub fn print_devices() {
    for d in list_devices() {
        println!(
            "{:<14} {}{}",
            d.name,
            d.description,
            if d.kind.is_bit_perfect() && d.kind == ricercar_audio::DeviceKind::Hardware {
                "  [bit-perfect]"
            } else {
                ""
            },
        );
    }
}

/// A command-line argument as a playable URI (paths become file:// URIs).
pub fn to_uri(arg: &str) -> String {
    if arg.contains("://") {
        return arg.to_string();
    }
    let p = std::fs::canonicalize(arg).unwrap_or_else(|_| PathBuf::from(arg));
    ricercar_core::meta::file_uri(&p)
}

/// When ricercar already runs, hand it the files to play (or bring it to
/// the front) over MPRIS and return true: one instance owns the DAC.
pub fn forward_to_running(args: &Args) -> bool {
    let Ok(conn) = zbus::blocking::Connection::session() else {
        return false;
    };
    let owned = conn
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "NameHasOwner",
            &(ricercar_mpris::BUS_NAME,),
        )
        .ok()
        .and_then(|m| m.body().deserialize::<bool>().ok())
        .unwrap_or(false);
    if !owned {
        return false;
    }
    let call = |iface: &str, member: &str, uri: Option<&str>| {
        let r = match uri {
            Some(u) => conn.call_method(
                Some(ricercar_mpris::BUS_NAME),
                ricercar_mpris::PATH,
                Some(iface),
                member,
                &(u,),
            ),
            None => conn.call_method(
                Some(ricercar_mpris::BUS_NAME),
                ricercar_mpris::PATH,
                Some(iface),
                member,
                &(),
            ),
        };
        if let Err(e) = r {
            eprintln!("ricercar: {member}: {e}");
        }
    };
    match args.open.first() {
        Some(first) => call(
            "org.mpris.MediaPlayer2.Player",
            "OpenUri",
            Some(&to_uri(first)),
        ),
        None => call("org.mpris.MediaPlayer2", "Raise", None),
    }
    true
}

/// Front-end callbacks for desktop integration (MPRIS Raise/Quit).
#[derive(Clone, Default)]
pub struct Hooks {
    pub on_raise: Option<Arc<dyn Fn() + Send + Sync>>,
    pub on_quit: Option<Arc<dyn Fn() + Send + Sync>>,
}

pub struct AppContext {
    pub ctl: Arc<Controller>,
    pub lib: Arc<Library>,
    pub covers: Arc<CoverCache>,
    pub config: Arc<RwLock<Config>>,
    pub config_path: PathBuf,
    pub quit: Arc<AtomicBool>,
    /// Source plugins declared in the config (docs/plugins.md).
    pub plugins: ricercar_core::plugin::PluginHost,
    network: Arc<Network>,
    watcher: Arc<RwLock<Option<WatcherHandle>>>,
    _mpris: Option<zbus::blocking::Connection>,
}

/// What the network services are doing, for the settings page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkStatus {
    /// Renderer and server both turned off (or `--no-upnp`).
    Off,
    /// Being (re)started after a settings change.
    Starting,
    Running {
        port: u16,
    },
    /// The chosen interface is missing, down or without an IPv4 address:
    /// the services start once it has one.
    InterfaceMissing(String),
    Failed(String),
}

/// How often a chosen interface is checked (address back, or changed).
const INTERFACE_POLL: std::time::Duration = std::time::Duration::from_secs(3);

/// The UPnP services, restartable while the app runs.
struct Network {
    handle: RwLock<Option<ricercar_upnp::RendererHandle>>,
    status: RwLock<NetworkStatus>,
    /// Bumped on every status change.
    rev: AtomicU64,
    /// One restart at a time; holds the settings last applied.
    applied: Mutex<Option<config::NetworkConfig>>,
    /// Last HTTP port served, asked again after a stop or an interface
    /// outage: control points keep their cached URLs.
    last_port: std::sync::atomic::AtomicU16,
    disabled: bool,
}

impl Network {
    fn set_status(&self, s: NetworkStatus) {
        *self.status.write().unwrap() = s;
        self.rev.fetch_add(1, Ordering::SeqCst);
    }

    /// Bring the services in line with `cfg` (blocking: joins threads).
    fn apply(&self, ctl: &Arc<Controller>, cfg: &config::NetworkConfig) {
        let mut applied = self.applied.lock().unwrap();
        if applied.as_ref() == Some(cfg) {
            return;
        }
        *applied = Some(cfg.clone());
        self.restart(ctl, cfg, &mut applied);
    }

    /// With an interface chosen: start once it is back, restart when its
    /// address changes (a new bind; control points get byebye then alive).
    fn check_interface(&self, ctl: &Arc<Controller>) {
        let mut applied = self.applied.lock().unwrap();
        let Some(cfg) = applied.clone() else {
            return;
        };
        if cfg.interface.is_empty() || self.disabled || !(cfg.renderer || cfg.media_server) {
            return;
        }
        let now = ricercar_upnp::interface_ipv4(&cfg.interface);
        let bound = self.handle.read().unwrap().as_ref().and_then(|h| h.ip);
        let status = self.status.read().unwrap().clone();
        let again = match status {
            NetworkStatus::InterfaceMissing(_) => now.is_some(),
            NetworkStatus::Running { .. } => now != bound,
            _ => false,
        };
        if again {
            tracing::info!(
                "interface {}: address now {}",
                cfg.interface,
                now.map(|ip| ip.to_string())
                    .unwrap_or_else(|| "none".into())
            );
            self.restart(ctl, &cfg, &mut applied);
        }
    }

    /// Stop what runs and start with `cfg`; `applied` is the held lock.
    fn restart(
        &self,
        ctl: &Arc<Controller>,
        cfg: &config::NetworkConfig,
        applied: &mut Option<config::NetworkConfig>,
    ) {
        let old = self.handle.write().unwrap().take();
        let port = self.last_port.load(Ordering::SeqCst);
        if let Some(mut h) = old {
            self.set_status(NetworkStatus::Starting);
            h.stop();
        }
        if self.disabled || !(cfg.renderer || cfg.media_server) {
            self.set_status(NetworkStatus::Off);
            return;
        }
        let opts = ricercar_upnp::UpnpOptions {
            name: cfg.name.clone(),
            renderer: cfg.renderer,
            media_server: cfg.media_server,
            port,
            interface: (!cfg.interface.is_empty()).then(|| cfg.interface.clone()),
        };
        match ricercar_upnp::start(ctl.clone(), &opts) {
            Ok(h) => {
                tracing::info!(
                    renderer = opts.renderer,
                    media_server = opts.media_server,
                    "UPnP \"{}\" on port {}",
                    opts.name,
                    h.port
                );
                let port = h.port;
                self.last_port.store(port, Ordering::SeqCst);
                *self.handle.write().unwrap() = Some(h);
                self.set_status(NetworkStatus::Running { port });
            }
            Err(e)
                if opts.interface.is_some() && e.kind() == std::io::ErrorKind::AddrNotAvailable =>
            {
                // Kept as applied: the interface watch starts the services
                // when the interface comes back.
                tracing::warn!("UPnP waiting: {e}");
                self.set_status(NetworkStatus::InterfaceMissing(cfg.interface.clone()));
            }
            Err(e) => {
                tracing::warn!("UPnP unavailable: {e}");
                // Let the next settings change try again.
                *applied = None;
                self.set_status(NetworkStatus::Failed(e.to_string()));
            }
        }
    }
}

pub fn init_logging() {
    logging::init();
}

pub fn startup(args: Args, hooks: Hooks) -> Result<AppContext, Box<dyn std::error::Error>> {
    init_logging();
    let config_path = args.config.clone().unwrap_or_else(Config::default_path);
    let first_run = !config_path.exists();
    let mut cfg = Config::load(&config_path);
    if let Some(d) = &args.device {
        cfg.audio.device = d.clone();
    }
    if let Some(n) = &args.name {
        cfg.network.name = n.clone();
    }
    if let Some(i) = &args.interface {
        cfg.network.interface = i.clone();
    }
    for r in &args.roots {
        if !cfg.library.roots.contains(r) {
            cfg.library.roots.push(r.clone());
        }
    }
    if let Ok(env_roots) = std::env::var("RICERCAR_LIBRARY") {
        for p in env_roots.split(':').filter(|p| !p.is_empty()) {
            let p = PathBuf::from(p);
            if !cfg.library.roots.contains(&p) {
                cfg.library.roots.push(p);
            }
        }
    }
    if first_run && args.config.is_none() {
        // Persist detected defaults (music dir) so the settings page shows them.
        let _ = cfg.save(&config_path);
    }

    let db = args
        .db
        .clone()
        .unwrap_or_else(|| config::data_dir().join("library.db"));
    let lib = Arc::new(Library::open(&db)?);
    let covers = Arc::new(CoverCache::new(CoverCache::default_dir()));

    let ctl = Arc::new(Controller::new(lib.clone(), &cfg.audio.device));
    let plugins = ricercar_core::plugin::PluginHost::new(
        env!("CARGO_PKG_VERSION"),
        config::data_dir(),
        config::cache_dir(),
    );
    ctl.set_resolver(Arc::new(plugins.clone()));
    plugins.attach(&ctl);
    plugins.set_output(output_info(&cfg.audio.device, None));
    plugins.reconcile(&cfg.plugins);
    if !args.no_session {
        ctl.enable_session(
            config::data_dir().join("session.json"),
            cfg.audio.restore_session,
        );
    }
    tracing::info!(device = %cfg.audio.device, "audio engine up");

    let watcher = Arc::new(RwLock::new(None));
    rescan_in_background(&lib, &cfg, &watcher);

    let quit = Arc::new(AtomicBool::new(false));

    let network = Arc::new(Network {
        handle: RwLock::new(None),
        status: RwLock::new(NetworkStatus::Off),
        rev: AtomicU64::new(0),
        applied: Mutex::new(None),
        last_port: Default::default(),
        disabled: args.no_upnp,
    });
    network.apply(&ctl, &cfg.network);
    watch_interface(network.clone(), ctl.clone(), quit.clone());

    let mpris = if !args.no_mpris {
        let opts = ricercar_mpris::MprisOptions {
            covers: Some(covers.clone()),
            on_raise: hooks.on_raise.clone(),
            on_quit: Some(hooks.on_quit.clone().unwrap_or_else(|| {
                let q = quit.clone();
                Arc::new(move || q.store(true, Ordering::SeqCst))
            })),
        };
        match ricercar_mpris::serve_with(ctl.clone(), opts) {
            Ok(conn) => {
                ricercar_mpris::spawn_event_loop(conn.clone(), ctl.clone(), quit.clone());
                tracing::info!("MPRIS: {}", ricercar_mpris::BUS_NAME);
                Some(conn)
            }
            Err(e) => {
                tracing::warn!("MPRIS unavailable: {e}");
                None
            }
        }
    } else {
        None
    };

    let config = Arc::new(RwLock::new(cfg));
    scrobble::spawn(ctl.clone(), config.clone());
    watch_plugin_tables(
        config_path.clone(),
        config.clone(),
        plugins.clone(),
        quit.clone(),
    );

    if !args.open.is_empty() {
        let infos = args
            .open
            .iter()
            .map(|a| ricercar_core::TrackInfo::from_uri(&to_uri(a)))
            .collect();
        ctl.play_tracks(infos, 0, ricercar_core::PlayContext::None);
    }

    Ok(AppContext {
        plugins,
        network,
        ctl,
        lib,
        covers,
        config,
        config_path,
        quit,
        watcher,
        _mpris: mpris,
    })
}

/// What plugins are told about the output. `caps`: accepted rates and
/// deepest content bits, when the device was probed.
pub fn output_info(
    device: &str,
    caps: Option<(Vec<u32>, Option<u8>)>,
) -> ricercar_core::plugin::OutputInfo {
    let kind = ricercar_audio::device::classify(device);
    let (rates, max_bits) = caps.unwrap_or_default();
    ricercar_core::plugin::OutputInfo {
        device: device.to_string(),
        bit_perfect: kind == ricercar_audio::DeviceKind::Hardware,
        max_rate: rates.iter().copied().max(),
        max_bits,
        rates,
    }
}

/// Config saves still running on another thread: until they are done the
/// file is older than the config in memory, and not a hand edit.
static SAVES_PENDING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Start, stop or tell plugins to match the config as it is now, one call
/// at a time.
fn reconcile_plugins(host: &ricercar_core::plugin::PluginHost, config: &RwLock<Config>) {
    static APPLYING: Mutex<()> = Mutex::new(());
    let _one = APPLYING.lock().unwrap_or_else(|e| e.into_inner());
    let list = config.read().unwrap().plugins.clone();
    host.reconcile(&list);
}

/// Follow the chosen network interface (a no-op while none is chosen).
fn watch_interface(network: Arc<Network>, ctl: Arc<Controller>, quit: Arc<AtomicBool>) {
    let _ = std::thread::Builder::new()
        .name("ricercar-upnp-iface".into())
        .spawn(move || {
            while !quit.load(Ordering::Relaxed) {
                std::thread::sleep(INTERFACE_POLL);
                network.check_interface(&ctl);
            }
        });
}

/// `[[plugins]]` edited in config.toml by hand applies without a restart:
/// only that section is taken from the file (the rest is ours to save).
fn watch_plugin_tables(
    path: PathBuf,
    config: Arc<RwLock<Config>>,
    host: ricercar_core::plugin::PluginHost,
    quit: Arc<AtomicBool>,
) {
    let mtime = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let _ = std::thread::Builder::new()
        .name("ricercar-config-watch".into())
        .spawn(move || {
            let mut seen = mtime(&path);
            while !quit.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_secs(2));
                let now = mtime(&path);
                if now == seen || SAVES_PENDING.load(Ordering::SeqCst) > 0 {
                    continue;
                }
                seen = now;
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                // A half-written or broken file changes nothing.
                let Ok(file) = toml::from_str::<Config>(&text) else {
                    continue;
                };
                let changed = {
                    let mut c = config.write().unwrap();
                    let changed = c.plugins != file.plugins;
                    c.plugins = file.plugins.clone();
                    changed
                };
                if changed {
                    tracing::info!("plugins changed in {}", path.display());
                    reconcile_plugins(&host, &config);
                }
            }
        });
}

/// Incremental scan of the configured roots off the calling thread, then
/// (re)start the filesystem watcher.
fn rescan_in_background(
    lib: &Arc<Library>,
    cfg: &Config,
    watcher: &Arc<RwLock<Option<WatcherHandle>>>,
) {
    let lib = lib.clone();
    let roots = cfg.library.roots.clone();
    let watch = cfg.library.watch;
    let watcher = watcher.clone();
    std::thread::Builder::new()
        .name("ricercar-scan".into())
        .spawn(move || {
            let t = std::time::Instant::now();
            let r = lib.scan_roots(&roots);
            tracing::info!(
                added = r.added,
                updated = r.updated,
                removed = r.removed,
                total = r.total,
                "library scanned in {:.1?}",
                t.elapsed()
            );
            ricercar_core::profile::record(
                &format!(
                    "library scan ({} files, {} indexed)",
                    r.total,
                    r.added + r.updated
                ),
                t.elapsed(),
            );
            let new = (watch && !roots.is_empty()).then(|| WatcherHandle::start(lib, roots));
            *watcher.write().unwrap() = new;
        })
        .expect("spawn scan");
}

impl AppContext {
    /// Persist the config and apply library changes (roots/watch).
    pub fn update_config(&self, f: impl FnOnce(&mut Config)) {
        let (old_roots, old_network, old_plugins, cfg) = {
            let mut c = self.config.write().unwrap();
            let (roots, network, plugins) =
                (c.library.clone(), c.network.clone(), c.plugins.clone());
            f(&mut c);
            (roots, network, plugins, c.clone())
        };
        if let Err(e) = cfg.save(&self.config_path) {
            tracing::warn!("save config: {e}");
        }
        if cfg.audio.device != self.ctl.device_name() {
            self.ctl.set_device(&cfg.audio.device);
            self.plugins
                .set_output(output_info(&cfg.audio.device, None));
        }
        if old_plugins != cfg.plugins {
            self.apply_plugins();
        }
        if old_roots != cfg.library {
            self.lib.retain_roots(&cfg.library.roots);
            rescan_in_background(&self.lib, &cfg, &self.watcher);
        }
        if old_network != cfg.network {
            self.restart_network();
        }
    }

    /// Bring the plugin processes in line with the config, off the calling
    /// thread: stopping a plugin can take its 2 s grace. Applied one at a
    /// time, each with the config as it is then, so a burst of changes
    /// ends on the latest.
    fn apply_plugins(&self) {
        let (host, config) = (self.plugins.clone(), self.config.clone());
        std::thread::spawn(move || reconcile_plugins(&host, &config));
    }

    /// Change one setting a plugin declares (docs/plugins.md, "Settings"):
    /// checked against its declaration, then saved and sent to the plugin
    /// (or the plugin restarted, when the setting asks for it) off the
    /// calling thread. Returns the value in effect.
    pub fn set_plugin_setting(
        &self,
        id: &str,
        key: &str,
        value: &settings::Value,
    ) -> Result<settings::Value, settings::SettingError> {
        let schema = self
            .plugins
            .status(id)
            .map(|s| s.settings)
            .unwrap_or_default();
        let v = {
            let mut c = self.config.write().unwrap();
            let p = c
                .plugins
                .iter_mut()
                .find(|p| p.id == id)
                .ok_or(settings::SettingError::UnknownKey)?;
            let before = p.settings.clone();
            let v = settings::store(&schema, &mut p.settings, key, value)?;
            if p.settings == before {
                return Ok(v);
            }
            v
        };
        self.save_plugin_settings();
        Ok(v)
    }

    /// Forget every stored value of a plugin's settings: the defaults apply.
    pub fn reset_plugin_settings(&self, id: &str) {
        {
            let mut c = self.config.write().unwrap();
            match c.plugins.iter_mut().find(|p| p.id == id) {
                Some(p) if !p.settings.is_empty() => p.settings.clear(),
                _ => return,
            }
        }
        self.save_plugin_settings();
    }

    /// Save the config and tell the plugins, off the calling thread. The
    /// config stays locked while it is written, so an older state never
    /// lands on disk after a newer one.
    fn save_plugin_settings(&self) {
        let (host, config, path) = (
            self.plugins.clone(),
            self.config.clone(),
            self.config_path.clone(),
        );
        SAVES_PENDING.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            if let Err(e) = config.read().unwrap().save(&path) {
                tracing::warn!("save config: {e}");
            }
            SAVES_PENDING.fetch_sub(1, Ordering::SeqCst);
            reconcile_plugins(&host, &config);
        });
    }

    /// Restart the UPnP services with the current settings, off the calling
    /// thread (stopping waits for the service threads). Same UDNs and, when
    /// free, the same port: control points find the device again.
    pub fn restart_network(&self) {
        let (network, ctl, config) = (self.network.clone(), self.ctl.clone(), self.config.clone());
        std::thread::Builder::new()
            .name("ricercar-upnp-restart".into())
            .spawn(move || {
                // Restarts are serialized and skip settings already applied,
                // so a burst of edits ends on the latest one.
                let cfg = config.read().unwrap().network.clone();
                network.apply(&ctl, &cfg);
            })
            .expect("spawn upnp restart");
    }

    pub fn network_status(&self) -> NetworkStatus {
        self.network.status.read().unwrap().clone()
    }

    /// Changes with every network status change (cheap to poll).
    pub fn network_revision(&self) -> u64 {
        self.network.rev.load(Ordering::SeqCst)
    }

    pub fn renderer_port(&self) -> Option<u16> {
        match self.network_status() {
            NetworkStatus::Running { port } => Some(port),
            _ => None,
        }
    }

    pub fn rescan(&self) {
        let cfg = self.config.read().unwrap().clone();
        rescan_in_background(&self.lib, &cfg, &self.watcher);
    }

    pub fn wait_until_quit(&self) {
        let q = self.quit.clone();
        let _ = ctrlc::set_handler(move || q.store(true, Ordering::SeqCst));
        while !self.quit.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        self.shutdown();
    }

    pub fn shutdown(&self) {
        self.quit.store(true, Ordering::SeqCst);
        if let Some(mut h) = self.network.handle.write().unwrap().take() {
            h.stop();
        }
        self.plugins.shutdown();
        self.ctl.shutdown();
        tracing::info!("bye");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_restarts_on_the_same_port() {
        let lib = Arc::new(Library::in_memory().unwrap());
        let ctl = Arc::new(Controller::new(lib, "null"));
        let net = Network {
            handle: RwLock::new(None),
            status: RwLock::new(NetworkStatus::Off),
            rev: AtomicU64::new(0),
            applied: Mutex::new(None),
            last_port: Default::default(),
            disabled: false,
        };
        let mut cfg = config::NetworkConfig {
            name: "One".into(),
            renderer: true,
            media_server: false,
            interface: String::new(),
        };
        net.apply(&ctl, &cfg);
        let NetworkStatus::Running { port } = net.status.read().unwrap().clone() else {
            panic!("not running");
        };
        let rev = net.rev.load(Ordering::SeqCst);
        net.apply(&ctl, &cfg);
        assert_eq!(
            net.rev.load(Ordering::SeqCst),
            rev,
            "same settings: no restart"
        );

        cfg.name = "Two".into();
        cfg.media_server = true;
        net.apply(&ctl, &cfg);
        assert_eq!(*net.status.read().unwrap(), NetworkStatus::Running { port });

        cfg.renderer = false;
        cfg.media_server = false;
        net.apply(&ctl, &cfg);
        assert_eq!(*net.status.read().unwrap(), NetworkStatus::Off);
        assert!(net.handle.read().unwrap().is_none());
    }

    #[test]
    fn missing_interface_waits_then_follows() {
        let lib = Arc::new(Library::in_memory().unwrap());
        let ctl = Arc::new(Controller::new(lib, "null"));
        let net = Network {
            handle: RwLock::new(None),
            status: RwLock::new(NetworkStatus::Off),
            rev: AtomicU64::new(0),
            applied: Mutex::new(None),
            last_port: Default::default(),
            disabled: false,
        };
        let mut cfg = config::NetworkConfig {
            interface: "no-such-if0".into(),
            ..Default::default()
        };
        net.apply(&ctl, &cfg);
        assert_eq!(
            *net.status.read().unwrap(),
            NetworkStatus::InterfaceMissing("no-such-if0".into())
        );
        assert!(net.handle.read().unwrap().is_none(), "no fallback to all");
        // Still missing: the watch leaves it waiting.
        net.check_interface(&ctl);
        assert!(matches!(
            *net.status.read().unwrap(),
            NetworkStatus::InterfaceMissing(_)
        ));

        // Switching to an interface that exists starts the services on it.
        let Some(lo) = ricercar_upnp::interface_ipv4("lo") else {
            return;
        };
        cfg.interface = "lo".into();
        net.apply(&ctl, &cfg);
        assert!(matches!(
            *net.status.read().unwrap(),
            NetworkStatus::Running { .. }
        ));
        assert_eq!(net.handle.read().unwrap().as_ref().unwrap().ip, Some(lo));
        // Same address: the watch changes nothing.
        let rev = net.rev.load(Ordering::SeqCst);
        net.check_interface(&ctl);
        assert_eq!(net.rev.load(Ordering::SeqCst), rev);
        net.handle.write().unwrap().take();
    }

    #[test]
    fn version_and_usage() {
        assert_eq!(version(), format!("ricercar {}", env!("CARGO_PKG_VERSION")));
        assert!(usage().contains("--version"));
        let a = Args::parse(["--headless".to_string(), "a.flac".to_string()].into_iter()).unwrap();
        assert!(a.headless && a.open == ["a.flac"]);
        assert!(Args::parse(["--bogus".to_string()].into_iter()).is_err());
        assert!(usage().contains("--interface"));
        let a = Args::parse(["--interface".to_string(), "eth0".to_string()].into_iter()).unwrap();
        assert_eq!(a.interface.as_deref(), Some("eth0"));
        assert!(a.open.is_empty());
        assert!(Args::parse(["--interface".to_string()].into_iter()).is_err());
    }

    #[test]
    fn output_info_for_plugins() {
        let o = output_info("hw:1,0", Some((vec![44_100, 96_000, 192_000], Some(24))));
        assert!(o.bit_perfect);
        assert_eq!((o.max_rate, o.max_bits), (Some(192_000), Some(24)));
        let o = output_info("default", None);
        assert!(!o.bit_perfect && o.rates.is_empty() && o.max_rate.is_none());
    }

    #[test]
    fn plugin_tables_edited_by_hand_apply_live() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[audio]\ndevice = \"null\"\n").unwrap();
        let config = Arc::new(RwLock::new(Config::load(&path)));
        let host =
            ricercar_core::plugin::PluginHost::new("t", dir.path().join("d"), dir.path().join("c"));
        let quit = Arc::new(AtomicBool::new(false));
        watch_plugin_tables(path.clone(), config.clone(), host.clone(), quit.clone());
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(
            &path,
            "[audio]\ndevice = \"null\"\n\n[[plugins]]\nid = \"hand\"\ncommand = \"/nonexistent/plugin\"\nenabled = false\n",
        )
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        while host.statuses().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        quit.store(true, Ordering::SeqCst);
        assert_eq!(host.statuses()[0].id, "hand");
        assert_eq!(config.read().unwrap().plugins.len(), 1);
        host.shutdown();
    }
}
