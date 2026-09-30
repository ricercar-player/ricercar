//! Strings produced on the Rust side (toasts, statuses, chain labels) and
//! small formatting helpers. The .slint side is translated through gettext;
//! both read their translations from `tools/<code>.json`.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Interface languages: code and name in the language itself. The settings
/// list them in this order after "System"; English is the source.
pub const LANGUAGES: [(&str, &str); 6] = [
    ("en", "English"),
    ("fr", "Français"),
    ("de", "Deutsch"),
    ("es", "Español"),
    ("it", "Italiano"),
    ("ja", "日本語"),
];

/// The translations, keyed by the English string, in `LANGUAGES` order
/// (none for English).
const CATALOGS: [&str; 6] = [
    "{}",
    include_str!("../tools/fr.json"),
    include_str!("../tools/de.json"),
    include_str!("../tools/es.json"),
    include_str!("../tools/it.json"),
    include_str!("../tools/ja.json"),
];

/// Index in `LANGUAGES` of the language in use.
static CURRENT: AtomicUsize = AtomicUsize::new(0);

/// Pick the interface language: `pref` (a code from `LANGUAGES`) from the
/// settings, or when empty the usual POSIX variables (LC_ALL > LC_MESSAGES >
/// LANG). Unknown languages fall back to English. Applies it to both the
/// Rust strings and the .slint side; returns the code in use.
pub fn set_language(pref: &str) -> &'static str {
    let lang = if pref.is_empty() {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .filter_map(|v| std::env::var(v).ok())
            .find(|v| !v.is_empty())
            .unwrap_or_default()
    } else {
        pref.to_string()
    };
    let i = LANGUAGES
        .iter()
        .position(|(code, _)| lang.starts_with(code))
        .unwrap_or(0);
    CURRENT.store(i, Ordering::Relaxed);
    let code = LANGUAGES[i].0;
    let _ = slint::select_bundled_translation(code);
    code
}

fn current() -> usize {
    #[cfg(test)]
    if let Some(i) = tests::LANG.get() {
        return i;
    }
    CURRENT.load(Ordering::Relaxed)
}

/// Language in use, as a code from `LANGUAGES`.
pub fn language() -> &'static str {
    LANGUAGES[current()].0
}

fn catalog(i: usize) -> &'static HashMap<String, serde_json::Value> {
    static TABLES: [OnceLock<HashMap<String, serde_json::Value>>; 6] =
        [const { OnceLock::new() }; 6];
    TABLES[i].get_or_init(|| serde_json::from_str(CATALOGS[i]).expect("bundled catalog"))
}

