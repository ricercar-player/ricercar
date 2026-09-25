//! OpenHome renderer services: the Playlist is the Controller queue and
//! AVTransport reports the same state.

mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use common::*;

fn pl(port: u16, action: &str, args: &[(&str, &str)]) -> String {
    soap(port, "ohplaylist", PLAYLIST, action, args)
}

fn insert(port: u16, after: u32, uri: &str, meta: &str) -> u32 {
    let r = pl(
        port,
        "Insert",
        &[
            ("AfterId", &after.to_string()),
            ("Uri", uri),
            ("Metadata", meta),
        ],
    );
    assert!(r.contains("200 OK"), "insert: {r}");
    field(&r, "NewId").parse().unwrap()
}

#[test]
fn device_lists_openhome_services() {
    let r = rig("oh-desc", false);
    let (_, body) = get(r.port(), "/device.xml", "");
    let desc = String::from_utf8(body).unwrap();
    for svc in ["Product", "Playlist", "Info", "Time", "Volume"] {
        assert!(
            desc.contains(&format!("urn:av-openhome-org:service:{svc}:1")),
            "{svc} missing"
        );
    }
    assert!(desc.contains("AVTransport"));
    for (path, needle) in [
        ("/svc/ohplaylist.xml", "IdArrayChanged"),
        ("/svc/ohproduct.xml", "SourceXml"),
        ("/svc/ohinfo.xml", "Metatext"),
        ("/svc/ohtime.xml", "Seconds"),
        ("/svc/ohvolume.xml", "VolumeLimit"),
    ] {
        let (head, body) = get(r.port(), path, "");
        assert!(head.contains("200 OK"), "{path}: {head}");
        let scpd = String::from_utf8(body).unwrap();
        assert!(scpd.contains(needle), "{path} lacks {needle}");
        assert!(scpd.contains("<serviceStateTable>"));
    }
}

