//! Plugin host against the reference plugin (tests/fixtures/demo-plugin),
//! with the `null` sink.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ricercar_core::config::PluginConfig;
use ricercar_core::plugin::{
    AuthState, Item, ItemKind, PluginError, PluginHost, Resolver, RunState,
};
use ricercar_core::{Controller, EnqueueAt, Library, Origin, PlayContext, TrackInfo};

const BIN: &str = env!("CARGO_BIN_EXE_ricercar-demo-plugin");

struct Rig {
    dir: tempfile::TempDir,
    host: PluginHost,
}

impl Rig {
    fn new(args: &[&str]) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let host = PluginHost::new(
            "0.0.0-test",
            dir.path().join("data"),
            dir.path().join("cache"),
        );
        host.reconcile(&[config(args, true)]);
        let rig = Rig { dir, host };
        assert!(
            wait(10, || rig.state() == RunState::Running),
            "plugin did not start: {:?}",
            rig.state()
        );
        rig
    }

    fn state(&self) -> RunState {
        self.host
            .status("demo")
            .map(|s| s.state)
            .unwrap_or(RunState::Disabled)
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("data/plugins/demo/calls.log"))
            .unwrap_or_default()
    }

    fn count(&self, line: &str) -> usize {
        self.log().lines().filter(|l| *l == line).count()
    }

    fn wait_log(&self, line: &str) -> bool {
        wait(10, || self.count(line) > 0)
    }

    fn sign_in(&self) {
        assert_eq!(
            self.host.auth_complete("demo", "DEMO").unwrap().state,
            AuthState::SignedIn
        );
    }

    fn controller(&self) -> Arc<Controller> {
        let ctl = Arc::new(Controller::new(
            Arc::new(Library::in_memory().unwrap()),
            "null",
        ));
        ctl.set_resolver(Arc::new(self.host.clone()));
        self.host.attach(&ctl);
        ctl
    }

    fn tracks(&self, reference: &str) -> Vec<TrackInfo> {
        let (items, _, _) = self.host.browse_list("demo", reference, 0, 50).unwrap();
        items
            .iter()
            .map(|i: &Item| i.to_track_info("demo"))
            .collect()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.host.shutdown();
    }
}

fn config(args: &[&str], enabled: bool) -> PluginConfig {
    PluginConfig {
        id: "demo".into(),
        command: PathBuf::from(BIN),
        args: args.iter().map(|a| a.to_string()).collect(),
        enabled,
    }
}

fn wait(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    f()
}

#[test]
fn handshake_and_status() {
    let rig = Rig::new(&[]);
    let st = rig.host.status("demo").unwrap();
    assert_eq!(
        (st.name.as_str(), st.version.as_str()),
        ("Demo Music", "1.0.0")
    );
    assert!(st.caps.auth && st.caps.browse && st.caps.resolve && st.caps.remote_control);
    assert_eq!(st.auth.unwrap().state, AuthState::SignedOut);
    assert!(!rig.host.status("demo").unwrap().signed_in());
    assert!(rig.log().starts_with("initialize"));
    assert!(Path::new(&rig.dir.path().join("cache/plugins/demo/t1.flac")).exists());
}

#[test]
fn other_protocol_is_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let host = PluginHost::new("t", dir.path().join("d"), dir.path().join("c"));
    host.reconcile(&[config(&["--protocol", "2"], true)]);
    assert!(wait(10, || matches!(
        host.status("demo").unwrap().state,
        RunState::Failed(ref m) if m.contains("protocol 2")
    )));
    host.shutdown();
}

#[test]
fn crash_and_restart() {
    let rig = Rig::new(&[]);
    let _ = rig.host.call("demo", "demo.crash", serde_json::json!({}));
    assert!(wait(5, || matches!(
        rig.state(),
        RunState::Restarting { .. }
    )));
    assert_eq!(
        rig.host.auth_status("demo"),
        Err(PluginError::NotRunning),
        "unavailable while restarting"
    );
    assert!(wait(10, || rig.state() == RunState::Running));
    assert_eq!(rig.log().matches("initialize").count(), 2);
}

