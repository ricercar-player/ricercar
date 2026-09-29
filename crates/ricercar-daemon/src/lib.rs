//! Shared startup for the headless daemon and the desktop app: config,
//! library, engine, UPnP, MPRIS.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use ricercar_audio::device::list_devices;
use ricercar_core::config::{self, Config};
use ricercar_core::covers::CoverCache;
use ricercar_core::{Controller, Library, watcher::WatcherHandle};

mod scrobble;

/// Command-line overrides on top of the config file.
#[derive(Debug, Clone, Default)]
pub struct Args {
    pub config: Option<PathBuf>,
    pub device: Option<String>,
    pub name: Option<String>,
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
                "--db" => out.db = Some(value(&mut args, "--db")?.into()),
                "--library" => out.roots.push(value(&mut args, "--library")?.into()),
                "--no-mpris" => out.no_mpris = true,
                "--no-upnp" => out.no_upnp = true,
                "--no-session" => out.no_session = true,
                "--headless" => out.headless = true,
                "-h" | "--help" => return Err(usage()),
                other if !other.starts_with('-') => out.open.push(other.to_string()),
                other => return Err(format!("unknown argument: {other}\n{}", usage())),
            }
        }
        Ok(out)
    }
}

pub fn usage() -> String {
    "usage: ricercar [FILE|URI]... [--headless] [--config FILE] [--device NAME] [--name NAME] [--db PATH]
                [--library DIR]... [--no-mpris] [--no-upnp] [--no-session]
       ricercar --print-devices

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
    Failed(String),
}

/// The UPnP services, restartable while the app runs.
struct Network {
    handle: RwLock<Option<ricercar_upnp::RendererHandle>>,
    status: RwLock<NetworkStatus>,
    /// Bumped on every status change.
    rev: AtomicU64,
    /// One restart at a time; holds the settings last applied.
    applied: Mutex<Option<config::NetworkConfig>>,
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
        let old = self.handle.write().unwrap().take();
        let port = old.as_ref().map(|h| h.port).unwrap_or(0);
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
                *self.handle.write().unwrap() = Some(h);
                self.set_status(NetworkStatus::Running { port });
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
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,symphonia=warn".into()),
        )
        .try_init();
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
        disabled: args.no_upnp,
    });
    network.apply(&ctl, &cfg.network);

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

    if !args.open.is_empty() {
        let infos = args
            .open
            .iter()
            .map(|a| ricercar_core::TrackInfo::from_uri(&to_uri(a)))
            .collect();
        ctl.play_tracks(infos, 0, ricercar_core::PlayContext::None);
    }

    Ok(AppContext {
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
        let (old_roots, old_network, cfg) = {
            let mut c = self.config.write().unwrap();
            let (roots, network) = (c.library.clone(), c.network.clone());
            f(&mut c);
            (roots, network, c.clone())
        };
        if let Err(e) = cfg.save(&self.config_path) {
            tracing::warn!("save config: {e}");
        }
        if cfg.audio.device != self.ctl.device_name() {
            self.ctl.set_device(&cfg.audio.device);
        }
        if old_roots != cfg.library {
            self.lib.retain_roots(&cfg.library.roots);
            rescan_in_background(&self.lib, &cfg, &self.watcher);
        }
        if old_network != cfg.network {
            self.restart_network();
        }
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
            disabled: false,
        };
        let mut cfg = config::NetworkConfig {
            name: "One".into(),
            renderer: true,
            media_server: false,
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
}
