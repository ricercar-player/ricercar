//! Device descriptions and service SCPDs.

use std::sync::OnceLock;

use crate::Svc;

pub use crate::xml::escape as xml_escape;

/// (name, direction is out, related state variable)
type ArgDef = (&'static str, bool, &'static str);
/// (action name, arguments)
type ActionDef = (&'static str, &'static [ArgDef]);

/// State variable: (name, data type, sendEvents, allowed values, range).
type VarDef = (
    &'static str,
    &'static str,
    bool,
    &'static [&'static str],
    Option<(u32, u32)>,
);

const I: bool = false;
const O: bool = true;
const IID: ArgDef = ("InstanceID", I, "A_ARG_TYPE_InstanceID");

fn scpd(actions: &[ActionDef], vars: &[VarDef]) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<scpd xmlns=\"urn:schemas-upnp-org:service-1-0\"><specVersion><major>1</major><minor>0</minor></specVersion><actionList>",
    );
    for (name, args) in actions {
        s.push_str(&format!("<action><name>{name}</name>"));
        if !args.is_empty() {
            s.push_str("<argumentList>");
            for (a, out, var) in args.iter() {
                s.push_str(&format!(
                    "<argument><name>{a}</name><direction>{}</direction><relatedStateVariable>{var}</relatedStateVariable></argument>",
                    if *out { "out" } else { "in" }
                ));
            }
            s.push_str("</argumentList>");
        }
        s.push_str("</action>");
    }
    s.push_str("</actionList><serviceStateTable>");
    for (name, ty, evented, allowed, range) in vars {
        s.push_str(&format!(
            "<stateVariable sendEvents=\"{}\"><name>{name}</name><dataType>{ty}</dataType>",
            if *evented { "yes" } else { "no" }
        ));
        if !allowed.is_empty() {
            s.push_str("<allowedValueList>");
            for v in allowed.iter() {
                s.push_str(&format!("<allowedValue>{v}</allowedValue>"));
            }
            s.push_str("</allowedValueList>");
        }
        if let Some((min, max)) = range {
            s.push_str(&format!(
                "<allowedValueRange><minimum>{min}</minimum><maximum>{max}</maximum><step>1</step></allowedValueRange>"
            ));
        }
        s.push_str("</stateVariable>");
    }
    s.push_str("</serviceStateTable></scpd>");
    s
}

const fn v(name: &'static str, ty: &'static str) -> VarDef {
    (name, ty, false, &[], None)
}
const fn ev(name: &'static str, ty: &'static str) -> VarDef {
    (name, ty, true, &[], None)
}

