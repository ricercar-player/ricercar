//! OpenHome renderer services (Product, Playlist, Info, Time, Volume).
//!
//! The OpenHome Playlist is the Controller queue: track ids are queue item
//! ids (ui4). Metadata inserted by control points is kept verbatim so
//! `Read`/`ReadList` hand it back unchanged.

use std::sync::atomic::Ordering;

use ricercar_audio::player::TransportStatus;
use ricercar_core::Repeat;

use crate::notify::Vars;
use crate::soap::{Args, Reply};
use crate::{Renderer, Snap, Svc, Wake, avt, media, mime_for, oh_id, xml};

/// Playlist capacity advertised to control points.
pub const TRACKS_MAX: usize = 16_384;
const SOURCE_NAME: &str = "Playlist";
const ATTRIBUTES: &str = "Info Time Volume";
const URL: &str = "https://github.com/ricercar-player/ricercar";

fn b(v: bool) -> String {
    if v { "true" } else { "false" }.to_string()
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.len();
        let v = (chunk[0] as u32) << 16
            | (chunk.get(1).copied().unwrap_or(0) as u32) << 8
            | chunk.get(2).copied().unwrap_or(0) as u32;
        out.push(T[(v >> 18) as usize & 63] as char);
        out.push(T[(v >> 12) as usize & 63] as char);
        out.push(if n > 1 {
            T[(v >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if n > 2 {
            T[v as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// OpenHome `IdArray`: big-endian ui4 ids, base64.
pub fn id_array(ids: &[u64]) -> String {
    let bytes: Vec<u8> = ids
        .iter()
        .filter_map(|id| u32::try_from(*id).ok())
        .flat_map(|id| id.to_be_bytes())
        .collect();
    base64(&bytes)
}

fn token(s: &Snap) -> u32 {
    s.queue_rev as u32
}

fn transport_state(s: &Snap) -> &'static str {
    match s.status {
        TransportStatus::Playing => "Playing",
        TransportStatus::Paused => "Paused",
        TransportStatus::Stopped => "Stopped",
    }
}

fn source_xml() -> String {
    format!(
        "<SourceList><Source><SystemName>{SOURCE_NAME}</SystemName><Type>Playlist</Type><Name>{SOURCE_NAME}</Name><Visible>true</Visible></Source></SourceList>"
    )
}

fn is_lossless(codec: &str) -> bool {
    matches!(
        codec.to_ascii_uppercase().as_str(),
        "FLAC" | "ALAC" | "WAV" | "AIFF" | "PCM" | "APE" | "WAVPACK"
    )
}

/// Info `Details` of the current item.
pub fn details(r: &Renderer, s: &Snap) -> Vars {
    let info = s.cur.as_ref().map(|q| &q.info);
    let rate = s
        .format
        .map(|f| f.sample_rate)
        .or(info.and_then(|i| i.sample_rate))
        .unwrap_or(0);
    let bits = s
        .format
        .map(|f| f.bits as u32)
        .or(info.and_then(|i| i.bits.map(u32::from)))
        .unwrap_or(0);
    let channels = s.format.map(|f| f.channels as u32).unwrap_or(2);
    let codec = info
        .map(|i| {
            i.codec.clone().unwrap_or_else(|| {
                crate::didl::codec_from_mime(mime_for(i))
                    .unwrap_or("")
                    .to_string()
            })
        })
        .unwrap_or_default();
    let codec = if codec == "AAC/ALAC" {
        if bits > 0 { "ALAC" } else { "AAC" }.to_string()
    } else {
        codec
    };
    let lossless = is_lossless(&codec);
    let bitrate = info
        .and_then(|i| i.path.as_deref())
        .and_then(|p| r.ctl.lib.track(p))
        .and_then(|t| t.bitrate)
        .map(|kbps| kbps * 1000)
        .unwrap_or(if lossless { rate * bits * channels } else { 0 });
    vec![
        ("Duration", (s.dur_ms / 1000).to_string()),
        ("BitRate", bitrate.to_string()),
        ("BitDepth", bits.to_string()),
        ("SampleRate", rate.to_string()),
        ("Lossless", b(lossless && info.is_some())),
        ("CodecName", codec),
    ]
}

impl Renderer {
    pub fn oh_product_vars(&self) -> Vars {
        vec![
            ("ManufacturerName", "Ricercar".into()),
            ("ManufacturerInfo", "".into()),
            ("ManufacturerUrl", URL.into()),
            ("ManufacturerImageUri", "".into()),
            ("ModelName", "Ricercar".into()),
            ("ModelInfo", "Bit-perfect network player".into()),
            ("ModelUrl", URL.into()),
            ("ModelImageUri", "".into()),
            ("ProductRoom", self.name.clone()),
            ("ProductName", self.name.clone()),
            ("ProductInfo", "".into()),
            ("ProductUrl", "".into()),
            ("ProductImageUri", "".into()),
            ("Standby", b(self.standby.load(Ordering::Relaxed))),
            ("SourceIndex", "0".into()),
            ("SourceCount", "1".into()),
            ("SourceXml", source_xml()),
            ("Attributes", ATTRIBUTES.into()),
        ]
    }

    pub fn oh_playlist_vars(&self, s: &Snap) -> Vars {
        vec![
            ("TransportState", transport_state(s).into()),
            ("Repeat", b(s.repeat != Repeat::Off)),
            ("Shuffle", b(s.shuffle)),
            (
                "Id",
                s.cur.as_ref().map(|q| oh_id(q.id)).unwrap_or(0).to_string(),
            ),
            ("IdArray", id_array(&s.ids)),
            ("TracksMax", TRACKS_MAX.to_string()),
            ("ProtocolInfo", media::SINK_PROTOCOLS.into()),
        ]
    }

    pub fn oh_info_vars(&self, s: &Snap, base: &str) -> Vars {
        let (t, d, m) = {
            let c = self.counters.lock().unwrap();
            (c.track, c.details, c.metatext)
        };
        let mut v = vec![
            ("TrackCount", t.to_string()),
            ("DetailsCount", d.to_string()),
            ("MetatextCount", m.to_string()),
            (
                "Uri",
                s.cur
                    .as_ref()
                    .map(|q| q.info.uri.clone())
                    .unwrap_or_default(),
            ),
            (
                "Metadata",
                s.cur
                    .as_ref()
                    .map(|q| self.item_meta(q, base))
                    .unwrap_or_default(),
            ),
        ];
        v.extend(details(self, s));
        v.push(("Metatext", s.stream_title.clone().unwrap_or_default()));
        v
    }

    pub fn oh_time_vars(&self, s: &Snap) -> Vars {
        let t = self.counters.lock().unwrap().track;
        vec![
            ("TrackCount", t.to_string()),
            ("Duration", (s.dur_ms / 1000).to_string()),
            ("Seconds", (s.pos_ms / 1000).to_string()),
        ]
    }

    pub fn oh_volume_vars(&self, s: &Snap) -> Vars {
        vec![
            ("Volume", s.volume.to_string()),
            ("Mute", b(s.muted)),
            ("Balance", "0".into()),
            ("Fade", "0".into()),
            ("VolumeLimit", "100".into()),
            ("VolumeMax", "100".into()),
            ("VolumeUnity", "100".into()),
            ("VolumeSteps", "100".into()),
            ("VolumeMilliDbPerStep", "500".into()),
            ("BalanceMax", "0".into()),
            ("FadeMax", "0".into()),
        ]
    }

    pub fn oh_product(&self, action: &str, args: &Args) -> Reply {
        let ns = Svc::OhProduct.urn();
        let vars = self.oh_product_vars();
        let get = |k: &str| {
            vars.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        let ok = |out: &[(&str, &str)]| Reply::ok(action, ns, out);
        match action {
            "Manufacturer" | "Model" => {
                let p = if action == "Model" {
                    "Model"
                } else {
                    "Manufacturer"
                };
                ok(&[
                    ("Name", &get(&format!("{p}Name"))),
                    ("Info", &get(&format!("{p}Info"))),
                    ("Url", &get(&format!("{p}Url"))),
                    ("ImageUri", &get(&format!("{p}ImageUri"))),
                ])
            }
            "Product" => ok(&[
                ("Room", &get("ProductRoom")),
                ("Name", &get("ProductName")),
                ("Info", &get("ProductInfo")),
                ("Url", &get("ProductUrl")),
                ("ImageUri", &get("ProductImageUri")),
            ]),
            "Standby" => ok(&[("Value", &get("Standby"))]),
            "SetStandby" => {
                let Some(on) = args.bool("Value") else {
                    return Reply::Err(402, "Invalid Args");
                };
                if on {
                    self.ctl.stop();
                }
                self.standby.store(on, Ordering::Relaxed);
                self.wake(Wake::Dirty);
                ok(&[])
            }
            "SourceCount" => ok(&[("Value", "1")]),
            "SourceXml" => ok(&[("Value", &source_xml())]),
            "SourceIndex" => ok(&[("Value", "0")]),
            "SetSourceIndex" => match args.num::<u32>("Value") {
                Some(0) => ok(&[]),
                _ => Reply::Err(800, "Source index out of range"),
            },
            "SetSourceIndexByName" => {
                if args.get("Value") == SOURCE_NAME {
                    ok(&[])
                } else {
                    Reply::Err(800, "Source not found")
                }
            }
            "Source" => match args.num::<u32>("Index") {
                Some(0) => ok(&[
                    ("SystemName", SOURCE_NAME),
                    ("Type", "Playlist"),
                    ("Name", SOURCE_NAME),
                    ("Visible", "true"),
                ]),
                _ => Reply::Err(800, "Source index out of range"),
            },
            "Attributes" => ok(&[("Value", ATTRIBUTES)]),
            "SourceXmlChangeCount" => ok(&[("Value", "0")]),
            _ => Reply::Err(401, "Invalid Action"),
        }
    }

    pub fn oh_playlist(&self, action: &str, args: &Args, base: &str) -> Reply {
        let ns = Svc::OhPlaylist.urn();
        let ok = |out: &[(&str, &str)]| Reply::ok(action, ns, out);
        let find = |id: u32| {
            self.ctl
                .lock()
                .queue
                .iter()
                .find(|q| q.id == id as u64)
                .cloned()
        };
        match action {
            "Play" => {
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
            "Next" => {
                self.ctl.next();
                ok(&[])
            }
            "Previous" => {
                self.ctl.prev();
                ok(&[])
            }
            "SetRepeat" => match args.bool("Value") {
                Some(on) => {
                    self.ctl
                        .set_repeat(if on { Repeat::All } else { Repeat::Off });
                    ok(&[])
                }
                None => Reply::Err(402, "Invalid Args"),
            },
            "Repeat" => {
                let r = self.ctl.lock().repeat;
                ok(&[("Value", &b(r != Repeat::Off))])
            }
            "SetShuffle" => match args.bool("Value") {
                Some(on) => {
                    self.ctl.set_shuffle(on);
                    ok(&[])
                }
                None => Reply::Err(402, "Invalid Args"),
            },
            "Shuffle" => {
                let s = self.ctl.lock().shuffle;
                ok(&[("Value", &b(s))])
            }
            "SeekSecondAbsolute" => match args.num::<u64>("Value") {
                Some(v) => {
                    self.ctl.seek_ms(v.saturating_mul(1000));
                    ok(&[])
                }
                None => Reply::Err(402, "Invalid Args"),
            },
            "SeekSecondRelative" => match args.num::<i64>("Value") {
                Some(v) => {
                    self.ctl.seek_relative(v.saturating_mul(1000));
                    ok(&[])
                }
                None => Reply::Err(402, "Invalid Args"),
            },
            "SeekId" => match args.num::<u32>("Value").and_then(find) {
                Some(q) => {
                    self.ctl.play_id(q.id);
                    ok(&[])
                }
                None => Reply::Err(800, "Id not found"),
            },
            "SeekIndex" => {
                let len = self.ctl.lock().queue.len();
                match args.num::<usize>("Value") {
                    Some(i) if i < len => {
                        self.ctl.play_index(i);
                        ok(&[])
                    }
                    _ => Reply::Err(800, "Index out of range"),
                }
            }
            "TransportState" => ok(&[("Value", transport_state(&self.snap()))]),
            "Id" => {
                let id = self.ctl.lock().current_item().map(|q| oh_id(q.id));
                ok(&[("Value", &id.unwrap_or(0).to_string())])
            }
            "Read" => match args.num::<u32>("Id").filter(|i| *i != 0).and_then(find) {
                Some(q) => ok(&[
                    ("Uri", &q.info.uri),
                    ("Metadata", &self.item_meta(&q, base)),
                ]),
                None => Reply::Err(800, "Id not found"),
            },
            "ReadList" => {
                let items: Vec<_> = {
                    let st = self.ctl.lock();
                    args.get("IdList")
                        .split(|c: char| c.is_whitespace() || c == ',')
                        .filter_map(|t| t.parse::<u64>().ok())
                        .filter_map(|id| st.queue.iter().find(|q| q.id == id).cloned())
                        .collect()
                };
                let mut list = String::from("<TrackList>");
                for q in &items {
                    list.push_str(&format!(
                        "<Entry><Id>{}</Id><Uri>{}</Uri><Metadata>{}</Metadata></Entry>",
                        oh_id(q.id),
                        xml::escape(&q.info.uri),
                        xml::escape(&self.item_meta(q, base))
                    ));
                }
                list.push_str("</TrackList>");
                ok(&[("TrackList", &list)])
            }
            "Insert" => {
                let Some(after) = args.num::<u32>("AfterId") else {
                    return Reply::Err(402, "Invalid Args");
                };
                let uri = args.get("Uri").trim().to_string();
                if uri.is_empty() {
                    return Reply::Err(402, "Invalid Args");
                }
                if !self.uri_allowed(&uri) {
                    return Reply::Err(716, "Resource not found");
                }
                if self.ctl.lock().queue.len() >= TRACKS_MAX {
                    return Reply::Err(801, "Playlist full");
                }
                let meta = args.get("Metadata");
                let info = avt::remote_info(&uri, meta);
                let Some(ids) = self.ctl.insert_after(after as u64, vec![info]) else {
                    return Reply::Err(800, "Id not found");
                };
                let id = ids[0];
                let Ok(new_id) = u32::try_from(id) else {
                    self.ctl.remove_ids(&[id]);
                    return Reply::Err(801, "Playlist full");
                };
                self.store_meta(id, meta);
                self.wake(Wake::Dirty);
                ok(&[("NewId", &new_id.to_string())])
            }
            "DeleteId" => match args.num::<u32>("Value").and_then(find) {
                Some(q) => {
                    self.ctl.remove_ids(&[q.id]);
                    ok(&[])
                }
                None => Reply::Err(800, "Id not found"),
            },
            "DeleteAll" => {
                self.ctl.clear_queue();
                ok(&[])
            }
            "TracksMax" => ok(&[("Value", &TRACKS_MAX.to_string())]),
            "IdArray" => {
                let s = self.snap();
                ok(&[
                    ("Token", &token(&s).to_string()),
                    ("Array", &id_array(&s.ids)),
                ])
            }
            "IdArrayChanged" => match args.num::<u32>("Token") {
                Some(t) => ok(&[("Value", &b(t != token(&self.snap())))]),
                None => Reply::Err(402, "Invalid Args"),
            },
            "ProtocolInfo" => ok(&[("Value", media::SINK_PROTOCOLS)]),
            _ => Reply::Err(401, "Invalid Action"),
        }
    }

    pub fn oh_info(&self, action: &str, base: &str) -> Reply {
        let ns = Svc::OhInfo.urn();
        let s = self.snap();
        let (t, d, m) = self.update_counters(&s);
        let ok = |out: &[(&str, &str)]| Reply::ok(action, ns, out);
        match action {
            "Counters" => ok(&[
                ("TrackCount", &t.to_string()),
                ("DetailsCount", &d.to_string()),
                ("MetatextCount", &m.to_string()),
            ]),
            "Track" => {
                let (uri, meta) = s
                    .cur
                    .as_ref()
                    .map(|q| (q.info.uri.clone(), self.item_meta(q, base)))
                    .unwrap_or_default();
                ok(&[("Uri", &uri), ("Metadata", &meta)])
            }
            "Details" => {
                let d = details(self, &s);
                let out: Vec<(&str, &str)> = d.iter().map(|(k, v)| (*k, v.as_str())).collect();
                ok(&out)
            }
            "Metatext" => ok(&[("Value", s.stream_title.as_deref().unwrap_or(""))]),
            _ => Reply::Err(401, "Invalid Action"),
        }
    }

    pub fn oh_time(&self, action: &str) -> Reply {
        match action {
            "Time" => {
                let s = self.snap();
                self.update_counters(&s);
                let v = self.oh_time_vars(&s);
                let out: Vec<(&str, &str)> = v.iter().map(|(k, v)| (*k, v.as_str())).collect();
                Reply::ok(action, Svc::OhTime.urn(), &out)
            }
            _ => Reply::Err(401, "Invalid Action"),
        }
    }

    pub fn oh_volume(&self, action: &str, args: &Args) -> Reply {
        let ns = Svc::OhVolume.urn();
        let ok = |out: &[(&str, &str)]| Reply::ok(action, ns, out);
        let (vol, muted) = {
            let st = self.ctl.lock();
            (st.volume, st.muted)
        };
        match action {
            "Characteristics" => ok(&[
                ("VolumeMax", "100"),
                ("VolumeUnity", "100"),
                ("VolumeSteps", "100"),
                ("VolumeMilliDbPerStep", "500"),
                ("BalanceMax", "0"),
                ("FadeMax", "0"),
            ]),
            "SetVolume" => match args.num::<u32>("Value") {
                Some(v) if v <= 100 => {
                    self.ctl.set_volume(v);
                    ok(&[])
                }
                _ => Reply::Err(801, "Volume out of range"),
            },
            "VolumeInc" => {
                self.ctl.set_volume((vol + 1).min(100));
                ok(&[])
            }
            "VolumeDec" => {
                self.ctl.set_volume(vol.saturating_sub(1));
                ok(&[])
            }
            "Volume" => ok(&[("Value", &vol.to_string())]),
            "SetMute" => match args.bool("Value") {
                Some(m) => {
                    self.ctl.set_muted(m);
                    ok(&[])
                }
                None => Reply::Err(402, "Invalid Args"),
            },
            "Mute" => ok(&[("Value", &b(muted))]),
            "VolumeLimit" => ok(&[("Value", "100")]),
            // Balance and fade are fixed at centre: bit-perfect output.
            "Balance" | "Fade" => ok(&[("Value", "0")]),
            "SetBalance" | "SetFade" => match args.num::<i32>("Value") {
                Some(0) => ok(&[]),
                _ => Reply::Err(801, "Out of range"),
            },
            "BalanceInc" | "BalanceDec" | "FadeInc" | "FadeDec" => Reply::Err(801, "Out of range"),
            _ => Reply::Err(401, "Invalid Action"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(id_array(&[1, 2]), base64(&[0, 0, 0, 1, 0, 0, 0, 2]));
        // ids beyond ui4 are never exposed
        assert_eq!(id_array(&[u64::MAX]), "");
    }
}
