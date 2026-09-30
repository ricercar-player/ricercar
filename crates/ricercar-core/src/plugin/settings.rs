//! Settings a plugin declares (docs/plugins.md, "Settings"): the schema
//! sent in `initialize` or `settings.declared`, checked here, and the values
//! the user picked, stored in the plugin's `[[plugins]]` table.

use std::collections::BTreeMap;

use serde_json::Map;
pub use serde_json::Value;

/// Most entries kept from a declaration.
pub const MAX_SETTINGS: usize = 100;
/// Longest string value, whatever the plugin allows.
pub const MAX_STRING: usize = 1024;
/// Most options of a `choice`.
pub const MAX_OPTIONS: usize = 50;
const MAX_KEY: usize = 64;
const MAX_LABEL: usize = 200;
const MAX_DESCRIPTION: usize = 1000;
const MAX_SECTION: usize = 80;
const MAX_SHORT: usize = 200;
const MAX_UNIT: usize = 20;

/// Values stored for a plugin, as in `[plugins.settings]`: only the ones
/// that differ from their default.
pub type Stored = BTreeMap<String, toml::Value>;

#[derive(Debug, Clone, PartialEq)]
pub struct ChoiceOption {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Bool,
    String {
        placeholder: Option<String>,
        max_length: usize,
    },
    Number {
        min: Option<f64>,
        max: Option<f64>,
        step: Option<f64>,
        integer: bool,
        unit: Option<String>,
    },
    Choice {
        options: Vec<ChoiceOption>,
    },
}

/// One declared setting, checked.
#[derive(Debug, Clone, PartialEq)]
pub struct Setting {
    pub key: String,
    pub label: String,
    pub description: Option<String>,
    pub section: Option<String>,
    /// Takes effect after the plugin restarts: the host restarts it on
    /// change instead of sending `settings.changed`.
    pub restart: bool,
    pub default: Value,
    pub kind: Kind,
}

/// Why a value was refused.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingError {
    /// The plugin declares no such key.
    UnknownKey,
    WrongType,
    BelowMin(f64),
    AboveMax(f64),
    /// Longer than that many characters.
    TooLong(usize),
    NotAnOption,
}

impl std::fmt::Display for SettingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SettingError::UnknownKey => write!(f, "unknown setting"),
            SettingError::WrongType => write!(f, "wrong type"),
            SettingError::BelowMin(m) => write!(f, "at least {m}"),
            SettingError::AboveMax(m) => write!(f, "at most {m}"),
            SettingError::TooLong(n) => write!(f, "at most {n} characters"),
            SettingError::NotAnOption => write!(f, "not one of the options"),
        }
    }
}

impl std::error::Error for SettingError {}

pub fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_KEY
        && key.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-')
        })
}

/// Plain text on one line, at most `max` characters.
fn clean(s: &str, max: usize) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max)
        .collect::<String>()
        .trim()
        .to_string()
}

fn text(v: &Value, field: &str, max: usize) -> Option<String> {
    v.get(field)
        .and_then(Value::as_str)
        .map(|s| clean(s, max))
        .filter(|s| !s.is_empty())
}

fn finite(v: &Value, field: &str) -> Result<Option<f64>, String> {
    match v.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(n) => n
            .as_f64()
            .filter(|n| n.is_finite())
            .map(Some)
            .ok_or_else(|| format!("{field} is not a number")),
    }
}

