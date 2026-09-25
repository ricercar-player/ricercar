//! Small XML helpers shared by the SOAP, DIDL and description code.

use quick_xml::events::BytesRef;

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// Text for an entity/character reference event (quick-xml reports `&amp;`
/// and friends as separate events, between text chunks).
pub fn resolve_ref(r: &BytesRef<'_>) -> String {
    if let Ok(Some(c)) = r.resolve_char_ref() {
        return c.to_string();
    }
    let name = r.borrow().into_inner();
    match quick_xml::escape::resolve_predefined_entity(&name) {
        Some(s) => s.to_string(),
        None => format!("&{name};"),
    }
}

/// Unescaped value of an attribute.
pub fn attr_value(a: &quick_xml::events::attributes::Attribute<'_>) -> String {
    a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
        .map(|v| v.into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #[test]
    fn escape_roundtrip_chars() {
        assert_eq!(super::escape("a<b>&\"'"), "a&lt;b&gt;&amp;&quot;&apos;");
    }
}
