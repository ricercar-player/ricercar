//! The settings a plugin declares (docs/plugins.md, "Settings"), in a
//! dialog opened from its row on the Plugins page. Each change is checked
//! here, then saved and sent to the plugin off the UI thread by the
//! `AppContext`.

use std::rc::Rc;

use ricercar_core::plugin::RunState;
use ricercar_core::plugin::settings::{self, Kind, Setting, SettingError, Value};
use serde_json::Map;
use slint::{Model, ModelRc, VecModel};

use crate::SettingRow;
use crate::app::{Ui, with_ui};
use crate::text::{t, tf};

/// The dialog's plugin, and the declaration and values it shows.
pub struct Open {
    id: String,
    schema: Vec<Setting>,
    values: Map<String, Value>,
}

/// Row kinds (see `SettingRow` in state.slint).
const HEADING: i32 = 0;
const SWITCH: i32 = 1;
const TEXT: i32 = 2;
const NUMBER: i32 = 3;
const SEGMENTED: i32 = 4;
const LIST: i32 = 5;

/// Declaration and values in effect for plugin `id`, from the config in
/// memory (ahead of the plugin host while a change is being applied).
fn current(ui: &Ui, id: &str) -> Option<(Vec<Setting>, Map<String, Value>, bool)> {
    let st = ui.ctx.plugins.status(id)?;
    let cfg = ui.ctx.config.read().unwrap();
    let stored = &cfg.plugins.iter().find(|p| p.id == id)?.settings;
    let values = settings::effective(&st.settings, stored);
    Some((st.settings, values, st.state == RunState::Running))
}

fn number_text(v: &Value) -> String {
    match (v.as_i64(), v.as_f64()) {
        (Some(i), _) => i.to_string(),
        (None, Some(f)) => f.to_string(),
        _ => String::new(),
    }
}

/// The dialog's lines: settings without a section first, then each section
/// under its heading, in declaration order.
pub fn rows(schema: &[Setting], values: &Map<String, Value>) -> Vec<SettingRow> {
    let mut sections: Vec<Option<&str>> = vec![None];
    for s in schema {
        if !sections.contains(&s.section.as_deref()) {
            sections.push(s.section.as_deref());
        }
    }
    let mut out = Vec::new();
    for section in sections {
        let members: Vec<&Setting> = schema
            .iter()
            .filter(|s| s.section.as_deref() == section)
            .collect();
        if members.is_empty() {
            continue;
        }
        if let Some(title) = section {
            out.push(SettingRow {
                kind: HEADING,
                label: title.into(),
                ..Default::default()
            });
        }
        out.extend(members.into_iter().map(|s| row(s, values)));
    }
    out
}

fn row(s: &Setting, values: &Map<String, Value>) -> SettingRow {
    let v = values.get(&s.key).unwrap_or(&s.default);
    let mut r = SettingRow {
        key: s.key.as_str().into(),
        label: s.label.as_str().into(),
        description: s.description.clone().unwrap_or_default().into(),
        restart: s.restart,
        ..Default::default()
    };
    match &s.kind {
        Kind::Bool => {
            r.kind = SWITCH;
            r.on = v.as_bool().unwrap_or(false);
        }
        Kind::String { placeholder, .. } => {
            r.kind = TEXT;
            r.text = v.as_str().unwrap_or("").into();
            r.placeholder = placeholder.clone().unwrap_or_default().into();
        }
        Kind::Number { min, max, unit, .. } => {
            r.kind = NUMBER;
            r.text = number_text(v).into();
            r.unit = unit.clone().unwrap_or_default().into();
            r.placeholder = match (min, max) {
                (Some(a), Some(b)) => format!("{a}–{b}"),
                _ => String::new(),
            }
            .into();
        }
        Kind::Choice { options } => {
            let short = options.len() <= 3
                && options
                    .iter()
                    .map(|o| o.label.chars().count())
                    .sum::<usize>()
                    <= 24;
            r.kind = if short { SEGMENTED } else { LIST };
            let labels: Vec<slint::SharedString> =
                options.iter().map(|o| o.label.as_str().into()).collect();
            r.options = ModelRc::new(VecModel::from(labels));
            r.selected = options
                .iter()
                .position(|o| Some(o.value.as_str()) == v.as_str())
                .unwrap_or(0) as i32;
        }
    }
    r
}

