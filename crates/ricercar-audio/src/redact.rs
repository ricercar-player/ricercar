//! URLs as they may appear in logs and error messages: signed stream URLs
//! carry tokens in their query, sometimes credentials in their authority.

const MASK: &str = "…";

/// Keep scheme, host, port and path; replace userinfo, every query value
/// and the fragment with `…`. Text without `://` is returned unchanged.
pub fn redact_url(url: &str) -> String {
    let Some(i) = url.find("://") else {
        return url.to_string();
    };
    let (scheme, rest) = url.split_at(i + 3);
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, rest) = rest.split_at(auth_end);
    let mut out = String::with_capacity(url.len());
    out.push_str(scheme);
    match authority.rfind('@') {
        Some(at) => {
            out.push_str(MASK);
            out.push_str(&authority[at..]);
        }
        None => out.push_str(authority),
    }
    let (rest, fragment) = match rest.split_once('#') {
        Some((r, _)) => (r, true),
        None => (rest, false),
    };
    let (path, query) = match rest.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (rest, None),
    };
    out.push_str(path);
    if let Some(q) = query {
        out.push('?');
        let parts: Vec<String> = q
            .split('&')
            .map(|kv| match kv.split_once('=') {
                Some((k, _)) => format!("{k}={MASK}"),
                None if kv.is_empty() => String::new(),
                None => MASK.to_string(),
            })
            .collect();
        out.push_str(&parts.join("&"));
    }
    if fragment {
        out.push('#');
        out.push_str(MASK);
    }
    out
}

/// [`redact_url`] applied to every `http://` / `https://` URL in `text`.
pub fn redact_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = find_url(rest) {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail
            .find(|c: char| c.is_whitespace() || "\"'<>`".contains(c))
            .unwrap_or(tail.len());
        // Punctuation closing a sentence is not part of the URL.
        let url = tail[..end].trim_end_matches(['.', ',', ';', ':', ')', ']']);
        out.push_str(&redact_url(url));
        rest = &tail[url.len()..];
    }
    out.push_str(rest);
    out
}

fn find_url(text: &str) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    match (lower.find("http://"), lower.find("https://")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_values_are_masked() {
        assert_eq!(
            redact_url("https://cdn.example:8443/a/b.flac?uid=42&hmac=deadbeef&x"),
            "https://cdn.example:8443/a/b.flac?uid=…&hmac=…&…"
        );
        assert_eq!(redact_url("http://h/p?=v&k="), "http://h/p?=…&k=…");
    }

    #[test]
    fn userinfo_and_fragment_are_masked() {
        assert_eq!(
            redact_url("https://user:pass@host/x#token=abc"),
            "https://…@host/x#…"
        );
        assert_eq!(redact_url("http://u@h:80?q=1"), "http://…@h:80?q=…");
    }

    #[test]
    fn plain_urls_and_paths_are_unchanged() {
        assert_eq!(redact_url("http://host/a/b.flac"), "http://host/a/b.flac");
        assert_eq!(redact_url("https://host"), "https://host");
        assert_eq!(redact_url("/music/a?b.flac"), "/music/a?b.flac");
        assert_eq!(redact_url("plugin://x/track/1"), "plugin://x/track/1");
    }

    #[test]
    fn urls_in_text_are_masked() {
        assert_eq!(
            redact_urls("GET https://h/x?sig=s3cr3t: status 410, http://a/b?t=1."),
            "GET https://h/x?sig=…: status 410, http://a/b?t=…."
        );
        assert_eq!(redact_urls("no url here"), "no url here");
        let once = redact_urls("x https://h/p?a=1&b=2#f y");
        assert_eq!(redact_urls(&once), once);
    }
}
