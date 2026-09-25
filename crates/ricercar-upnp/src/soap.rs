use quick_xml::Reader;
use quick_xml::events::Event;

use crate::xml;

/// Parse a SOAP envelope: returns (action, args). Argument values are fully
/// unescaped (DIDL metadata arrives entity-escaped inside an argument).
pub fn parse(body: &[u8]) -> Result<(String, Vec<(String, String)>), String> {
    let body = std::str::from_utf8(body).map_err(|_| "bad utf8".to_string())?;
    let mut reader = Reader::from_str(body);
    let mut action: Option<String> = None;
    let mut args: Vec<(String, String)> = Vec::new();
    // 0 before Action, 1 inside Action, 2 inside an argument, >2 nested
    // markup inside an argument (kept verbatim-ish as text is all we need).
    let mut depth = 0usize;
    let mut in_body = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let local = e.local_name().as_ref().to_string();
                if depth == 0 && in_body {
                    action = Some(local);
                    depth = 1;
                } else if depth == 0 && local == "Body" {
                    in_body = true;
                } else if depth == 1 {
                    args.push((local, String::new()));
                    depth = 2;
                } else if depth >= 2 {
                    depth += 1;
                }
            }
            Ok(Event::Empty(e)) if depth == 1 => {
                let local = e.local_name().as_ref().to_string();
                args.push((local, String::new()));
            }
            Ok(Event::Text(t)) if depth == 2 => {
                if let Some(last) = args.last_mut() {
                    last.1.push_str(&t.into_inner());
                }
            }
            Ok(Event::CData(t)) if depth == 2 => {
                if let Some(last) = args.last_mut() {
                    last.1.push_str(&t.into_inner());
                }
            }
            Ok(Event::GeneralRef(r)) if depth == 2 => {
                if let Some(last) = args.last_mut() {
                    last.1.push_str(&xml::resolve_ref(&r));
                }
            }
            Ok(Event::End(_)) => {
                if depth > 0 {
                    depth -= 1;
                    if depth == 0 && action.is_some() {
                        break;
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return Err("bad xml".into()),
            _ => {}
        }
    }
    action
        .ok_or_else(|| "no action".to_string())
        .map(|a| (a, args))
}

pub fn response_body(action: &str, service_ns: &str, out: &[(&str, &str)]) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>",
    );
    s.push_str(&format!("<u:{action}Response xmlns:u=\"{service_ns}\">"));
    for (k, v) in out {
        s.push_str(&format!("<{k}>{}</{k}>", xml::escape(v)));
    }
    s.push_str(&format!("</u:{action}Response></s:Body></s:Envelope>"));
    s
}

pub fn fault(code: u16, desc: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode>{code}</errorCode><errorDescription>{}</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>",
        xml::escape(desc)
    )
}

/// Result of a control action: a response envelope or a UPnP error.
pub enum Reply {
    Ok(String),
    Err(u16, &'static str),
}

impl Reply {
    pub fn ok(action: &str, ns: &str, out: &[(&str, &str)]) -> Reply {
        Reply::Ok(response_body(action, ns, out))
    }
}

/// Named argument lookup.
pub struct Args(pub Vec<(String, String)>);

impl Args {
    pub fn get(&self, k: &str) -> &str {
        self.0
            .iter()
            .find(|(a, _)| a == k)
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    }

    pub fn bool(&self, k: &str) -> Option<bool> {
        match self.get(k).trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" => Some(true),
            "0" | "false" | "no" => Some(false),
            _ => None,
        }
    }

    pub fn num<T: std::str::FromStr>(&self, k: &str) -> Option<T> {
        self.get(k).trim().parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_escaped_didl_argument() {
        let env = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:SetAVTransportURI xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><InstanceID>0</InstanceID><CurrentURI>http://h/a?x=1&amp;y=2</CurrentURI><CurrentURIMetaData>&lt;DIDL-Lite&gt;&lt;dc:title&gt;Tom &amp;amp; Jerry&lt;/dc:title&gt;&lt;/DIDL-Lite&gt;</CurrentURIMetaData><Empty/></u:SetAVTransportURI></s:Body></s:Envelope>"#;
        let (action, args) = parse(env.as_bytes()).unwrap();
        assert_eq!(action, "SetAVTransportURI");
        let a = Args(args);
        assert_eq!(a.get("CurrentURI"), "http://h/a?x=1&y=2");
        assert_eq!(
            a.get("CurrentURIMetaData"),
            "<DIDL-Lite><dc:title>Tom &amp; Jerry</dc:title></DIDL-Lite>"
        );
        assert_eq!(a.get("Empty"), "");
    }
}
