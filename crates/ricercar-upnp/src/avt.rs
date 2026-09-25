//! UPnP AV renderer services: AVTransport, RenderingControl,
//! ConnectionManager. AVTransport is a view of the Controller queue, so it
//! reports the same state when the queue is driven through OpenHome.

use ricercar_audio::player::TransportStatus;
use ricercar_core::{Repeat, TrackInfo};

use crate::notify::Vars;
use crate::soap::{Args, Reply};
use crate::{Renderer, Snap, Svc, Wake, didl, media};

/// `H:MM:SS` for AVTransport time values.
pub fn fmt_time(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

pub fn transport_state(s: &Snap) -> &'static str {
    if s.ids.is_empty() {
        return "NO_MEDIA_PRESENT";
    }
    match s.status {
        TransportStatus::Playing => "PLAYING",
        TransportStatus::Paused => "PAUSED_PLAYBACK",
        TransportStatus::Stopped => "STOPPED",
    }
}

pub fn play_mode(shuffle: bool, repeat: Repeat) -> &'static str {
    if shuffle {
        return "SHUFFLE";
    }
    match repeat {
        Repeat::Off => "NORMAL",
        Repeat::All => "REPEAT_ALL",
        Repeat::One => "REPEAT_ONE",
    }
}

/// `SetPlayMode` → (shuffle, repeat).
pub fn parse_play_mode(m: &str) -> Option<(bool, Repeat)> {
    Some(match m.trim() {
        "NORMAL" | "DIRECT_1" => (false, Repeat::Off),
        "REPEAT_ALL" => (false, Repeat::All),
        "REPEAT_ONE" => (false, Repeat::One),
        "SHUFFLE" | "RANDOM" | "SHUFFLE_NOREPEAT" => (true, Repeat::Off),
        "SHUFFLE_REPEAT_ALL" => (true, Repeat::All),
        _ => return None,
    })
}

fn transport_actions(s: &Snap) -> &'static str {
    match transport_state(s) {
        "PLAYING" => "Pause,Stop,Seek,Next,Previous",
        "PAUSED_PLAYBACK" => "Play,Stop,Seek,Next,Previous",
        "STOPPED" => "Play,Next,Previous",
        _ => "",
    }
}

pub fn cms_vars(server: bool) -> Vars {
    let (source, sink) = if server {
        (media::source_protocols(), String::new())
    } else {
        (String::new(), media::SINK_PROTOCOLS.to_string())
    };
    vec![
        ("SourceProtocolInfo", source),
        ("SinkProtocolInfo", sink),
        ("CurrentConnectionIDs", "0".into()),
    ]
}

/// Item parsed from control-point metadata, or derived from the URI.
pub fn remote_info(uri: &str, meta: &str) -> TrackInfo {
    let from_uri = TrackInfo::from_uri(uri);
    match didl::parse_didl(uri, meta) {
        Some(mut info) => {
            if info.title.is_empty() {
                info.title = from_uri.title;
            }
            if info.path.is_none() {
                info.path = from_uri.path;
            }
            info
        }
        None => from_uri,
    }
}

impl Renderer {
    pub fn avt_vars(&self, s: &Snap, base: &str) -> Vars {
        let cur_uri = s
            .cur
            .as_ref()
            .map(|q| q.info.uri.clone())
            .unwrap_or_default();
        let cur_meta = s
            .cur
            .as_ref()
            .map(|q| self.item_meta(q, base))
            .unwrap_or_default();
        let next_uri = s
            .next
            .as_ref()
            .map(|q| q.info.uri.clone())
            .unwrap_or_default();
        let next_meta = s
            .next
            .as_ref()
            .map(|q| self.item_meta(q, base))
            .unwrap_or_default();
        vec![
            ("TransportState", transport_state(s).into()),
            ("TransportStatus", "OK".into()),
            ("TransportPlaySpeed", "1".into()),
            ("CurrentPlayMode", play_mode(s.shuffle, s.repeat).into()),
            ("PlaybackStorageMedium", "NETWORK".into()),
            ("PossiblePlaybackStorageMedia", "NETWORK".into()),
            ("NumberOfTracks", s.ids.len().to_string()),
            (
                "CurrentTrack",
                s.current.map(|i| i + 1).unwrap_or(0).to_string(),
            ),
            ("CurrentTrackDuration", fmt_time(s.dur_ms)),
            ("CurrentMediaDuration", fmt_time(s.total_ms)),
            ("CurrentTrackURI", cur_uri.clone()),
            ("CurrentTrackMetaData", cur_meta.clone()),
            ("AVTransportURI", cur_uri),
            ("AVTransportURIMetaData", cur_meta),
            ("NextAVTransportURI", next_uri),
            ("NextAVTransportURIMetaData", next_meta),
            ("CurrentTransportActions", transport_actions(s).into()),
        ]
    }

