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
        version: None,
        host: None,
        settings: Default::default(),
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
    let (_, home) = h.browse_root_full("demo").unwrap();
    assert_eq!(home.unwrap()[0].title, "New releases");
    let playlists = h
        .library_all("demo", ricercar_core::plugin::LibraryList::Playlists, 100)
        .unwrap();
    assert_eq!(playlists[0].kind, ItemKind::Playlist);
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
    use ricercar_core::plugin::LibraryList;
    assert!(h.status("demo").unwrap().caps.library);
    assert_eq!(
        h.library_all("demo", LibraryList::Albums, 100)
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        h.library_all("demo", LibraryList::Tracks, 100)
            .unwrap()
            .len(),
        6
    );
    assert_eq!(
        h.library_all("demo", LibraryList::Tracks, 4).unwrap().len(),
        4
    );
    let artists = h.library_all("demo", LibraryList::Artists, 100).unwrap();
    assert_eq!(artists[0].kind, ItemKind::Artist);
    assert_eq!(
        h.browse_list("demo", &artists[0].reference, 0, 50)
            .unwrap()
            .0
            .len(),
        2
    );
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
    // The queue ran out on the last track: that is an end, not a stop.
    assert!(
        rig.wait_log(r#"playback.reason track/3 "ended""#),
        "{}",
        rig.log()
    );
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
    // Play the first restored item. The saved position is not asserted:
    // the first controller (no plugin host) starts on enqueue and may have
    // skipped the unresolvable first track before it paused.
    let first = ctl.lock().queue[0].id;
    ctl.play_id(first);
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

fn with_settings(args: &[&str], settings: &[(&str, toml::Value)]) -> PluginConfig {
    let mut c = config(args, true);
    c.settings = settings
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    c
}

fn quality_options(rig: &Rig) -> usize {
    use ricercar_core::plugin::settings::Kind;
    let st = rig.host.status("demo").unwrap();
    match st
        .settings
        .iter()
        .find(|s| s.key == "quality")
        .map(|s| &s.kind)
    {
        Some(Kind::Choice { options }) => options.len(),
        _ => 0,
    }
}

#[test]
fn settings_are_declared_and_sent_at_start() {
    let dir = tempfile::tempdir().unwrap();
    let host = PluginHost::new("t", dir.path().join("data"), dir.path().join("cache"));
    host.reconcile(&[with_settings(
        &[],
        &[
            ("page_size", toml::Value::Integer(20)),
            ("old_key", toml::Value::Boolean(true)),
        ],
    )]);
    let rig = Rig { dir, host };
    assert!(wait(10, || rig.state() == RunState::Running));
    // Every stored value goes in `initialize`, declared or not.
    assert!(
        rig.wait_log(r#"settings {"old_key":true,"page_size":20}"#),
        "{}",
        rig.log()
    );
    assert!(rig.wait_log("greeting Hello from Demo Music"));
    let st = rig.host.status("demo").unwrap();
    let keys: Vec<&str> = st.settings.iter().map(|s| s.key.as_str()).collect();
    assert_eq!(
        keys,
        ["report_playback", "quality", "page_size", "greeting"]
    );
    assert_eq!(st.values["page_size"], serde_json::json!(20));
    assert_eq!(st.values["quality"], serde_json::json!("lossless"));
    assert!(!st.values.contains_key("old_key"));
    assert_eq!(quality_options(&rig), 2);
    // Signed in, the plugin declares its settings again.
    rig.sign_in();
    assert!(wait(5, || quality_options(&rig) == 3));
    rig.host.auth_sign_out("demo").unwrap();
    assert!(wait(5, || quality_options(&rig) == 2));
}

#[test]
fn settings_apply_live_or_restart_the_plugin() {
    let rig = Rig::new(&["--no-auth"]);
    assert!(wait(5, || quality_options(&rig) == 3));
    let off = [("report_playback", toml::Value::Boolean(false))];
    rig.host.reconcile(&[with_settings(&["--no-auth"], &off)]);
    assert!(
        rig.wait_log(
            r#"settings.changed {"greeting":"Hello from Demo Music","page_size":100,"quality":"lossless","report_playback":false}"#
        ),
        "{}",
        rig.log()
    );
    assert_eq!(rig.log().matches("initialize").count(), 1, "no restart");
    assert_eq!(rig.state(), RunState::Running);
    // The same config again changes nothing.
    rig.host.reconcile(&[with_settings(&["--no-auth"], &off)]);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(rig.log().matches("settings.changed").count(), 1);

    // Reporting is off: the plugin ignores playback reports.
    let ctl = rig.controller();
    ctl.play_tracks(rig.tracks("album/1")[..1].to_vec(), 0, PlayContext::None);
    assert!(rig.wait_log("track.resolve track/1 play"), "{}", rig.log());
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!rig.log().contains("playback."), "{}", rig.log());
    ctl.stop();

    // Back to the defaults.
    rig.host.reconcile(&[config(&["--no-auth"], true)]);
    assert!(wait(5, || rig.log().contains(r#""report_playback":true}"#)));

    // `greeting` asks for a restart.
    let hi = [("greeting", toml::Value::String("Bonjour".into()))];
    rig.host.reconcile(&[with_settings(&["--no-auth"], &hi)]);
    assert!(rig.wait_log("greeting Bonjour"), "{}", rig.log());
    assert_eq!(rig.log().matches("initialize").count(), 2);
    assert!(wait(10, || rig.state() == RunState::Running));
    assert_eq!(
        rig.host.status("demo").unwrap().values["greeting"],
        serde_json::json!("Bonjour")
    );
}

#[test]
fn lyrics_from_the_plugin() {
    let rig = Rig::new(&["--no-auth"]);
    let h = &rig.host;
    assert!(h.status("demo").unwrap().caps.lyrics);
    let l = h.lyrics_get("demo", "track/1").unwrap();
    let synced = l.synced.unwrap();
    assert_eq!(synced.len(), 3);
    assert!(synced.windows(2).all(|w| w[0].time_ms <= w[1].time_ms));
    assert!(l.plain.unwrap().starts_with("First light"));
    let l = h.lyrics_get("demo", "track/2").unwrap();
    assert!(l.synced.is_none() && l.plain.is_some());
    assert!(h.lyrics_get("demo", "track/3").unwrap().instrumental);
    assert_eq!(h.lyrics_get("demo", "track/4"), Err(PluginError::NotFound));
}

#[test]
fn contextual_refs_actions_and_favorites() {
    let rig = Rig::new(&["--no-auth"]);
    let h = &rig.host;
    let t = h.item_get("demo", "track/4").unwrap();
    assert_eq!(t.album_ref.as_deref(), Some("album/2"));
    assert_eq!(t.artist_ref.as_deref(), Some("artist/1"));
    assert_eq!(t.label_ref.as_deref(), Some("label/1"));
    assert_eq!(t.favorite, Some(false));
    let info = t.to_track_info("demo");
    assert_eq!(
        info.plugin_album_ref(),
        Some(("demo".to_string(), "album/2".to_string()))
    );
    assert_eq!(
        info.plugin_artist_ref(),
        Some(("demo".to_string(), "artist/1".to_string()))
    );
    // The label opens like any browsable ref.
    assert_eq!(h.browse_list("demo", "label/1", 0, 50).unwrap().0.len(), 2);

    h.favorites_set("demo", "track/4", true).unwrap();
    assert_eq!(h.item_get("demo", "track/4").unwrap().favorite, Some(true));

    let album = h.item_get("demo", "album/1").unwrap();
    use ricercar_core::plugin::ActionKind;
    let kinds: Vec<(&str, ActionKind)> = album
        .actions
        .iter()
        .map(|a| (a.id.as_str(), a.kind))
        .collect();
    assert_eq!(
        kinds,
        [("radio", ActionKind::Play), ("similar", ActionKind::Browse)]
    );
    // "play": the tracks under the action's ref, ready to queue.
    let tracks = h
        .playable_tracks("demo", &album.actions[0].reference, 500)
        .unwrap();
    assert_eq!(
        tracks.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(),
        ["Blue Hour", "Lanterns"]
    );
    assert!(tracks[0].uri.starts_with("plugin://demo/"));
    // "browse": a page of items.
    let (similar, _, _) = h
        .browse_list("demo", &album.actions[1].reference, 0, 50)
        .unwrap();
    assert_eq!(similar[0].title, "Night Studies");
}

#[test]
fn details_of_artists_and_albums() {
    let rig = Rig::new(&["--no-auth"]);
    let d = rig.host.item_details("demo", "artist/1").unwrap();
    let bio = d.biography.unwrap();
    assert!(bio.text.contains("Demo Ensemble"));
    assert_eq!(bio.source.as_deref(), Some("Demo Music"));
    assert_eq!(d.related.len(), 2);
    assert_eq!(d.related[0].items.len(), 2);
    assert_eq!(d.facts[0].label, "Formed");
    let d = rig.host.item_details("demo", "album/1").unwrap();
    assert!(d.biography.is_none());
    assert_eq!(d.related[0].title, "Similar albums");
    assert_eq!(
        rig.host.item_details("demo", "nope"),
        Err(PluginError::NotFound)
    );
}

#[test]
fn playlist_editing() {
    let rig = Rig::new(&["--no-auth"]);
    let h = &rig.host;
    use ricercar_core::plugin::LibraryList;
    let lists = h.library_all("demo", LibraryList::Playlists, 100).unwrap();
    assert_eq!(
        lists.iter().map(|p| p.editable).collect::<Vec<_>>(),
        [true, false]
    );
    // Not the user's: refused without asking the plugin.
    assert!(matches!(
        h.playlist_rename("demo", "playlist/2", "Mine now"),
        Err(PluginError::Other { code: -32602, .. })
    ));
    assert!(!rig.log().contains("playlists.rename"));

    let created = h
        .playlist_create("demo", "  Road trip ", None, Some(false))
        .unwrap();
    assert_eq!(created.value.title, "Road trip");
    assert!(created.value.editable);
    assert_eq!(created.playlists.as_ref().unwrap().len(), 3);
    let p = created.value.reference.clone();

    let e = h
        .playlist_add(
            "demo",
            &p,
            &[
                "plugin://demo/track%2F1".into(),
                "plugin://demo/track%2F4".into(),
            ],
        )
        .unwrap();
    let listed = e.playlists.unwrap();
    assert_eq!(
        listed
            .iter()
            .find(|x| x.reference == p)
            .unwrap()
            .track_count,
        Some(2)
    );
    // Tracks of another plugin, or local files, never go in.
    for bad in ["plugin://other/track%2F1", "file:///music/a.flac"] {
        assert!(matches!(
            h.playlist_add("demo", &p, &[bad.to_string()]),
            Err(PluginError::Other { code: -32602, .. })
        ));
    }

    let (tracks, _, _) = h.browse_list("demo", &p, 0, 50).unwrap();
    let entries: Vec<String> = tracks.iter().map(|t| t.entry_id.clone().unwrap()).collect();
    assert!(h.playlist_move_supported("demo"));
    h.playlist_move("demo", &p, &entries[1], 0).unwrap();
    let (tracks, _, _) = h.browse_list("demo", &p, 0, 50).unwrap();
    assert_eq!(tracks[0].title, "Blue Hour");
    h.playlist_remove("demo", &p, &entries[1..]).unwrap();
    assert_eq!(h.browse_list("demo", &p, 0, 50).unwrap().0.len(), 1);
    h.playlist_rename("demo", &p, "Road trip II").unwrap();
    // A failed write is not sent again: it may have happened on the service.
    assert_eq!(
        h.playlist_rename("demo", &p, "offline").err(),
        Some(PluginError::Network)
    );
    assert_eq!(rig.count(&format!("playlists.rename {p} offline")), 1);
    let gone = h.playlist_delete("demo", &p).unwrap();
    assert_eq!(gone.playlists.unwrap().len(), 2);
    assert!(rig.wait_log("playlists.delete playlist/3"));
}

#[test]
fn playlist_move_can_be_missing() {
    let rig = Rig::new(&["--no-auth", "--no-move"]);
    let h = &rig.host;
    let (tracks, _, _) = h.browse_list("demo", "playlist/1", 0, 50).unwrap();
    let entry = tracks[0].entry_id.clone().unwrap();
    assert!(h.playlist_move_supported("demo"));
    assert!(matches!(
        h.playlist_move("demo", "playlist/1", &entry, 2),
        Err(PluginError::Other { code: -32601, .. })
    ));
    assert!(!h.playlist_move_supported("demo"));
}

#[test]
fn resolve_carries_the_delivery() {
    use ricercar_core::plugin::{Delivery, Purpose};
    let rig = Rig::new(&["--no-auth"]);
    let direct = rig
        .host
        .resolve("plugin://demo/track%2F1", Purpose::Play)
        .unwrap();
    assert_eq!(direct.delivery, Delivery::Direct);
    let relayed = rig
        .host
        .resolve("plugin://demo/track%2F4", Purpose::Play)
        .unwrap();
    assert_eq!(relayed.delivery, Delivery::Proxied);
    let ctl = rig.controller();
    ctl.play_tracks(rig.tracks("album/2")[..1].to_vec(), 0, PlayContext::None);
    assert!(rig.wait_log("playback.started track/4"), "{}", rig.log());
    assert!(wait(5, || ctl.lock().delivery == Some(Delivery::Proxied)));
}

fn continuous_rig(args: &[&str], on: bool) -> (Rig, Arc<Controller>) {
    let rig = Rig::new(args);
    let ctl = rig.controller();
    ctl.set_continuous(on);
    (rig, ctl)
}

#[test]
fn queue_end_stops_without_continuous_playback() {
    let (rig, ctl) = continuous_rig(&["--no-auth"], false);
    ctl.play_tracks(rig.tracks("album/1")[2..].to_vec(), 0, PlayContext::None);
    assert!(rig.wait_log("playback.ended track/3"), "{}", rig.log());
    std::thread::sleep(Duration::from_millis(500));
    assert!(!rig.log().contains("radio.next"));
    assert_eq!(ctl.lock().queue.len(), 1);
}

#[test]
fn continuous_playback_appends_and_plays() {
    let (rig, ctl) = continuous_rig(&["--no-auth"], true);
    ctl.play_tracks(rig.tracks("album/1")[2..].to_vec(), 0, PlayContext::None);
    assert!(
        rig.wait_log("radio.next track/3 exclude=track/3 limit=20"),
        "{}",
        rig.log()
    );
    // Track 1 comes after track 3, played without a gap.
    assert!(rig.wait_log("playback.started track/1"), "{}", rig.log());
    let st = ctl.lock();
    assert_eq!(st.queue.len(), 5);
    assert!(
        st.queue
            .iter()
            .all(|q| q.info.uri.starts_with("plugin://demo/"))
    );
}

#[test]
fn continuous_playback_stops_on_error() {
    let (rig, ctl) = continuous_rig(&["--no-auth", "--radio-fail"], true);
    ctl.play_tracks(rig.tracks("album/1")[2..].to_vec(), 0, PlayContext::None);
    assert!(rig.wait_log("playback.ended track/3"), "{}", rig.log());
    assert_eq!(rig.log().matches("radio.next").count(), 1);
    assert_eq!(ctl.lock().queue.len(), 1);
    assert!(!ctl.lock().radio_pending);
}

#[test]
fn continuous_playback_never_follows_a_local_track() {
    let (rig, ctl) = continuous_rig(&["--no-auth"], true);
    let file = rig.dir.path().join("local.flac");
    ricercar_core::synth::write_flac(
        &file,
        &ricercar_core::meta::TagInfo {
            title: Some("Local".into()),
            sample_rate: Some(44_100),
            bits: Some(16),
            ..Default::default()
        },
    )
    .unwrap();
    let mut list = rig.tracks("album/1")[..1].to_vec();
    list.push(TrackInfo::from_uri(&ricercar_core::meta::file_uri(&file)));
    let events = ctl.subscribe();
    ctl.play_tracks(list, 0, PlayContext::None);
    assert!(rig.wait_log("playback.ended track/1"), "{}", rig.log());
    // The local track plays, then the queue ends.
    assert!(wait(10, || events.try_iter().any(|e| matches!(
        e,
        ricercar_core::CtlEvent::StatusChanged(ricercar_audio::TransportStatus::Stopped)
    )) && ctl.lock().current == Some(1)));
    std::thread::sleep(Duration::from_millis(300));
    assert!(!rig.log().contains("radio.next"), "{}", rig.log());
    assert_eq!(ctl.lock().queue.len(), 2);
}

#[test]
fn radio_from_an_item() {
    let rig = Rig::new(&["--no-auth"]);
    let ctl = rig.controller();
    let lead = rig.tracks("album/2")[..1].to_vec().pop();
    let n = ctl.start_radio("plugin://demo/track%2F4", lead).unwrap();
    assert_eq!(n, 5);
    assert!(
        rig.wait_log("radio.next track/4 exclude=track/4 limit=20"),
        "{}",
        rig.log()
    );
    assert_eq!(ctl.lock().queue[0].info.title, "Blue Hour");
    assert!(rig.wait_log("playback.started track/4"));
    // An album seeds too.
    assert_eq!(ctl.start_radio("plugin://demo/album%2F1", None).unwrap(), 2);
}

#[test]
fn interface_language_reaches_plugins() {
    let rig = Rig::new(&["--no-auth"]);
    rig.host.set_language("fr");
    assert!(rig.wait_log(r#"locale.changed "fr""#), "{}", rig.log());
    // Same language again: nothing sent.
    rig.host.set_language("fr");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(rig.log().matches("locale.changed").count(), 1);
    // A restart starts in the chosen language.
    rig.host.reconcile(&[]);
    rig.host.reconcile(&[config(&["--no-auth"], true)]);
    assert!(rig.wait_log(r#"locale "fr""#), "{}", rig.log());
}