fn avt_scpd() -> String {
    scpd(
        &[
            (
                "SetAVTransportURI",
                &[
                    IID,
                    ("CurrentURI", I, "AVTransportURI"),
                    ("CurrentURIMetaData", I, "AVTransportURIMetaData"),
                ],
            ),
            (
                "SetNextAVTransportURI",
                &[
                    IID,
                    ("NextURI", I, "NextAVTransportURI"),
                    ("NextURIMetaData", I, "NextAVTransportURIMetaData"),
                ],
            ),
            (
                "GetMediaInfo",
                &[
                    IID,
                    ("NrTracks", O, "NumberOfTracks"),
                    ("MediaDuration", O, "CurrentMediaDuration"),
                    ("CurrentURI", O, "AVTransportURI"),
                    ("CurrentURIMetaData", O, "AVTransportURIMetaData"),
                    ("NextURI", O, "NextAVTransportURI"),
                    ("NextURIMetaData", O, "NextAVTransportURIMetaData"),
                    ("PlayMedium", O, "PlaybackStorageMedium"),
                    ("RecordMedium", O, "RecordStorageMedium"),
                    ("WriteStatus", O, "RecordMediumWriteStatus"),
                ],
            ),
            (
                "GetTransportInfo",
                &[
                    IID,
                    ("CurrentTransportState", O, "TransportState"),
                    ("CurrentTransportStatus", O, "TransportStatus"),
                    ("CurrentSpeed", O, "TransportPlaySpeed"),
                ],
            ),
            (
                "GetPositionInfo",
                &[
                    IID,
                    ("Track", O, "CurrentTrack"),
                    ("TrackDuration", O, "CurrentTrackDuration"),
                    ("TrackMetaData", O, "CurrentTrackMetaData"),
                    ("TrackURI", O, "CurrentTrackURI"),
                    ("RelTime", O, "RelativeTimePosition"),
                    ("AbsTime", O, "AbsoluteTimePosition"),
                    ("RelCount", O, "RelativeCounterPosition"),
                    ("AbsCount", O, "AbsoluteCounterPosition"),
                ],
            ),
            (
                "GetDeviceCapabilities",
                &[
                    IID,
                    ("PlayMedia", O, "PossiblePlaybackStorageMedia"),
                    ("RecMedia", O, "PossibleRecordStorageMedia"),
                    ("RecQualityModes", O, "PossibleRecordQualityModes"),
                ],
            ),
            (
                "GetTransportSettings",
                &[
                    IID,
                    ("PlayMode", O, "CurrentPlayMode"),
                    ("RecQualityMode", O, "CurrentRecordQualityMode"),
                ],
            ),
            ("Stop", &[IID]),
            ("Play", &[IID, ("Speed", I, "TransportPlaySpeed")]),
            ("Pause", &[IID]),
            (
                "Seek",
                &[
                    IID,
                    ("Unit", I, "A_ARG_TYPE_SeekMode"),
                    ("Target", I, "A_ARG_TYPE_SeekTarget"),
                ],
            ),
            ("Next", &[IID]),
            ("Previous", &[IID]),
            ("SetPlayMode", &[IID, ("NewPlayMode", I, "CurrentPlayMode")]),
            (
                "GetCurrentTransportActions",
                &[IID, ("Actions", O, "CurrentTransportActions")],
            ),
        ],
        &[
            v("A_ARG_TYPE_InstanceID", "ui4"),
            (
                "A_ARG_TYPE_SeekMode",
                "string",
                false,
                &["ABS_TIME", "REL_TIME", "TRACK_NR"],
                None,
            ),
            v("A_ARG_TYPE_SeekTarget", "string"),
            ev("LastChange", "string"),
            (
                "TransportState",
                "string",
                false,
                &[
                    "STOPPED",
                    "PLAYING",
                    "PAUSED_PLAYBACK",
                    "TRANSITIONING",
                    "NO_MEDIA_PRESENT",
                ],
                None,
            ),
            (
                "TransportStatus",
                "string",
                false,
                &["OK", "ERROR_OCCURRED"],
                None,
            ),
            ("TransportPlaySpeed", "string", false, &["1"], None),
            (
                "PlaybackStorageMedium",
                "string",
                false,
                &["NONE", "NETWORK"],
                None,
            ),
            (
                "RecordStorageMedium",
                "string",
                false,
                &["NOT_IMPLEMENTED"],
                None,
            ),
            v("PossiblePlaybackStorageMedia", "string"),
            v("PossibleRecordStorageMedia", "string"),
            v("PossibleRecordQualityModes", "string"),
            (
                "RecordMediumWriteStatus",
                "string",
                false,
                &["NOT_IMPLEMENTED"],
                None,
            ),
            (
                "CurrentRecordQualityMode",
                "string",
                false,
                &["NOT_IMPLEMENTED"],
                None,
            ),
            (
                "CurrentPlayMode",
                "string",
                false,
                &["NORMAL", "SHUFFLE", "REPEAT_ONE", "REPEAT_ALL"],
                None,
            ),
            v("NumberOfTracks", "ui4"),
            v("CurrentTrack", "ui4"),
            v("CurrentTrackDuration", "string"),
            v("CurrentMediaDuration", "string"),
            v("CurrentTrackMetaData", "string"),
            v("CurrentTrackURI", "string"),
            v("AVTransportURI", "string"),
            v("AVTransportURIMetaData", "string"),
            v("NextAVTransportURI", "string"),
            v("NextAVTransportURIMetaData", "string"),
            v("RelativeTimePosition", "string"),
            v("AbsoluteTimePosition", "string"),
            v("RelativeCounterPosition", "i4"),
            v("AbsoluteCounterPosition", "i4"),
            v("CurrentTransportActions", "string"),
        ],
    )
}