#[test]
fn playlist_flow_and_avtransport_view() {
    let r = rig("oh-flow", false);
    let p = r.port();
    let didl1 = r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><item id="x" parentID="y" restricted="1"><dc:title>First &amp; foremost</dc:title><upnp:artist>Tester</upnp:artist><upnp:class>object.item.audioItem.musicTrack</upnp:class><res protocolInfo="http-get:*:audio/flac:*" duration="0:00:02.000">x</res></item></DIDL-Lite>"#;

    let token0: u32 = field(&pl(p, "IdArray", &[]), "Token").parse().unwrap();
    let id1 = insert(p, 0, &fixture("tone_16_441.flac"), didl1);
    let id2 = insert(p, id1, &fixture("tone_24_96.flac"), "");
    let id3 = insert(p, id2, &fixture("tone_16_441.flac"), "");
    assert!(id1 != 0 && id1 != id2 && id2 != id3);

    // Insert does not start playback.
    assert_eq!(field(&pl(p, "TransportState", &[]), "Value"), "Stopped");
    assert_eq!(field(&pl(p, "Id", &[]), "Value"), "0");

    let arr = pl(p, "IdArray", &[]);
    assert_eq!(id_array(&arr), [id1, id2, id3]);
    let token: u32 = field(&arr, "Token").parse().unwrap();
    assert_ne!(token, token0);
    assert_eq!(
        field(
            &pl(p, "IdArrayChanged", &[("Token", &token0.to_string())]),
            "Value"
        ),
        "true"
    );

    // The DIDL a control point inserted is handed back verbatim.
    let read = pl(p, "Read", &[("Id", &id1.to_string())]);
    assert_eq!(field(&read, "Metadata"), didl1);
    assert_eq!(field(&read, "Uri"), fixture("tone_16_441.flac"));
    // Items without DIDL get generated metadata.
    let list = pl(p, "ReadList", &[("IdList", &format!("{id1} {id2} 999999"))]);
    let tl = field(&list, "TrackList");
    assert_eq!(tl.matches("<Entry>").count(), 2, "{tl}");
    assert!(tl.contains(&format!("<Id>{id2}</Id>")));
    assert!(tl.contains("DIDL-Lite"));

    // AVTransport sees the same queue.
    let media = soap(p, "avt", AVT, "GetMediaInfo", &[("InstanceID", "0")]);
    assert_eq!(field(&media, "NrTracks"), "3");

    // Play from the second item; the un-paced file sink runs through 2 → 3.
    let seek = pl(p, "SeekId", &[("Value", &id2.to_string())]);
    assert!(seek.contains("200 OK"), "{seek}");
    let mut saw = Vec::new();
    assert!(
        wait_until(20, || {
            let st = field(&pl(p, "TransportState", &[]), "Value");
            if saw.last() != Some(&st) {
                saw.push(st.clone());
            }
            st == "Stopped" && field(&pl(p, "Id", &[]), "Value") == id3.to_string()
        }),
        "never finished on id3, states {saw:?}"
    );
    assert!(
        saw.iter().all(|s| s == "Playing" || s == "Stopped"),
        "{saw:?}"
    );
    let avt = soap(p, "avt", AVT, "GetTransportInfo", &[("InstanceID", "0")]);
    assert_eq!(field(&avt, "CurrentTransportState"), "STOPPED");
    let pos = soap(p, "avt", AVT, "GetPositionInfo", &[("InstanceID", "0")]);
    assert_eq!(field(&pos, "Track"), "3");

    // Info/Time describe the last item (16 bit / 44.1 kHz FLAC).
    let details = soap(p, "ohinfo", INFO, "Details", &[]);
    assert_eq!(field(&details, "SampleRate"), "44100");
    assert_eq!(field(&details, "BitDepth"), "16");
    assert_eq!(field(&details, "CodecName"), "FLAC");
    assert_eq!(field(&details, "Lossless"), "true");
    assert_eq!(field(&details, "Duration"), "2");
    let counters = soap(p, "ohinfo", INFO, "Counters", &[]);
    assert!(field(&counters, "TrackCount").parse::<u32>().unwrap() >= 1);
    let time = soap(p, "ohtime", TIME, "Time", &[]);
    assert_eq!(field(&time, "Duration"), "2");
    let track = soap(p, "ohinfo", INFO, "Track", &[]);
    assert_eq!(field(&track, "Uri"), fixture("tone_16_441.flac"));

    let bytes = std::fs::read(&r.out).unwrap_or_default();
    assert!(bytes.len() > 1_400_000, "{} bytes", bytes.len());

    // Delete the first item: the array shrinks, its id is gone.
    let del = pl(p, "DeleteId", &[("Value", &id1.to_string())]);
    assert!(del.contains("200 OK"), "{del}");
    let arr = pl(p, "IdArray", &[]);
    assert_eq!(id_array(&arr), [id2, id3]);
    assert_eq!(
        field(
            &pl(p, "IdArrayChanged", &[("Token", &token.to_string())]),
            "Value"
        ),
        "true"
    );
    let gone = pl(p, "Read", &[("Id", &id1.to_string())]);
    assert_eq!(fault_code(&gone), Some(800), "{gone}");
    assert_eq!(
        fault_code(&pl(p, "DeleteId", &[("Value", &id1.to_string())])),
        Some(800)
    );
    assert_eq!(
        fault_code(&pl(p, "SeekId", &[("Value", "4000000")])),
        Some(800)
    );
    // Inserting after an unknown id fails.
    let bad = pl(
        p,
        "Insert",
        &[
            ("AfterId", "4000000"),
            ("Uri", "http://x/y"),
            ("Metadata", ""),
        ],
    );
    assert_eq!(fault_code(&bad), Some(800));

    let media = soap(p, "avt", AVT, "GetMediaInfo", &[("InstanceID", "0")]);
    assert_eq!(field(&media, "NrTracks"), "2");

    pl(p, "DeleteAll", &[]);
    assert!(id_array(&pl(p, "IdArray", &[])).is_empty());
    let avt = soap(p, "avt", AVT, "GetTransportInfo", &[("InstanceID", "0")]);
    assert_eq!(field(&avt, "CurrentTransportState"), "NO_MEDIA_PRESENT");
}

