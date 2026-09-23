use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ricercar_audio::device::list_devices;
use ricercar_core::{watcher::WatcherHandle, Controller, Library};

#[derive(Debug, Clone)]
pub struct Config {
    pub device: String,
    pub name: String,
    pub db: PathBuf,
    pub roots: Vec<PathBuf>,
    pub mpris: bool,
    pub upnp: bool,
}

impl Config {
    pub fn from_args(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut cfg = Config {
            device: "default".into(),
            name: "ricercar".into(),
            db: default_db(),
            roots: Vec::new(),
            mpris: true,
            upnp: true,
        };
        let mut args = args.peekable();
        while let Some(a) = args.next() {
            match a.as_str() {
                "--device" => {
                    cfg.device = args.next().ok_or("--device needs a value")?;
                }
                "--name" => {
                    cfg.name = args.next().ok_or("--name needs a value")?;
                }
                "--db" => {
                    cfg.db = args.next().ok_or("--db needs a value")?.into();
                }
                "--library" => {
                    cfg.roots
                        .push(args.next().ok_or("--library needs a value")?.into());
                }
                "--no-mpris" => cfg.mpris = false,
                "--no-upnp" => cfg.upnp = false,
                "-h" | "--help" => return Err(usage()),
                other => return Err(format!("unknown argument: {other}\n{}", usage())),
            }
        }
        if let Ok(music) = std::env::var("RICERCAR_LIBRARY") {
            for p in music.split(':') {
                if !p.is_empty() && !cfg.roots.iter().any(|r| r == p) {
                    cfg.roots.push(p.into());
                }
            }
        }
        Ok(cfg)
    }
}

fn usage() -> String {
    "usage: ricercar-daemon [--device NAME] [--name NAME] [--db PATH] [--library DIR]... [--no-mpris] [--no-upnp]"
        .into()
}

fn default_db() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| PathBuf::from(h).join(".local/share"))
                .unwrap_or_else(|_| std::env::temp_dir())
        });
    base.join("ricercar").join("library.db")
}

pub fn print_devices() {
    for d in list_devices() {
        println!(
            "{}  {}{}{}",
            d.name,
            d.description,
            if d.kind.is_bit_perfect() { " [bit-perfect]" } else { "" },
            if d.kind == ricercar_audio::DeviceKind::Virtual {
                " [virtual]"
            } else {
                ""
            },
        );
    }
}

pub fn run(cfg: Config) -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    if let Some(parent) = cfg.db.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let lib = Arc::new(Library::open(&cfg.db)?);
    for root in &cfg.roots {
        let n = ricercar_core::watcher::scan(&lib, root);
        tracing::info!(%n, root = %root.display(), "scanned");
    }
    let _watcher = if cfg.roots.is_empty() {
        None
    } else {
        Some(WatcherHandle::start(lib.clone(), cfg.roots.clone()))
    };

    let ctl = Arc::new(Controller::new(lib.clone(), &cfg.device));
    tracing::info!(device = %cfg.device, "audio engine up");

    let quit = Arc::new(AtomicBool::new(false));

    let _renderer = if cfg.upnp {
        let handle = ricercar_upnp::start_renderer(ctl.clone(), &cfg.name)?;
        let ip = local_ip().unwrap_or_else(|| "127.0.0.1".into());
        tracing::info!(
            "UPnP MediaRenderer \"{}\" at http://{}:{}/device.xml",
            cfg.name,
            ip,
            handle.port
        );
        Some(handle)
    } else {
        None
    };

    if cfg.mpris {
        match ricercar_mpris::serve(ctl.clone()) {
            Ok(conn) => {
                ricercar_mpris::spawn_event_loop(conn, quit.clone());
                tracing::info!("MPRIS: {}", ricercar_mpris::BUS_NAME);
            }
            Err(e) => tracing::warn!("mpris unavailable: {e}"),
        }
    }

    let q = quit.clone();
    let _ = ctrlc::set_handler(move || q.store(true, Ordering::SeqCst));
    while !quit.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    tracing::info!("bye");
    Ok(())
}

fn local_ip() -> Option<String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect(("8.8.8.8", 80)).ok()?;
    sock.local_addr().ok().map(|a| a.ip().to_string())
}