fn rcs_scpd() -> String {
    const CH: ArgDef = ("Channel", I, "A_ARG_TYPE_Channel");
    scpd(
        &[
            (
                "ListPresets",
                &[IID, ("CurrentPresetNameList", O, "PresetNameList")],
            ),
            (
                "SelectPreset",
                &[IID, ("PresetName", I, "A_ARG_TYPE_PresetName")],
            ),
            ("GetMute", &[IID, CH, ("CurrentMute", O, "Mute")]),
            ("SetMute", &[IID, CH, ("DesiredMute", I, "Mute")]),
            ("GetVolume", &[IID, CH, ("CurrentVolume", O, "Volume")]),
            ("SetVolume", &[IID, CH, ("DesiredVolume", I, "Volume")]),
        ],
        &[
            v("A_ARG_TYPE_InstanceID", "ui4"),
            ("A_ARG_TYPE_Channel", "string", false, &["Master"], None),
            (
                "A_ARG_TYPE_PresetName",
                "string",
                false,
                &["FactoryDefaults"],
                None,
            ),
            v("PresetNameList", "string"),
            ev("LastChange", "string"),
            v("Mute", "boolean"),
            ("Volume", "ui2", false, &[], Some((0, 100))),
        ],
    )
}

fn cms_scpd() -> String {
    scpd(
        &[
            (
                "GetProtocolInfo",
                &[
                    ("Source", O, "SourceProtocolInfo"),
                    ("Sink", O, "SinkProtocolInfo"),
                ],
            ),
            (
                "GetCurrentConnectionIDs",
                &[("ConnectionIDs", O, "CurrentConnectionIDs")],
            ),
            (
                "GetCurrentConnectionInfo",
                &[
                    ("ConnectionID", I, "A_ARG_TYPE_ConnectionID"),
                    ("RcsID", O, "A_ARG_TYPE_RcsID"),
                    ("AVTransportID", O, "A_ARG_TYPE_AVTransportID"),
                    ("ProtocolInfo", O, "A_ARG_TYPE_ProtocolInfo"),
                    ("PeerConnectionManager", O, "A_ARG_TYPE_ConnectionManager"),
                    ("PeerConnectionID", O, "A_ARG_TYPE_ConnectionID"),
                    ("Direction", O, "A_ARG_TYPE_Direction"),
                    ("Status", O, "A_ARG_TYPE_ConnectionStatus"),
                ],
            ),
        ],
        &[
            ev("SourceProtocolInfo", "string"),
            ev("SinkProtocolInfo", "string"),
            ev("CurrentConnectionIDs", "string"),
            v("A_ARG_TYPE_ConnectionID", "i4"),
            v("A_ARG_TYPE_RcsID", "i4"),
            v("A_ARG_TYPE_AVTransportID", "i4"),
            v("A_ARG_TYPE_ProtocolInfo", "string"),
            v("A_ARG_TYPE_ConnectionManager", "string"),
            (
                "A_ARG_TYPE_Direction",
                "string",
                false,
                &["Input", "Output"],
                None,
            ),
            (
                "A_ARG_TYPE_ConnectionStatus",
                "string",
                false,
                &[
                    "OK",
                    "ContentFormatMismatch",
                    "InsufficientBandwidth",
                    "UnreliableChannel",
                    "Unknown",
                ],
                None,
            ),
        ],
    )
}

fn cd_scpd() -> String {
    const BROWSE_OUT: [ArgDef; 4] = [
        ("Result", O, "A_ARG_TYPE_Result"),
        ("NumberReturned", O, "A_ARG_TYPE_Count"),
        ("TotalMatches", O, "A_ARG_TYPE_Count"),
        ("UpdateID", O, "A_ARG_TYPE_UpdateID"),
    ];
    scpd(
        &[
            (
                "GetSearchCapabilities",
                &[("SearchCaps", O, "SearchCapabilities")],
            ),
            (
                "GetSortCapabilities",
                &[("SortCaps", O, "SortCapabilities")],
            ),
            ("GetSystemUpdateID", &[("Id", O, "SystemUpdateID")]),
            (
                "Browse",
                &[
                    ("ObjectID", I, "A_ARG_TYPE_ObjectID"),
                    ("BrowseFlag", I, "A_ARG_TYPE_BrowseFlag"),
                    ("Filter", I, "A_ARG_TYPE_Filter"),
                    ("StartingIndex", I, "A_ARG_TYPE_Index"),
                    ("RequestCount", I, "A_ARG_TYPE_Count"),
                    ("SortCriteria", I, "A_ARG_TYPE_SortCriteria"),
                    BROWSE_OUT[0],
                    BROWSE_OUT[1],
                    BROWSE_OUT[2],
                    BROWSE_OUT[3],
                ],
            ),
            (
                "Search",
                &[
                    ("ContainerID", I, "A_ARG_TYPE_ObjectID"),
                    ("SearchCriteria", I, "A_ARG_TYPE_SearchCriteria"),
                    ("Filter", I, "A_ARG_TYPE_Filter"),
                    ("StartingIndex", I, "A_ARG_TYPE_Index"),
                    ("RequestCount", I, "A_ARG_TYPE_Count"),
                    ("SortCriteria", I, "A_ARG_TYPE_SortCriteria"),
                    BROWSE_OUT[0],
                    BROWSE_OUT[1],
                    BROWSE_OUT[2],
                    BROWSE_OUT[3],
                ],
            ),
        ],
        &[
            v("SearchCapabilities", "string"),
            v("SortCapabilities", "string"),
            ev("SystemUpdateID", "ui4"),
            v("A_ARG_TYPE_ObjectID", "string"),
            v("A_ARG_TYPE_Result", "string"),
            v("A_ARG_TYPE_SearchCriteria", "string"),
            (
                "A_ARG_TYPE_BrowseFlag",
                "string",
                false,
                &["BrowseMetadata", "BrowseDirectChildren"],
                None,
            ),
            v("A_ARG_TYPE_Filter", "string"),
            v("A_ARG_TYPE_SortCriteria", "string"),
            v("A_ARG_TYPE_Index", "ui4"),
            v("A_ARG_TYPE_Count", "ui4"),
            v("A_ARG_TYPE_UpdateID", "ui4"),
        ],
    )
}