    pub fn avt(&self, action: &str, args: &Args, base: &str) -> Reply {
        let ns = Svc::Avt.urn();
        let ok = |out: &[(&str, &str)]| Reply::ok(action, ns, out);
        if args.get("InstanceID").trim() != "0" && !args.get("InstanceID").is_empty() {
            return Reply::Err(718, "Invalid InstanceID");
        }
        match action {
            "SetAVTransportURI" => {
                let uri = args.get("CurrentURI").trim().to_string();
                if uri.is_empty() {
                    self.ctl.clear_queue();
                    return ok(&[]);
                }
                let meta = args.get("CurrentURIMetaData");
                self.ctl.set_remote(&uri, Some(remote_info(&uri, meta)));
                let id = self.ctl.lock().queue.first().map(|q| q.id);
                if let Some(id) = id {
                    self.store_meta(id, meta);
                }
                self.wake(Wake::Dirty);
                ok(&[])
            }
            "SetNextAVTransportURI" => {
                let uri = args.get("NextURI").trim().to_string();
                let meta = args.get("NextURIMetaData");
                let info = (!uri.is_empty()).then(|| remote_info(&uri, meta));
                self.ctl.remote_next(&uri, info);
                if !uri.is_empty() {
                    let id = self
                        .ctl
                        .lock()
                        .queue
                        .last()
                        .filter(|q| q.info.uri == uri)
                        .map(|q| q.id);
                    if let Some(id) = id {
                        self.store_meta(id, meta);
                    }
                }
                self.wake(Wake::Dirty);
                ok(&[])
            }
            "Play" => {
                if self.ctl.lock().queue.is_empty() {
                    return Reply::Err(701, "Transition not available");
                }
                self.ctl.play();
                ok(&[])
            }
            "Pause" => {
                self.ctl.pause();
                ok(&[])
            }
            "Stop" => {
                self.ctl.stop();
                ok(&[])
            }
            "Seek" => {
                let unit = args.get("Unit");
                let target = args.get("Target");
                match unit {
                    "ABS_TIME" | "REL_TIME" => {
                        let Some(ms) = didl::parse_duration(target) else {
                            return Reply::Err(711, "Illegal seek target");
                        };
                        self.ctl.seek_ms(ms);
                    }
                    "TRACK_NR" => {
                        let n: usize = match target.trim().parse() {
                            Ok(n) if n >= 1 => n,
                            _ => return Reply::Err(711, "Illegal seek target"),
                        };
                        if n > self.ctl.lock().queue.len() {
                            return Reply::Err(711, "Illegal seek target");
                        }
                        self.ctl.play_index(n - 1);
                    }
                    _ => return Reply::Err(710, "Seek mode not supported"),
                }
                ok(&[])
            }
            "Next" => {
                self.ctl.next();
                ok(&[])
            }
            "Previous" => {
                self.ctl.prev();
                ok(&[])
            }
            "SetPlayMode" => {
                let Some((shuffle, repeat)) = parse_play_mode(args.get("NewPlayMode")) else {
                    return Reply::Err(712, "Play mode not supported");
                };
                self.ctl.set_shuffle(shuffle);
                self.ctl.set_repeat(repeat);
                ok(&[])
            }
            "GetTransportSettings" => {
                let (shuffle, repeat) = {
                    let st = self.ctl.lock();
                    (st.shuffle, st.repeat)
                };
                ok(&[
                    ("PlayMode", play_mode(shuffle, repeat)),
                    ("RecQualityMode", "NOT_IMPLEMENTED"),
                ])
            }
            "GetTransportInfo" => {
                let s = self.snap();
                ok(&[
                    ("CurrentTransportState", transport_state(&s)),
                    ("CurrentTransportStatus", "OK"),
                    ("CurrentSpeed", "1"),
                ])
            }
            "GetPositionInfo" => {
                let s = self.snap();
                let (uri, meta) = s
                    .cur
                    .as_ref()
                    .map(|q| (q.info.uri.clone(), self.item_meta(q, base)))
                    .unwrap_or_default();
                let pos = fmt_time(s.pos_ms);
                ok(&[
                    ("Track", &s.current.map(|i| i + 1).unwrap_or(0).to_string()),
                    ("TrackDuration", &fmt_time(s.dur_ms)),
                    ("TrackMetaData", &meta),
                    ("TrackURI", &uri),
                    ("RelTime", &pos),
                    ("AbsTime", &pos),
                    ("RelCount", "2147483647"),
                    ("AbsCount", "2147483647"),
                ])
            }
            "GetMediaInfo" => {
                let s = self.snap();
                let vars = self.avt_vars(&s, base);
                let get = |k: &str| {
                    vars.iter()
                        .find(|(n, _)| *n == k)
                        .map(|(_, v)| v.as_str())
                        .unwrap_or("")
                };
                ok(&[
                    ("NrTracks", get("NumberOfTracks")),
                    ("MediaDuration", get("CurrentMediaDuration")),
                    ("CurrentURI", get("AVTransportURI")),
                    ("CurrentURIMetaData", get("AVTransportURIMetaData")),
                    ("NextURI", get("NextAVTransportURI")),
                    ("NextURIMetaData", get("NextAVTransportURIMetaData")),
                    (
                        "PlayMedium",
                        if s.ids.is_empty() { "NONE" } else { "NETWORK" },
                    ),
                    ("RecordMedium", "NOT_IMPLEMENTED"),
                    ("WriteStatus", "NOT_IMPLEMENTED"),
                ])
            }
            "GetDeviceCapabilities" => ok(&[
                ("PlayMedia", "NETWORK"),
                ("RecMedia", "NOT_IMPLEMENTED"),
                ("RecQualityModes", "NOT_IMPLEMENTED"),
            ]),
            "GetCurrentTransportActions" => {
                let s = self.snap();
                ok(&[("Actions", transport_actions(&s))])
            }
            _ => Reply::Err(401, "Invalid Action"),
        }
    }