#[test]
fn modes_volume_product() {
    let r = rig("oh-modes", false);
    let p = r.port();
    pl(p, "SetRepeat", &[("Value", "true")]);
    assert_eq!(field(&pl(p, "Repeat", &[]), "Value"), "true");
    let s = soap(
        p,
        "avt",
        AVT,
        "GetTransportSettings",
        &[("InstanceID", "0")],
    );
    assert_eq!(field(&s, "PlayMode"), "REPEAT_ALL");
    pl(p, "SetShuffle", &[("Value", "1")]);
    assert_eq!(field(&pl(p, "Shuffle", &[]), "Value"), "true");
    let s = soap(
        p,
        "avt",
        AVT,
        "GetTransportSettings",
        &[("InstanceID", "0")],
    );
    assert_eq!(field(&s, "PlayMode"), "SHUFFLE");

    let vol = |a: &str, args: &[(&str, &str)]| soap(p, "ohvolume", VOLUME, a, args);
    vol("SetVolume", &[("Value", "40")]);
    vol("VolumeInc", &[]);
    assert_eq!(field(&vol("Volume", &[]), "Value"), "41");
    let rcs = soap(
        p,
        "rcs",
        RCS,
        "GetVolume",
        &[("InstanceID", "0"), ("Channel", "Master")],
    );
    assert_eq!(field(&rcs, "CurrentVolume"), "41");
    vol("SetMute", &[("Value", "true")]);
    assert_eq!(field(&vol("Mute", &[]), "Value"), "true");
    let rcs = soap(
        p,
        "rcs",
        RCS,
        "GetMute",
        &[("InstanceID", "0"), ("Channel", "Master")],
    );
    assert_eq!(field(&rcs, "CurrentMute"), "1");
    assert_eq!(
        fault_code(&vol("SetVolume", &[("Value", "101")])),
        Some(801)
    );
    let ch = vol("Characteristics", &[]);
    assert_eq!(field(&ch, "VolumeMax"), "100");

    let prod = |a: &str, args: &[(&str, &str)]| soap(p, "ohproduct", PRODUCT, a, args);
    let xml = field(&prod("SourceXml", &[]), "Value");
    assert!(xml.contains("<Type>Playlist</Type>"), "{xml}");
    assert_eq!(field(&prod("SourceCount", &[]), "Value"), "1");
    assert_eq!(field(&prod("SourceIndex", &[]), "Value"), "0");
    assert_eq!(field(&prod("Attributes", &[]), "Value"), "Info Time Volume");
    assert_eq!(field(&prod("Standby", &[]), "Value"), "false");
    assert_eq!(field(&prod("Product", &[]), "Room"), "ricercar-test");
    assert!(prod("SetSourceIndex", &[("Value", "0")]).contains("200 OK"));
    assert_eq!(
        fault_code(&prod("SetSourceIndex", &[("Value", "1")])),
        Some(800)
    );
    assert_eq!(field(&pl(p, "TracksMax", &[]), "Value"), "16384");
    assert!(field(&pl(p, "ProtocolInfo", &[]), "Value").contains("audio/flac"));
}

/// A GENA callback server: returns each NOTIFY (headers + body).
fn callback_server() -> (String, mpsc::Receiver<String>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}/cb", l.local_addr().unwrap().port());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut s = s;
            s.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
            // Read headers, then the announced body.
            while let Ok(n) = s.read(&mut tmp) {
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(h) = text.find("\r\n\r\n") {
                    let len: usize = text[..h]
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse().ok())?
                        })
                        .unwrap_or(0);
                    if buf.len() >= h + 4 + len {
                        break;
                    }
                }
            }
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
        }
    });
    (url, rx)
}