// ------------------------------------------------------------- OpenHome

fn oh_product_scpd() -> String {
    scpd(
        &[
            (
                "Manufacturer",
                &[
                    ("Name", O, "ManufacturerName"),
                    ("Info", O, "ManufacturerInfo"),
                    ("Url", O, "ManufacturerUrl"),
                    ("ImageUri", O, "ManufacturerImageUri"),
                ],
            ),
            (
                "Model",
                &[
                    ("Name", O, "ModelName"),
                    ("Info", O, "ModelInfo"),
                    ("Url", O, "ModelUrl"),
                    ("ImageUri", O, "ModelImageUri"),
                ],
            ),
            (
                "Product",
                &[
                    ("Room", O, "ProductRoom"),
                    ("Name", O, "ProductName"),
                    ("Info", O, "ProductInfo"),
                    ("Url", O, "ProductUrl"),
                    ("ImageUri", O, "ProductImageUri"),
                ],
            ),
            ("Standby", &[("Value", O, "Standby")]),
            ("SetStandby", &[("Value", I, "Standby")]),
            ("SourceCount", &[("Value", O, "SourceCount")]),
            ("SourceXml", &[("Value", O, "SourceXml")]),
            ("SourceIndex", &[("Value", O, "SourceIndex")]),
            ("SetSourceIndex", &[("Value", I, "SourceIndex")]),
            ("SetSourceIndexByName", &[("Value", I, "SourceName")]),
            (
                "Source",
                &[
                    ("Index", I, "SourceIndex"),
                    ("SystemName", O, "SourceName"),
                    ("Type", O, "SourceType"),
                    ("Name", O, "SourceName"),
                    ("Visible", O, "SourceVisible"),
                ],
            ),
            ("Attributes", &[("Value", O, "Attributes")]),
            (
                "SourceXmlChangeCount",
                &[("Value", O, "SourceXmlChangeCount")],
            ),
        ],
        &[
            ev("ManufacturerName", "string"),
            ev("ManufacturerInfo", "string"),
            ev("ManufacturerUrl", "string"),
            ev("ManufacturerImageUri", "string"),
            ev("ModelName", "string"),
            ev("ModelInfo", "string"),
            ev("ModelUrl", "string"),
            ev("ModelImageUri", "string"),
            ev("ProductRoom", "string"),
            ev("ProductName", "string"),
            ev("ProductInfo", "string"),
            ev("ProductUrl", "string"),
            ev("ProductImageUri", "string"),
            ev("Standby", "boolean"),
            ev("SourceIndex", "ui4"),
            ev("SourceCount", "ui4"),
            ev("SourceXml", "string"),
            ev("Attributes", "string"),
            v("SourceXmlChangeCount", "ui4"),
            v("SourceType", "string"),
            v("SourceName", "string"),
            v("SourceVisible", "boolean"),
        ],
    )
}