    pub fn rcs(&self, action: &str, args: &Args) -> Reply {
        let ns = Svc::Rcs.urn();
        let ok = |out: &[(&str, &str)]| Reply::ok(action, ns, out);
        match action {
            "GetVolume" => {
                let v = self.ctl.lock().volume;
                ok(&[("CurrentVolume", &v.to_string())])
            }
            "SetVolume" => match args.num::<u32>("DesiredVolume") {
                Some(v) if v <= 100 => {
                    self.ctl.set_volume(v);
                    ok(&[])
                }
                _ => Reply::Err(402, "Invalid Args"),
            },
            "GetMute" => {
                let m = self.ctl.lock().muted;
                ok(&[("CurrentMute", if m { "1" } else { "0" })])
            }
            "SetMute" => match args.bool("DesiredMute") {
                Some(m) => {
                    self.ctl.set_muted(m);
                    ok(&[])
                }
                None => Reply::Err(402, "Invalid Args"),
            },
            "ListPresets" => ok(&[("CurrentPresetNameList", "FactoryDefaults")]),
            "SelectPreset" => {
                if args.get("PresetName") != "FactoryDefaults" {
                    return Reply::Err(701, "Invalid Name");
                }
                self.ctl.set_muted(false);
                self.ctl.set_volume(100);
                ok(&[])
            }
            _ => Reply::Err(401, "Invalid Action"),
        }
    }

    pub fn cms(&self, action: &str, server: bool) -> Reply {
        let ns = Svc::Cms.urn();
        let ok = |out: &[(&str, &str)]| Reply::ok(action, ns, out);
        match action {
            "GetProtocolInfo" => {
                let vars = cms_vars(server);
                ok(&[("Source", &vars[0].1), ("Sink", &vars[1].1)])
            }
            "GetCurrentConnectionIDs" => ok(&[("ConnectionIDs", "0")]),
            "GetCurrentConnectionInfo" => ok(&[
                ("RcsID", if server { "-1" } else { "0" }),
                ("AVTransportID", if server { "-1" } else { "0" }),
                ("ProtocolInfo", ""),
                ("PeerConnectionManager", ""),
                ("PeerConnectionID", "-1"),
                ("Direction", if server { "Output" } else { "Input" }),
                ("Status", "OK"),
            ]),
            _ => Reply::Err(401, "Invalid Action"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn play_modes_roundtrip() {
        for m in ["NORMAL", "REPEAT_ALL", "REPEAT_ONE", "SHUFFLE"] {
            let (s, r) = parse_play_mode(m).unwrap();
            assert_eq!(play_mode(s, r), m);
        }
        assert!(parse_play_mode("INTRO").is_none());
        assert_eq!(fmt_time(3_725_000), "1:02:05");
    }
}