fn number_value(n: f64, integer: bool) -> Value {
    if integer && n.abs() < 9.0e15 {
        Value::from(n as i64)
    } else {
        serde_json::Number::from_f64(n)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

impl Setting {
    /// One entry of a declaration, or why it is refused.
    fn parse(v: &Value) -> Result<Setting, String> {
        let key = v
            .get("key")
            .and_then(Value::as_str)
            .ok_or("no key")?
            .to_string();
        if !valid_key(&key) {
            return Err(format!("bad key \"{}\"", clean(&key, MAX_KEY)));
        }
        let label = text(v, "label", MAX_LABEL).ok_or_else(|| format!("{key}: no label"))?;
        let kind = match v.get("type").and_then(Value::as_str) {
            Some("bool") => Kind::Bool,
            Some("string") => {
                let max_length = match v.get("max_length") {
                    None | Some(Value::Null) => MAX_STRING,
                    Some(n) => n
                        .as_u64()
                        .filter(|n| *n >= 1)
                        .ok_or_else(|| format!("{key}: bad max_length"))?
                        .min(MAX_STRING as u64) as usize,
                };
                Kind::String {
                    placeholder: text(v, "placeholder", MAX_SHORT),
                    max_length,
                }
            }
            Some("number") => {
                let (min, max) = (finite(v, "min")?, finite(v, "max")?);
                let step = finite(v, "step")?;
                if let (Some(a), Some(b)) = (min, max)
                    && a > b
                {
                    return Err(format!("{key}: min above max"));
                }
                if step.is_some_and(|s| s <= 0.0) {
                    return Err(format!("{key}: step must be positive"));
                }
                Kind::Number {
                    min,
                    max,
                    step,
                    integer: v.get("integer").and_then(Value::as_bool).unwrap_or(false),
                    unit: text(v, "unit", MAX_UNIT),
                }
            }
            Some("choice") => {
                let list = v
                    .get("options")
                    .and_then(Value::as_array)
                    .ok_or_else(|| format!("{key}: no options"))?;
                if list.is_empty() || list.len() > MAX_OPTIONS {
                    return Err(format!("{key}: 1 to {MAX_OPTIONS} options"));
                }
                let mut options: Vec<ChoiceOption> = Vec::new();
                for o in list {
                    let value = o
                        .get("value")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty() && s.len() <= MAX_SHORT)
                        .ok_or_else(|| format!("{key}: bad option"))?
                        .to_string();
                    if options.iter().any(|p| p.value == value) {
                        return Err(format!("{key}: duplicate option"));
                    }
                    let label =
                        text(o, "label", MAX_LABEL).unwrap_or_else(|| clean(&value, MAX_LABEL));
                    options.push(ChoiceOption { value, label });
                }
                Kind::Choice { options }
            }
            _ => return Err(format!("{key}: unknown type")),
        };
        let mut s = Setting {
            key,
            label,
            description: text(v, "description", MAX_DESCRIPTION),
            section: text(v, "section", MAX_SECTION),
            restart: v.get("restart").and_then(Value::as_bool).unwrap_or(false),
            default: Value::Null,
            kind,
        };
        let default = v
            .get("default")
            .ok_or_else(|| format!("{}: no default", s.key))?;
        s.default = s
            .check(default)
            .map_err(|e| format!("{}: default refused ({e})", s.key))?;
        Ok(s)
    }

    /// The value to use for `v`: numbers are snapped to `step` and rounded
    /// when `integer`, strings lose their control characters.
    pub fn check(&self, v: &Value) -> Result<Value, SettingError> {
        match &self.kind {
            Kind::Bool => v.as_bool().map(Value::Bool).ok_or(SettingError::WrongType),
            Kind::String { max_length, .. } => {
                let s = v.as_str().ok_or(SettingError::WrongType)?;
                let s: String = s
                    .chars()
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .collect();
                if s.chars().count() > *max_length {
                    return Err(SettingError::TooLong(*max_length));
                }
                Ok(Value::String(s))
            }
            Kind::Number {
                min,
                max,
                step,
                integer,
                ..
            } => {
                let mut n = v
                    .as_f64()
                    .filter(|n| n.is_finite())
                    .ok_or(SettingError::WrongType)?;
                if let Some(step) = step {
                    let base = min.unwrap_or(0.0);
                    n = base + ((n - base) / step).round() * step;
                    // 0.1 + 0.2: keep what the user typed readable.
                    n = (n * 1e9).round() / 1e9;
                }
                if *integer {
                    n = n.round();
                }
                if let Some(m) = min
                    && n < *m
                {
                    return Err(SettingError::BelowMin(*m));
                }
                if let Some(m) = max
                    && n > *m
                {
                    return Err(SettingError::AboveMax(*m));
                }
                Ok(number_value(n, *integer))
            }
            Kind::Choice { options } => {
                let s = v.as_str().ok_or(SettingError::WrongType)?;
                if options.iter().any(|o| o.value == s) {
                    Ok(Value::String(s.to_string()))
                } else {
                    Err(SettingError::NotAnOption)
                }
            }
        }
    }

    fn same(&self, a: &Value, b: &Value) -> bool {
        match (&self.kind, a.as_f64(), b.as_f64()) {
            (Kind::Number { .. }, Some(x), Some(y)) => x == y,
            _ => a == b,
        }
    }
}

/// A plugin's declaration (`settings` of `initialize`, or of
/// `settings.declared`). Bad entries are dropped with a warning, and only
/// the first 100 good ones are kept.
pub fn parse_schema(plugin: &str, v: Option<&Value>) -> Vec<Setting> {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return Vec::new();
    };
    let Some(list) = v.as_array() else {
        tracing::warn!("plugin[{plugin}]: settings is not a list, ignored");
        return Vec::new();
    };
    let mut out: Vec<Setting> = Vec::new();
    for entry in list {
        if out.len() == MAX_SETTINGS {
            tracing::warn!("plugin[{plugin}]: more than {MAX_SETTINGS} settings, the rest ignored");
            break;
        }
        match Setting::parse(entry) {
            Ok(s) if out.iter().any(|o| o.key == s.key) => {
                tracing::warn!(
                    "plugin[{plugin}]: setting {}: duplicate key, ignored",
                    s.key
                )
            }
            Ok(s) => out.push(s),
            Err(e) => tracing::warn!("plugin[{plugin}]: setting ignored: {e}"),
        }
    }
    out
}