fn oh_playlist_scpd() -> String {
    scpd(
        &[
            ("Play", &[]),
            ("Pause", &[]),
            ("Stop", &[]),
            ("Next", &[]),
            ("Previous", &[]),
            ("SetRepeat", &[("Value", I, "Repeat")]),
            ("Repeat", &[("Value", O, "Repeat")]),
            ("SetShuffle", &[("Value", I, "Shuffle")]),
            ("Shuffle", &[("Value", O, "Shuffle")]),
            ("SeekSecondAbsolute", &[("Value", I, "Absolute")]),
            ("SeekSecondRelative", &[("Value", I, "Relative")]),
            ("SeekId", &[("Value", I, "Id")]),
            ("SeekIndex", &[("Value", I, "Index")]),
            ("TransportState", &[("Value", O, "TransportState")]),
            ("Id", &[("Value", O, "Id")]),
            (
                "Read",
                &[
                    ("Id", I, "Id"),
                    ("Uri", O, "Uri"),
                    ("Metadata", O, "Metadata"),
                ],
            ),
            (
                "ReadList",
                &[("IdList", I, "IdList"), ("TrackList", O, "TrackList")],
            ),
            (
                "Insert",
                &[
                    ("AfterId", I, "Id"),
                    ("Uri", I, "Uri"),
                    ("Metadata", I, "Metadata"),
                    ("NewId", O, "Id"),
                ],
            ),
            ("DeleteId", &[("Value", I, "Id")]),
            ("DeleteAll", &[]),
            ("TracksMax", &[("Value", O, "TracksMax")]),
            (
                "IdArray",
                &[("Token", O, "IdArrayToken"), ("Array", O, "IdArray")],
            ),
            (
                "IdArrayChanged",
                &[("Token", I, "IdArrayToken"), ("Value", O, "IdArrayChanged")],
            ),
            ("ProtocolInfo", &[("Value", O, "ProtocolInfo")]),
        ],
        &[
            (
                "TransportState",
                "string",
                true,
                &["Playing", "Paused", "Stopped", "Buffering"],
                None,
            ),
            ev("Repeat", "boolean"),
            ev("Shuffle", "boolean"),
            ev("Id", "ui4"),
            ev("IdArray", "bin.base64"),
            ev("TracksMax", "ui4"),
            ev("ProtocolInfo", "string"),
            v("Index", "ui4"),
            v("Relative", "i4"),
            v("Absolute", "ui4"),
            v("IdList", "string"),
            v("TrackList", "string"),
            v("Uri", "string"),
            v("Metadata", "string"),
            v("IdArrayToken", "ui4"),
            v("IdArrayChanged", "boolean"),
        ],
    )
}

fn oh_info_scpd() -> String {
    scpd(
        &[
            (
                "Counters",
                &[
                    ("TrackCount", O, "TrackCount"),
                    ("DetailsCount", O, "DetailsCount"),
                    ("MetatextCount", O, "MetatextCount"),
                ],
            ),
            ("Track", &[("Uri", O, "Uri"), ("Metadata", O, "Metadata")]),
            (
                "Details",
                &[
                    ("Duration", O, "Duration"),
                    ("BitRate", O, "BitRate"),
                    ("BitDepth", O, "BitDepth"),
                    ("SampleRate", O, "SampleRate"),
                    ("Lossless", O, "Lossless"),
                    ("CodecName", O, "CodecName"),
                ],
            ),
            ("Metatext", &[("Value", O, "Metatext")]),
        ],
        &[
            ev("TrackCount", "ui4"),
            ev("DetailsCount", "ui4"),
            ev("MetatextCount", "ui4"),
            ev("Uri", "string"),
            ev("Metadata", "string"),
            ev("Duration", "ui4"),
            ev("BitRate", "ui4"),
            ev("BitDepth", "ui4"),
            ev("SampleRate", "ui4"),
            ev("Lossless", "boolean"),
            ev("CodecName", "string"),
            ev("Metatext", "string"),
        ],
    )
}

fn oh_time_scpd() -> String {
    scpd(
        &[(
            "Time",
            &[
                ("TrackCount", O, "TrackCount"),
                ("Duration", O, "Duration"),
                ("Seconds", O, "Seconds"),
            ],
        )],
        &[
            ev("TrackCount", "ui4"),
            ev("Duration", "ui4"),
            ev("Seconds", "ui4"),
        ],
    )
}