#[test]
fn sign_in_browse_search_and_errors() {
    let rig = Rig::new(&[]);
    let h = &rig.host;
    assert_eq!(h.browse_root("demo"), Err(PluginError::AuthRequired));
    let begin = h.auth_begin("demo").unwrap();
    assert!(begin.url.starts_with("http://127.0.0.1:") && begin.expects_input);
    assert_eq!(
        h.auth_complete("demo", "wrong").unwrap().state,
        AuthState::SignedOut
    );
    rig.sign_in();
    assert!(h.status("demo").unwrap().signed_in());

    let root = h.browse_root("demo").unwrap();
    assert_eq!(root.len(), 3);
    let (albums, total, more) = h.browse_list("demo", "albums", 0, 50).unwrap();
    assert_eq!((albums.len(), total, more), (2, Some(2), false));
    assert_eq!(albums[0].kind, ItemKind::Album);
    let (page, _, more) = h.browse_list("demo", "album/1", 0, 2).unwrap();
    assert_eq!((page.len(), more), (2, true));
    assert_eq!(
        h.browse_list("demo", "nope", 0, 50),
        Err(PluginError::NotFound)
    );

    let groups = h.search("demo", "blue", 0, 20).unwrap();
    let tracks = &groups.iter().find(|g| g.0 == ItemKind::Track).unwrap().1;
    assert_eq!(tracks[0].title, "Blue Hour");
    assert_eq!(
        h.search("demo", "ratelimit", 0, 20),
        Err(PluginError::RateLimited { retry_after: 1 })
    );
    // Backing off: the plugin is not even asked.
    assert!(matches!(
        h.search("demo", "blue", 0, 20),
        Err(PluginError::RateLimited { .. })
    ));
    std::thread::sleep(Duration::from_millis(1100));
    assert_eq!(
        h.search("demo", "offline", 0, 20),
        Err(PluginError::Network)
    );

    assert_eq!(h.item_get("demo", "track/4").unwrap().title, "Blue Hour");
    h.favorites_set("demo", "track/4", true).unwrap();
    assert_eq!(
        h.browse_list("demo", "favorites", 0, 50).unwrap().0.len(),
        1
    );

    h.auth_sign_out("demo").unwrap();
    assert_eq!(h.browse_root("demo"), Err(PluginError::AuthRequired));
    assert!(!h.status("demo").unwrap().signed_in());
}

#[test]
fn resolve_then_play_with_gapless_preload() {
    let rig = Rig::new(&[]);
    rig.sign_in();
    let ctl = rig.controller();
    let album = rig.tracks("album/1");
    assert_eq!(album[0].uri, "plugin://demo/track%2F1");
    ctl.play_tracks(album, 0, PlayContext::None);
    assert!(rig.wait_log("playback.started track/3"), "{}", rig.log());
    assert_eq!(rig.count("track.resolve track/1 play"), 1);
    // Successors come from preloads, armed before the previous one ends.
    assert_eq!(rig.count("track.resolve track/2 preload"), 1);
    assert_eq!(rig.count("track.resolve track/2 play"), 0);
    assert!(rig.wait_log("playback.ended track/3"));
    let st = ctl.lock();
    assert_eq!(
        st.current_item().unwrap().info.uri,
        "plugin://demo/track%2F3"
    );
    assert_eq!(st.chain.format.map(|f| f.sample_rate), Some(44_100));
    assert_eq!(st.resolving, None);
}

#[test]
fn expired_preload_is_resolved_again() {
    let rig = Rig::new(&["--expire-preload"]);
    rig.sign_in();
    let ctl = rig.controller();
    ctl.play_tracks(rig.tracks("album/1")[..2].to_vec(), 0, PlayContext::None);
    assert!(rig.wait_log("playback.started track/2"), "{}", rig.log());
    assert_eq!(rig.count("track.resolve track/2 preload"), 2);
}

#[test]
fn refused_url_is_resolved_once_more() {
    let rig = Rig::new(&["--gone-first"]);
    rig.sign_in();
    let ctl = rig.controller();
    let errors = ctl.subscribe();
    ctl.play_tracks(rig.tracks("album/1")[..1].to_vec(), 0, PlayContext::None);
    assert!(rig.wait_log("playback.started track/1"), "{}", rig.log());
    assert_eq!(rig.count("track.resolve track/1 play"), 2);
    // The 410 was handled, not shown.
    assert!(
        !errors
            .try_iter()
            .any(|e| matches!(e, ricercar_core::CtlEvent::Error(_)))
    );
}