pub fn json_to_toml(v: &Value) -> Option<toml::Value> {
    Some(match v {
        Value::Bool(b) => toml::Value::Boolean(*b),
        Value::String(s) => toml::Value::String(s.clone()),
        Value::Number(n) => match n.as_i64() {
            Some(i) => toml::Value::Integer(i),
            None => toml::Value::Float(n.as_f64()?),
        },
        _ => return None,
    })
}

pub fn toml_to_json(v: &toml::Value) -> Option<Value> {
    Some(match v {
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => Value::from(*i),
        toml::Value::Float(f) => Value::Number(serde_json::Number::from_f64(*f)?),
        _ => return None,
    })
}

/// Stored values as sent in `initialize`: every one of them, declared now
/// or not (the host does not know the schema before the plugin answers).
pub fn stored_json(stored: &Stored) -> Map<String, Value> {
    stored
        .iter()
        .filter_map(|(k, v)| Some((k.clone(), toml_to_json(v)?)))
        .collect()
}

/// The value in effect for each declared setting: the stored one when it
/// is still acceptable, else the default.
pub fn effective(schema: &[Setting], stored: &Stored) -> Map<String, Value> {
    schema
        .iter()
        .map(|s| {
            let v = stored
                .get(&s.key)
                .and_then(toml_to_json)
                .and_then(|v| s.check(&v).ok())
                .unwrap_or_else(|| s.default.clone());
            (s.key.clone(), v)
        })
        .collect()
}

/// Check `value` for `key` and store it, or forget the key when the value
/// is the default. Returns the value in effect.
pub fn store(
    schema: &[Setting],
    stored: &mut Stored,
    key: &str,
    value: &Value,
) -> Result<Value, SettingError> {
    let s = schema
        .iter()
        .find(|s| s.key == key)
        .ok_or(SettingError::UnknownKey)?;
    let v = s.check(value)?;
    if s.same(&v, &s.default) {
        stored.remove(key);
    } else {
        stored.insert(
            key.to_string(),
            json_to_toml(&v).ok_or(SettingError::WrongType)?,
        );
    }
    Ok(v)
}