/// Plain-text reason for a refused value.
pub fn error_text(e: &SettingError) -> String {
    let n = |x: f64| number_text(&serde_json::json!(x));
    match e {
        SettingError::BelowMin(m) => tf("At least {n}", &[("n", &n(*m))]),
        SettingError::AboveMax(m) => tf("At most {n}", &[("n", &n(*m))]),
        SettingError::TooLong(max) => tf("At most {n} characters", &[("n", &max.to_string())]),
        _ => t("This value is not accepted").into(),
    }
}

/// A number as typed: a decimal comma is accepted.
pub fn parse_number(text: &str) -> Option<Value> {
    let n: f64 = text.trim().replace(',', ".").parse().ok()?;
    serde_json::Number::from_f64(n)
        .filter(|_| n.is_finite())
        .map(Value::Number)
}

pub fn open(ui: &Ui, id: &str) {
    let Some((schema, values, running)) = current(ui, id) else {
        return;
    };
    if schema.is_empty() {
        return;
    }
    let name = ui
        .ctx
        .plugins
        .status(id)
        .map(|s| s.name)
        .unwrap_or_else(|| id.to_string());
    let app = ui.app();
    app.set_psettings_title(tf("Settings for {name}", &[("name", &name)]).into());
    app.set_psettings_note(note(running).into());
    app.set_psettings_rows(ModelRc::new(VecModel::from(rows(&schema, &values))));
    ui.plugins.borrow_mut().settings = Some(Open {
        id: id.to_string(),
        schema,
        values,
    });
    app.set_psettings_open(true);
}

fn note(running: bool) -> &'static str {
    if running {
        t("Changes apply at once.")
    } else {
        t("The plugin is not running: changes apply when it starts.")
    }
}

pub fn close(ui: &Ui) {
    ui.plugins.borrow_mut().settings = None;
    ui.app().set_psettings_open(false);
}

/// From the plugin status poll: follow a new declaration, values edited in
/// config.toml, and the plugin stopping.
pub fn refresh(ui: &Ui) {
    let Some(id) = ui.plugins.borrow().settings.as_ref().map(|o| o.id.clone()) else {
        return;
    };
    let Some((schema, values, running)) = current(ui, &id).filter(|c| !c.0.is_empty()) else {
        close(ui);
        return;
    };
    ui.app().set_psettings_note(note(running).into());
    let same = ui
        .plugins
        .borrow()
        .settings
        .as_ref()
        .is_some_and(|o| o.schema == schema && o.values == values);
    if same {
        return;
    }
    ui.app()
        .set_psettings_rows(ModelRc::new(VecModel::from(rows(&schema, &values))));
    ui.plugins.borrow_mut().settings = Some(Open { id, schema, values });
}

/// Show the outcome of a change on its line: the value in effect, or why
/// it was refused.
fn show(ui: &Ui, key: &str, r: Result<Value, String>) {
    let model = ui.app().get_psettings_rows();
    let Some(i) = (0..model.row_count()).find(|i| {
        model
            .row_data(*i)
            .is_some_and(|r| r.kind != HEADING && r.key == key)
    }) else {
        return;
    };
    let mut row_now = model.row_data(i).unwrap_or_default();
    match r {
        Ok(v) => {
            let mut pv = ui.plugins.borrow_mut();
            let Some(open) = pv.settings.as_mut() else {
                return;
            };
            open.values.insert(key.to_string(), v);
            if let Some(s) = open.schema.iter().find(|s| s.key == key) {
                row_now = row(s, &open.values);
            }
        }
        Err(e) => row_now.error = e.into(),
    }
    model.set_row_data(i, row_now);
}

fn apply(ui: &Ui, key: &str, value: Option<Value>) {
    let Some(id) = ui.plugins.borrow().settings.as_ref().map(|o| o.id.clone()) else {
        return;
    };
    let r = match value {
        None => Err(t("Enter a number").to_string()),
        Some(v) => ui
            .ctx
            .set_plugin_setting(&id, key, &v)
            .map_err(|e| error_text(&e)),
    };
    show(ui, key, r);
}