#[test]
fn unavailable_track_is_skipped() {
    let rig = Rig::new(&[]);
    rig.sign_in();
    let ctl = rig.controller();
    let events = ctl.subscribe();
    let album2 = rig.tracks("album/2");
    let locked = album2
        .iter()
        .find(|t| t.title == "Locked Track")
        .unwrap()
        .clone();
    let mut list = vec![locked];
    list.extend(rig.tracks("album/1")[..1].to_vec());
    ctl.play_tracks(list, 0, PlayContext::None);
    assert!(rig.wait_log("playback.started track/1"), "{}", rig.log());
    let msg = events
        .try_iter()
        .find_map(|e| match e {
            ricercar_core::CtlEvent::Error(m) => Some(m),
            _ => None,
        })
        .unwrap();
    assert!(
        msg.contains("Locked Track") && msg.contains("Demo Music") && msg.contains("not available")
    );
}

#[test]
fn restored_session_resolves_plugin_items() {
    let rig = Rig::new(&[]);
    rig.sign_in();
    let session = rig.dir.path().join("session.json");
    {
        let ctl = Controller::new(Arc::new(Library::in_memory().unwrap()), "null");
        ctl.enable_session(session.clone(), false);
        ctl.enqueue(rig.tracks("album/2")[..2].to_vec(), EnqueueAt::End);
        ctl.pause();
        ctl.shutdown();
    }
    let text = std::fs::read_to_string(&session).unwrap();
    assert!(text.contains("plugin://demo/track%2F4"));
    assert!(
        !text.contains("/t/t4.flac"),
        "resolved URLs are never saved"
    );

    let ctl = rig.controller();
    ctl.enable_session(session, true);
    assert_eq!(ctl.lock().queue.len(), 2);
    ctl.play();
    assert!(rig.wait_log("playback.started track/4"), "{}", rig.log());
}

#[test]
fn remote_control_and_take_over() {
    let rig = Rig::new(&["--no-auth"]);
    let ctl = rig.controller();
    let (items, _, _) = rig.host.browse_list("demo", "album/1", 0, 50).unwrap();
    let items = serde_json::to_value(&items).unwrap();
    rig.host
        .call(
            "demo",
            "demo.remote",
            serde_json::json!({"method": "player.play", "params": {"items": items, "start": 1}}),
        )
        .unwrap();
    assert!(rig.wait_log("remote-reply null"), "{}", rig.log());
    assert_eq!(ctl.lock().origin, Origin::Plugin("demo".into()));
    assert!(rig.wait_log("playback.started track/2"));
    // The user plays something else: the plugin is told.
    ctl.play_tracks(rig.tracks("album/2")[..1].to_vec(), 0, PlayContext::None);
    assert!(rig.wait_log("player.taken_over"), "{}", rig.log());
    // Unknown methods are refused.
    rig.host
        .call(
            "demo",
            "demo.remote",
            serde_json::json!({"method": "player.fly", "params": {}}),
        )
        .unwrap();
    assert!(wait(5, || rig.log().contains("-32601")));
}

#[test]
fn toggling_applies_live() {
    let rig = Rig::new(&["--no-auth"]);
    rig.host.reconcile(&[config(&["--no-auth"], false)]);
    assert_eq!(rig.state(), RunState::Disabled);
    assert!(
        rig.log().lines().any(|l| l == "shutdown"),
        "asked to shut down"
    );
    assert_eq!(
        rig.host.resolve(
            "plugin://demo/track%2F1",
            ricercar_core::plugin::Purpose::Play
        ),
        Err(PluginError::NotRunning)
    );
    rig.host.reconcile(&[config(&["--no-auth"], true)]);
    assert!(wait(10, || rig.state() == RunState::Running));
    assert!(
        rig.host
            .resolve(
                "plugin://demo/track%2F1",
                ricercar_core::plugin::Purpose::Play
            )
            .unwrap()
            .url
            .starts_with("http://127.0.0.1:")
    );
}
