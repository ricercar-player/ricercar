use quick_xml::Reader;
use quick_xml::events::Event;

/// Parse a SOAP envelope: returns (action, args).
pub fn parse(body: &[u8]) -> Result<(String, Vec<(String, String)>), String> {
    let body = std::str::from_utf8(body).map_err(|_| "bad utf8".to_string())?;
    let mut reader = Reader::from_str(body);
    let mut action: Option<String> = None;
    let mut args: Vec<(String, String)> = Vec::new();
    let mut depth = 0usize; // 0 before Action, 1 inside Action, 2 inside an arg
    let mut in_body = false;
    let mut cur_key = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let local = e.local_name().as_ref().to_string();
                if depth == 0 && in_body {
                    action = Some(local);
                    depth = 1;
                } else if local == "Body" {
                    in_body = true;
                } else if depth == 1 {
                    cur_key = local;
                    depth = 2;
                }
            }
            Ok(Event::Empty(e)) if depth == 1 => {
                let local = e.local_name().as_ref().to_string();
                args.push((local, String::new()));
            }
            Ok(Event::Text(t)) if depth == 2 => {
                let val = t.into_inner().into_owned();
                if args.last().map(|(k, _)| k == &cur_key) == Some(true) {
                    args.last_mut().unwrap().1 = val;
                } else {
                    args.push((cur_key.clone(), val));
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
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>",
    );
    s.push_str(&format!("<u:{action}Response xmlns:u=\"{service_ns}\">"));
    for (k, v) in out {
        s.push_str(&format!("<{k}>{}</{k}>", crate::desc::xml_escape(v)));
    }
    s.push_str(&format!("</u:{action}Response></s:Body></s:Envelope>"));
    s
}

pub fn fault(code: u16, desc: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode>{code}</errorCode><errorDescription>{}</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>",
        crate::desc::xml_escape(desc)
    )
}