fn reset(ui: &Ui) {
    let Some(id) = ui.plugins.borrow().settings.as_ref().map(|o| o.id.clone()) else {
        return;
    };
    ui.ctx.reset_plugin_settings(&id);
    // Rebuilt from scratch: typed text and errors go too.
    ui.plugins.borrow_mut().settings = None;
    open(ui, &id);
}

pub fn wire(ui: &Rc<Ui>) {
    let app = ui.app();
    app.on_plugin_settings(|id| with_ui(|ui| open(ui, &id)));
    app.on_psettings_close(|| with_ui(|ui| close(ui)));
    app.on_psettings_reset(|| with_ui(|ui| reset(ui)));
    app.on_psettings_bool(|key, on| with_ui(|ui| apply(ui, &key, Some(Value::Bool(on)))));
    app.on_psettings_text(|key, text| {
        with_ui(|ui| {
            let number = ui.plugins.borrow().settings.as_ref().is_some_and(|o| {
                o.schema
                    .iter()
                    .any(|s| s.key == key.as_str() && matches!(s.kind, Kind::Number { .. }))
            });
            let v = if number {
                parse_number(&text)
            } else {
                Some(Value::String(text.to_string()))
            };
            apply(ui, &key, v);
        })
    });
    app.on_psettings_choice(|key, i| {
        with_ui(|ui| {
            let v = ui.plugins.borrow().settings.as_ref().and_then(|o| {
                match &o.schema.iter().find(|s| s.key == key.as_str())?.kind {
                    Kind::Choice { options } => options
                        .get(usize::try_from(i).ok()?)
                        .map(|o| Value::String(o.value.clone())),
                    _ => None,
                }
            });
            if v.is_some() {
                apply(ui, &key, v);
            }
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Vec<Setting> {
        settings::parse_schema(
            "t",
            Some(&json!([
                {"key": "on", "type": "bool", "label": "On", "default": true},
                {"key": "size", "type": "number", "label": "Size", "section": "B",
                 "default": 50, "min": 10, "max": 200, "integer": true, "unit": "items"},
                {"key": "q", "type": "choice", "label": "Quality", "section": "A", "default": "hi",
                 "options": [{"value": "lo", "label": "Low"}, {"value": "hi", "label": "High"}]},
                {"key": "long", "type": "choice", "label": "Long", "section": "A", "default": "a",
                 "options": [{"value": "a", "label": "A rather long label"},
                             {"value": "b", "label": "Another long label"}]},
                {"key": "name", "type": "string", "label": "Name", "section": "B", "default": "x"}
            ])),
        )
    }

    #[test]
    fn rows_are_grouped_by_section() {
        let s = schema();
        let mut values = settings::effective(&s, &Default::default());
        values.insert("q".into(), json!("lo"));
        let r = rows(&s, &values);
        let kinds: Vec<(i32, String)> = r.iter().map(|r| (r.kind, r.label.to_string())).collect();
        assert_eq!(
            kinds,
            [
                (SWITCH, "On".into()),
                (HEADING, "B".into()),
                (NUMBER, "Size".into()),
                (TEXT, "Name".into()),
                (HEADING, "A".into()),
                (SEGMENTED, "Quality".into()),
                (LIST, "Long".into()),
            ]
        );
        assert!(r[0].on);
        assert_eq!((r[2].text.as_str(), r[2].unit.as_str()), ("50", "items"));
        assert_eq!(r[5].selected, 0, "the stored value, not the default");
        assert_eq!(r[5].options.row_count(), 2);
    }

    #[test]
    fn numbers_and_errors_read_plainly() {
        assert_eq!(parse_number(" 12,5 "), Some(json!(12.5)));
        assert_eq!(parse_number("abc"), None);
        assert_eq!(parse_number("inf"), None);
        assert_eq!(error_text(&SettingError::BelowMin(10.0)), "At least 10");
        assert_eq!(error_text(&SettingError::AboveMax(0.5)), "At most 0.5");
        assert_eq!(
            error_text(&SettingError::TooLong(80)),
            "At most 80 characters"
        );
        assert_eq!(number_text(&json!(3)), "3");
    }
}