fn oh_volume_scpd() -> String {
    scpd(
        &[
            (
                "Characteristics",
                &[
                    ("VolumeMax", O, "VolumeMax"),
                    ("VolumeUnity", O, "VolumeUnity"),
                    ("VolumeSteps", O, "VolumeSteps"),
                    ("VolumeMilliDbPerStep", O, "VolumeMilliDbPerStep"),
                    ("BalanceMax", O, "BalanceMax"),
                    ("FadeMax", O, "FadeMax"),
                ],
            ),
            ("SetVolume", &[("Value", I, "Volume")]),
            ("VolumeInc", &[]),
            ("VolumeDec", &[]),
            ("Volume", &[("Value", O, "Volume")]),
            ("SetBalance", &[("Value", I, "Balance")]),
            ("BalanceInc", &[]),
            ("BalanceDec", &[]),
            ("Balance", &[("Value", O, "Balance")]),
            ("SetFade", &[("Value", I, "Fade")]),
            ("FadeInc", &[]),
            ("FadeDec", &[]),
            ("Fade", &[("Value", O, "Fade")]),
            ("SetMute", &[("Value", I, "Mute")]),
            ("Mute", &[("Value", O, "Mute")]),
            ("VolumeLimit", &[("Value", O, "VolumeLimit")]),
        ],
        &[
            ev("Volume", "ui4"),
            ev("Mute", "boolean"),
            ev("Balance", "i4"),
            ev("Fade", "i4"),
            ev("VolumeLimit", "ui4"),
            ev("VolumeMax", "ui4"),
            ev("VolumeUnity", "ui4"),
            ev("VolumeSteps", "ui4"),
            ev("VolumeMilliDbPerStep", "ui4"),
            ev("BalanceMax", "ui4"),
            ev("FadeMax", "ui4"),
        ],
    )
}

/// SCPD document for a service (built once).
pub(crate) fn scpd_for(svc: Svc) -> &'static str {
    static CACHE: OnceLock<Vec<(Svc, String)>> = OnceLock::new();
    let all = CACHE.get_or_init(|| {
        vec![
            (Svc::Avt, avt_scpd()),
            (Svc::Rcs, rcs_scpd()),
            (Svc::Cms, cms_scpd()),
            (Svc::ServerCms, cms_scpd()),
            (Svc::Cd, cd_scpd()),
            (Svc::OhProduct, oh_product_scpd()),
            (Svc::OhPlaylist, oh_playlist_scpd()),
            (Svc::OhInfo, oh_info_scpd()),
            (Svc::OhTime, oh_time_scpd()),
            (Svc::OhVolume, oh_volume_scpd()),
        ]
    });
    all.iter()
        .find(|(s, _)| *s == svc)
        .map(|(_, x)| x.as_str())
        .unwrap_or("")
}

fn service_xml(svc: Svc) -> String {
    format!(
        "<service><serviceType>{}</serviceType><serviceId>{}</serviceId><SCPDURL>/svc/{p}.xml</SCPDURL><controlURL>/ctl/{p}</controlURL><eventSubURL>/evt/{p}</eventSubURL></service>",
        svc.urn(),
        svc.service_id(),
        p = svc.path()
    )
}

fn device_doc(
    device_type: &str,
    friendly: &str,
    udn: &str,
    description: &str,
    dlna: &str,
    services: &[Svc],
) -> String {
    let services: String = services.iter().map(|s| service_xml(*s)).collect();
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0" xmlns:dlna="urn:schemas-dlna-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <device>
    <deviceType>{device_type}</deviceType>
    <friendlyName>{}</friendlyName>
    <manufacturer>Ricercar</manufacturer>
    <manufacturerURL>https://github.com/ricercar-player/ricercar</manufacturerURL>
    <modelDescription>{description}</modelDescription>
    <modelName>Ricercar</modelName>
    <modelNumber>{}</modelNumber>
    <UDN>uuid:{udn}</UDN>
    <dlna:X_DLNADOC>{dlna}</dlna:X_DLNADOC>
    <serviceList>{services}</serviceList>
  </device>
</root>"#,
        xml_escape(friendly),
        env!("CARGO_PKG_VERSION"),
    )
}

pub fn device_xml(friendly_name: &str, udn: &str) -> String {
    device_doc(
        "urn:schemas-upnp-org:device:MediaRenderer:1",
        friendly_name,
        udn,
        "Bit-perfect network renderer",
        "DMR-1.50",
        &[
            Svc::Avt,
            Svc::Rcs,
            Svc::Cms,
            Svc::OhProduct,
            Svc::OhPlaylist,
            Svc::OhInfo,
            Svc::OhTime,
            Svc::OhVolume,
        ],
    )
}

pub fn server_xml(friendly_name: &str, udn: &str) -> String {
    device_doc(
        "urn:schemas-upnp-org:device:MediaServer:1",
        &format!("{friendly_name} (library)"),
        udn,
        "Personal music library server",
        "DMS-1.50",
        &[Svc::Cd, Svc::ServerCms],
    )
}