fn subscribe(port: u16, path: &str, cb: &str) -> String {
    let (head, _) = common::raw(
        port,
        format!(
            "SUBSCRIBE {path} HTTP/1.1\r\nHOST: 127.0.0.1:{port}\r\nCALLBACK: <{cb}>\r\nNT: upnp:event\r\nTIMEOUT: Second-300\r\n\r\n"
        )
        .as_bytes(),
    );
    assert!(head.contains("200 OK"), "{head}");
    head.lines()
        .find_map(|l| l.strip_prefix("SID: "))
        .expect("SID")
        .trim()
        .to_string()
}

/// Next NOTIFY whose body contains `needle`.
fn next_event(rx: &mpsc::Receiver<String>, needle: &str) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let ev = rx
            .recv_timeout(left)
            .unwrap_or_else(|_| panic!("no event containing {needle}"));
        if ev.contains(needle) {
            return ev;
        }
    }
}

#[test]
fn gena_events_openhome_and_lastchange() {
    let r = rig("oh-gena", false);
    let p = r.port();

    let (cb, rx) = callback_server();
    let sid = subscribe(p, "/evt/ohplaylist", &cb);
    // Initial event: SEQ 0, every evented variable, individually.
    let init = next_event(&rx, "IdArray");
    assert!(init.contains("SEQ: 0"), "{init}");
    assert!(init.contains(&format!("SID: {sid}")));
    assert!(init.contains("<e:property><TransportState>Stopped</TransportState></e:property>"));
    assert!(init.contains("<TracksMax>16384</TracksMax>"));

    let id = insert(p, 0, &fixture("tone_16_441.flac"), "");
    let ev = next_event(&rx, "<IdArray>");
    assert!(ev.contains("SEQ: 1"), "{ev}");
    // Only changed variables are sent.
    assert!(!ev.contains("TracksMax"), "{ev}");
    let arr = ev
        .split("<IdArray>")
        .nth(1)
        .unwrap()
        .split("</IdArray>")
        .next()
        .unwrap();
    assert_eq!(b64_decode(arr), id.to_be_bytes());

    // AVTransport: LastChange document, escaped inside the property set.
    let (cb2, rx2) = callback_server();
    subscribe(p, "/evt/avt", &cb2);
    let init = next_event(&rx2, "LastChange");
    assert!(init.contains("SEQ: 0"));
    assert!(
        init.contains("&lt;TransportState val=&quot;STOPPED&quot;/&gt;"),
        "{init}"
    );
    assert!(init.contains("NumberOfTracks val=&quot;1&quot;"), "{init}");
    assert!(init.contains("CurrentPlayMode val=&quot;NORMAL&quot;"));

    soap(
        p,
        "avt",
        AVT,
        "SetPlayMode",
        &[("InstanceID", "0"), ("NewPlayMode", "REPEAT_ONE")],
    );
    let ev = next_event(&rx2, "CurrentPlayMode");
    assert!(ev.contains("REPEAT_ONE"), "{ev}");

    // Unsubscribe / renew bookkeeping.
    let (head, _) = common::raw(
        p,
        format!("SUBSCRIBE /evt/ohplaylist HTTP/1.1\r\nHOST: x\r\nSID: {sid}\r\nTIMEOUT: Second-300\r\n\r\n")
            .as_bytes(),
    );
    assert!(head.contains("200 OK"), "{head}");
    let (head, _) = common::raw(
        p,
        format!("UNSUBSCRIBE /evt/ohplaylist HTTP/1.1\r\nHOST: x\r\nSID: {sid}\r\n\r\n").as_bytes(),
    );
    assert!(head.contains("200 OK"), "{head}");
    let (head, _) = common::raw(
        p,
        format!("UNSUBSCRIBE /evt/ohplaylist HTTP/1.1\r\nHOST: x\r\nSID: {sid}\r\n\r\n").as_bytes(),
    );
    assert!(head.contains("412"), "{head}");
}