/// Translate a Rust-side string (English is the key).
pub fn t(en: &'static str) -> &'static str {
    match catalog(current()).get(en) {
        Some(serde_json::Value::String(s)) => s,
        _ => en,
    }
}

/// Translate a template and fill its `{name}` placeholders. Values are
/// inserted as they are, even when they contain braces.
pub fn tf(en: &'static str, args: &[(&str, &str)]) -> String {
    let mut rest = t(en);
    let mut out = String::with_capacity(rest.len());
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let tail = &rest[open + 1..];
        match tail
            .find('}')
            .and_then(|close| Some((close, args.iter().find(|(k, _)| *k == &tail[..close])?)))
        {
            Some((close, (_, v))) => {
                out.push_str(v);
                rest = &tail[close + 1..];
            }
            None => {
                out.push('{');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

pub fn mmss(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// "1 h 12 min", "48 min", "312 h".
pub fn long_duration(ms: u64) -> String {
    let mins = (ms + 30_000) / 60_000;
    match mins {
        0..=59 => format!("{mins} min"),
        60..=5999 if !mins.is_multiple_of(60) && mins < 600 => {
            format!("{} h {} min", mins / 60, mins % 60)
        }
        _ => format!("{} h", mins / 60),
    }
}

/// "24/96", "16/44.1", "320k" (lossy).
pub fn quality(
    rate: Option<u32>,
    bits: Option<u8>,
    codec: Option<&str>,
    bitrate: Option<u32>,
) -> String {
    let lossy = matches!(codec, Some("MP3" | "AAC" | "Vorbis" | "Opus"));
    if lossy {
        return bitrate.map(|b| format!("{b}k")).unwrap_or_default();
    }
    match (bits, rate) {
        (Some(b), Some(r)) => format!("{b}/{}", khz(r)),
        (None, Some(r)) => khz(r),
        _ => String::new(),
    }
}

pub fn khz(rate: u32) -> String {
    if rate.is_multiple_of(1000) {
        format!("{}", rate / 1000)
    } else {
        format!("{:.1}", rate as f64 / 1000.0)
    }
}

pub fn greeting() -> &'static str {
    use chrono::Timelike;
    match chrono::Local::now().hour() {
        5..=11 => t("Good morning"),
        12..=17 => t("Good afternoon"),
        18..=22 => t("Good evening"),
        _ => t("Good night"),
    }
}

/// kHz with the decimal separator of the language ("352,8" in French).
fn khz_local(rate: u32) -> String {
    decimal(&khz(rate))
}

fn decimal(s: &str) -> String {
    if matches!(language(), "fr" | "de" | "es" | "it") {
        s.replace('.', ",")
    } else {
        s.to_string()
    }
}

/// "12 albums at 352.8 kHz can't be played natively on this DAC".
pub fn unsupported_albums(n: u32, rate: u32) -> String {
    let rate = khz_local(rate);
    let n = n.to_string();
    if n == "1" {
        tf(
            "1 album at {rate} kHz can't be played natively on this DAC",
            &[("rate", &rate)],
        )
    } else {
        tf(
            "{n} albums at {rate} kHz can't be played natively on this DAC",
            &[("n", &n), ("rate", &rate)],
        )
    }
}

/// Install confirmation: what comes from where, and what is not checked.
pub fn install_body(name: &str, version: &str, author: &str, host: &str, repo: &str) -> String {
    let args = [
        ("name", name),
        ("version", version),
        ("author", author),
        ("host", host),
        ("repo", repo),
    ];
    if author.is_empty() {
        tf(
            "{name} {version} will be downloaded from {host} and run with your permissions. ricercar checks that the file matches the catalogue (SHA-256) but does not review what the plugin does. Source: {repo}",
            &args,
        )
    } else {
        tf(
            "{name} {version} by {author} will be downloaded from {host} and run with your permissions. ricercar checks that the file matches the catalogue (SHA-256) but does not review what the plugin does. Source: {repo}",
            &args,
        )
    }
}

/// Warning shown above the confirmation of a plugin update whose binary now
/// comes from another host than the installed one.
pub fn host_changed(old: &str, new: &str) -> String {
    tf(
        "Warning: the download address has changed. The installed version came from {old}; this update comes from {new}. Only continue if you trust the new source.",
        &[("old", old), ("new", new)],
    )
}

/// "The DAC refuses 352.8 kHz (accepts: 44.1–192 kHz)".
pub fn dac_refuses(rate: u32, accepts: Option<&str>) -> String {
    let rate = khz_local(rate);
    match accepts {
        Some(a) => tf(
            "The DAC refuses {rate} kHz (accepts: {accepts})",
            &[("rate", &rate), ("accepts", &decimal(a))],
        ),
        None => tf("The DAC refuses {rate} kHz", &[("rate", &rate)]),
    }
}

pub fn count(n: usize, one: &'static str, many: &'static str) -> String {
    format!("{n} {}", if n == 1 { t(one) } else { t(many) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        /// Language of this test thread, leaving the global one alone.
        pub(super) static LANG: Cell<Option<usize>> = const { Cell::new(None) };
    }

    fn with_language<R>(code: &str, f: impl FnOnce() -> R) -> R {
        let i = LANGUAGES.iter().position(|(c, _)| *c == code).unwrap();
        LANG.set(Some(i));
        let r = f();
        LANG.set(None);
        r
    }

    fn placeholders(s: &str) -> Vec<&str> {
        let mut v: Vec<&str> = s
            .match_indices('{')
            .filter_map(|(i, _)| s[i..].find('}').map(|j| &s[i..=i + j]))
            .collect();
        v.sort();
        v
    }

    /// Every language translates the same strings as French, keeps their
    /// placeholders and has the plural forms gettext expects.
    #[test]
    fn catalogs_are_complete() {
        let fr = catalog(1);
        for (i, (code, _)) in LANGUAGES.iter().enumerate().skip(1) {
            let cat = catalog(i);
            let forms = if *code == "ja" { 1 } else { 2 };
            for (en, ref_value) in fr {
                let value = cat
                    .get(en)
                    .unwrap_or_else(|| panic!("{code}: missing {en:?}"));
                let texts: Vec<&str> = match (ref_value, value) {
                    (serde_json::Value::String(_), serde_json::Value::String(s)) => vec![s],
                    (serde_json::Value::Array(_), serde_json::Value::Array(a)) => {
                        assert_eq!(a.len(), forms, "{code}: plural forms of {en:?}");
                        a.iter().map(|v| v.as_str().unwrap()).collect()
                    }
                    _ => panic!("{code}: wrong kind of value for {en:?}"),
                };
                for text in texts {
                    assert!(!text.trim().is_empty(), "{code}: empty {en:?}");
                    let (want, got) = (placeholders(en), placeholders(text));
                    let plural = ref_value.is_array();
                    assert!(
                        got == want || plural && got.iter().all(|p| want.contains(p)),
                        "{code}: placeholders of {en:?} in {text:?}"
                    );
                }
            }
            assert_eq!(cat.len(), fr.len(), "{code}: strings French does not have");
        }
    }

    /// The literal strings the sources pass to `t` and `tf`.
    fn rust_strings() -> Vec<(std::path::PathBuf, String)> {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        let mut found = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let src = std::fs::read_to_string(&path).unwrap();
            for (i, call) in src.match_indices("t(").chain(src.match_indices("tf(")) {
                if src[..i].ends_with(|c: char| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                let rest = src[i + call.len()..].trim_start();
                let Some(rest) = rest.strip_prefix('"') else {
                    continue;
                };
                let Some(end) = rest.find('"') else { continue };
                let after = rest[end + 1..].trim_start();
                // t takes the string alone, tf the string then its values;
                // anything else is another function.
                let closes = if call == "t(" {
                    after.trim_start_matches(',').trim_start().starts_with(')')
                } else {
                    after.starts_with(',')
                };
                if closes && !rest[..end].contains('\\') {
                    found.push((path.clone(), rest[..end].to_string()));
                }
            }
        }
        found
    }

    /// Every literal string the sources pass to `t` or `tf` has a translation.
    #[test]
    fn rust_strings_are_translated() {
        let strings = rust_strings();
        assert!(
            strings.len() > 100,
            "scanner found {} strings",
            strings.len()
        );
        for (path, en) in strings {
            assert!(
                catalog(1).contains_key(&en),
                "{path:?}: {en:?} is not translated"
            );
        }
    }

    #[test]
    fn templates() {
        assert_eq!(
            tf("{name} installed", &[("name", "Demo {name}")]),
            "Demo {name} installed"
        );
        with_language("ja", || {
            assert_eq!(
                tf("Install {name}?", &[("name", "Demo")]),
                "Demo をインストールしますか？"
            );
        });
    }

    #[test]
    fn formats() {
        assert_eq!(mmss(61_000), "1:01");
        assert_eq!(mmss(3_725_000), "1:02:05");
        assert_eq!(long_duration(47 * 60_000), "47 min");
        assert_eq!(long_duration(72 * 60_000), "1 h 12 min");
        assert_eq!(long_duration(3000 * 60_000), "50 h");
        assert_eq!(quality(Some(96_000), Some(24), Some("FLAC"), None), "24/96");
        assert_eq!(
            quality(Some(44_100), Some(16), Some("FLAC"), None),
            "16/44.1"
        );
        assert_eq!(quality(Some(44_100), None, Some("MP3"), Some(320)), "320k");
        assert_eq!(
            unsupported_albums(12, 352_800),
            "12 albums at 352.8 kHz can't be played natively on this DAC"
        );
        assert_eq!(
            dac_refuses(352_800, Some("44.1–192 kHz")),
            "The DAC refuses 352.8 kHz (accepts: 44.1–192 kHz)"
        );
        assert_eq!(
            install_body("Demo", "1.0", "", "github.com", "https://x"),
            "Demo 1.0 will be downloaded from github.com and run with your permissions. ricercar checks that the file matches the catalogue (SHA-256) but does not review what the plugin does. Source: https://x"
        );
    }

    #[test]
    fn formats_in_french() {
        with_language("fr", || {
            assert_eq!(t("Muted"), "Sourdine");
            assert_eq!(count(3, "track", "tracks"), "3 titres");
            assert_eq!(
                unsupported_albums(1, 352_800),
                "1 album à 352,8 kHz ne pourra pas être lu nativement sur ce DAC"
            );
            assert_eq!(
                dac_refuses(352_800, Some("44.1–192 kHz")),
                "Le DAC refuse 352,8 kHz (accepte : 44,1–192 kHz)"
            );
            assert!(
                install_body("Demo", "1.0", "Jo", "github.com", "https://x")
                    .starts_with("Demo 1.0 de Jo sera téléchargé depuis github.com")
            );
            assert!(host_changed("a.org", "b.org").starts_with("Attention : l'adresse"));
        });
        assert_eq!(t("Muted"), "Muted");
    }

    #[test]
    fn formats_in_german() {
        with_language("de", || {
            assert_eq!(dac_refuses(352_800, None), "Der DAC lehnt 352,8 kHz ab");
        });
    }
}