/// Keys whose stored value differs between `a` and `b`.
pub fn changed_keys(a: &Stored, b: &Stored) -> Vec<String> {
    let mut keys: Vec<String> = a
        .keys()
        .chain(b.keys())
        .filter(|k| a.get(*k) != b.get(*k))
        .cloned()
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Vec<Setting> {
        parse_schema(
            "t",
            Some(&json!([
                {"key": "on", "type": "bool", "label": "On", "default": true},
                {"key": "name", "type": "string", "label": "Name", "default": "", "max_length": 5},
                {"key": "size", "type": "number", "label": "Size", "default": 50,
                 "min": 10, "max": 200, "integer": true, "unit": "items"},
                {"key": "gain", "type": "number", "label": "Gain", "default": 0.5, "step": 0.25},
                {"key": "q", "type": "choice", "label": "Quality", "default": "hi", "section": "Audio",
                 "options": [{"value": "lo", "label": "Low"}, {"value": "hi", "label": "High"}]}
            ])),
        )
    }

    #[test]
    fn declarations_are_checked() {
        let s = schema();
        assert_eq!(s.len(), 5);
        assert_eq!(s[2].default, json!(50));
        assert_eq!(s[4].section.as_deref(), Some("Audio"));
        let bad = parse_schema(
            "t",
            Some(&json!([
                {"key": "Bad Key", "type": "bool", "label": "x", "default": true},
                {"key": "k".repeat(65), "type": "bool", "label": "x", "default": true},
                {"key": "a", "type": "bool", "label": "x", "default": "yes"},
                {"key": "b", "type": "bool", "label": "", "default": true},
                {"key": "c", "type": "color", "label": "x", "default": 1},
                {"key": "d", "type": "bool", "label": "x"},
                {"key": "e", "type": "choice", "label": "x", "default": "z",
                 "options": [{"value": "y"}]},
                {"key": "f", "type": "choice", "label": "x", "default": "y", "options": []},
                {"key": "g", "type": "number", "label": "x", "default": 5, "min": 10},
                {"key": "h", "type": "number", "label": "x", "default": 5, "min": 9, "max": 1},
                {"key": "i", "type": "string", "label": "x", "default": "toolong", "max_length": 3},
                {"key": "ok", "type": "bool", "label": "Fine", "default": false},
                {"key": "ok", "type": "bool", "label": "Twice", "default": false},
                "not an object"
            ])),
        );
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].label, "Fine");
        let many: Vec<Value> = (0..150)
            .map(|i| json!({"key": format!("k{i}"), "type": "bool", "label": "x", "default": true}))
            .collect();
        assert_eq!(parse_schema("t", Some(&json!(many))).len(), MAX_SETTINGS);
        assert!(parse_schema("t", Some(&json!({"a": 1}))).is_empty());
        assert!(parse_schema("t", None).is_empty());
        let huge = parse_schema(
            "t",
            Some(
                &json!([{"key": "s", "type": "string", "label": "l\nx", "default": "", "max_length": 99999}]),
            ),
        );
        assert_eq!(huge[0].label, "l x");
        assert!(matches!(
            huge[0].kind,
            Kind::String {
                max_length: MAX_STRING,
                ..
            }
        ));
    }

    #[test]
    fn values_are_checked_and_coerced() {
        let s = schema();
        let size = &s[2];
        assert_eq!(size.check(&json!(42.6)), Ok(json!(43)));
        assert_eq!(size.check(&json!(9)), Err(SettingError::BelowMin(10.0)));
        assert_eq!(size.check(&json!(201)), Err(SettingError::AboveMax(200.0)));
        assert_eq!(size.check(&json!("50")), Err(SettingError::WrongType));
        assert_eq!(s[3].check(&json!(0.3)), Ok(json!(0.25)));
        assert_eq!(s[1].check(&json!("abcdef")), Err(SettingError::TooLong(5)));
        assert_eq!(s[1].check(&json!("a\tb")), Ok(json!("a b")));
        assert_eq!(s[4].check(&json!("mid")), Err(SettingError::NotAnOption));
        assert_eq!(s[0].check(&json!(1)), Err(SettingError::WrongType));
    }

    #[test]
    fn only_changes_from_the_default_are_stored() {
        let s = schema();
        let mut st = Stored::new();
        assert_eq!(store(&s, &mut st, "size", &json!(80.2)), Ok(json!(80)));
        assert_eq!(st.get("size"), Some(&toml::Value::Integer(80)));
        store(&s, &mut st, "gain", &json!(1.0)).unwrap();
        assert_eq!(st.get("gain"), Some(&toml::Value::Float(1.0)));
        store(&s, &mut st, "size", &json!(50)).unwrap();
        assert!(!st.contains_key("size"), "back to the default");
        assert_eq!(
            store(&s, &mut st, "nope", &json!(1)),
            Err(SettingError::UnknownKey)
        );
        assert_eq!(
            store(&s, &mut st, "size", &json!(5)),
            Err(SettingError::BelowMin(10.0))
        );

        // Hand-edited values of the wrong type fall back to the default;
        // keys no longer declared are kept but not in effect.
        st.insert("q".into(), toml::Value::Integer(3));
        st.insert("gone".into(), toml::Value::Boolean(true));
        let eff = effective(&s, &st);
        assert_eq!(eff["q"], json!("hi"));
        assert_eq!(eff["gain"], json!(1.0));
        assert_eq!(eff["size"], json!(50));
        assert!(!eff.contains_key("gone"));
        assert_eq!(stored_json(&st)["gone"], json!(true));

        let mut other = st.clone();
        other.insert("q".into(), toml::Value::String("lo".into()));
        other.remove("gone");
        assert_eq!(changed_keys(&st, &other), ["gone", "q"]);
    }
}
