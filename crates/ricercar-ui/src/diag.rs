//! Diagnostic report for compatibility issues (Settings → About): versions,
//! display backend, audio devices and their capabilities, signal path,
//! network, settings with secrets masked, recent log lines.

use std::io::Write;
use std::path::PathBuf;

use ricercar_daemon::{diag, logging};
use slint::Model;

use crate::app::{Ui, post};
use crate::text::t;

/// Log lines included in the report.
const LOG_LINES: usize = 200;

pub fn display_backend() -> &'static str {
    if std::env::var_os("RICERCAR_SNAPSHOT").is_some() {
        "headless (snapshot)"
    } else if crate::app::is_x11() {
        "X11"
    } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        "Wayland"
    } else {
        "unknown"
    }
}

/// One report section: a title and its lines.
pub struct Section {
    pub title: &'static str,
    pub body: String,
}

/// Plain text, readable in a terminal and in a GitHub issue.
pub fn render(sections: &[Section]) -> String {
    let mut out = String::from("ricercar diagnostic report\n==========================\n");
    for s in sections {
        out.push_str(&format!("\n## {}\n\n{}\n", s.title, s.body.trim_end()));
    }
    out
}

fn devices(ui: &Ui) -> String {
    let current = ui.ctx.ctl.device_name();
    let cache = ui.saved_state.borrow().dac_caps.clone();
    let mut lines = Vec::new();
    for d in ricercar_audio::device::list_devices() {
        let mut line = format!("- {} · {} · {:?}", d.name, d.description, d.kind);
        if d.name == current {
            line.push_str(" · selected");
        }
        if let Some(c) = cache.get(&d.name) {
            line.push_str(&format!(
                "\n  accepts {} · {} · {} ch",
                crate::dac::rates_summary(&c.rates),
                c.formats.join(", "),
                crate::dac::channels_label(c)
            ));
        }
        lines.push(line);
    }
    if !lines.iter().any(|l| l.contains(" · selected")) {
        lines.push(format!("- {current} · selected"));
    }
    lines.join("\n")
}

fn signal_path(ui: &Ui) -> String {
    let app = ui.app();
    let chain = app.get_chain();
    if chain.row_count() == 0 {
        return "Nothing playing".into();
    }
    let state = |s: i32| match s {
        0 => "untouched",
        1 => "altered",
        _ => "unknown",
    };
    let mut lines: Vec<String> = chain
        .iter()
        .map(|h| format!("{}: {} ({})", h.label, h.value, state(h.state)))
        .collect();
    lines.push(format!("Bit-perfect: {}", app.get_bitperfect()));
    lines.join("\n")
}

/// Build the whole report (UI thread).
pub fn report(ui: &Ui) -> String {
    let cfg = ui.ctx.config.read().unwrap().clone();
    let log = diag::mask_emails(&logging::recent_lines(LOG_LINES).join("\n"));
    render(&[
        Section {
            title: "System",
            body: format!(
                "ricercar {}\n{}\n{}\nDisplay: {}\nLanguage: {}",
                env!("CARGO_PKG_VERSION"),
                diag::kernel(),
                diag::distribution(),
                display_backend(),
                crate::text::language(),
            ),
        },
        Section {
            title: "Audio devices",
            body: devices(ui),
        },
        Section {
            title: "Signal path",
            body: signal_path(ui),
        },
        Section {
            title: "Network",
            body: ui.ctx.network_report(),
        },
        Section {
            title: "Plugins",
            body: ui.ctx.plugins_report(),
        },
        Section {
            title: "Settings (secrets hidden)",
            body: format!("```toml\n{}```", diag::masked_config(&cfg)),
        },
        Section {
            title: "Log (last lines)",
            body: format!("```\n{log}\n```"),
        },
    ])
}

/// Hand text to a clipboard tool; false when none worked.
fn to_clipboard(text: &str) -> bool {
    let tools: &[(&str, &[&str])] = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        &[("wl-copy", &[])]
    } else {
        &[("xclip", &["-selection", "clipboard"])]
    };
    tools.iter().any(|(cmd, args)| {
        let child = std::process::Command::new(cmd)
            .args(*args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        let Ok(mut child) = child else {
            return false;
        };
        let wrote = child
            .stdin
            .take()
            .is_some_and(|mut s| s.write_all(text.as_bytes()).is_ok());
        wrote && child.wait().is_ok_and(|s| s.success())
    })
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// Copy the report to the clipboard; without a clipboard tool, write
/// `~/ricercar-report.txt` and open its folder.
pub fn copy_report(ui: &Ui) {
    let text = report(ui);
    std::thread::spawn(move || {
        if to_clipboard(&text) {
            post(|ui| ui.toast(t("Diagnostic report copied"), false));
            return;
        }
        let path = home().join("ricercar-report.txt");
        match std::fs::write(&path, &text) {
            Ok(()) => {
                let _ = std::process::Command::new("xdg-open").arg(home()).spawn();
                post(move |ui| {
                    ui.toast(
                        format!("{} {}", t("Report saved to"), path.display()),
                        false,
                    )
                });
            }
            Err(e) => post(move |ui| ui.toast(e.to_string(), true)),
        }
    });
}

pub fn open_logs() {
    let dir = logging::log_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_layout() {
        let r = render(&[
            Section {
                title: "System",
                body: "ricercar 0.3.0\nLinux 6.9\n".into(),
            },
            Section {
                title: "Network",
                body: "Status: off".into(),
            },
        ]);
        assert!(r.starts_with("ricercar diagnostic report\n"));
        assert!(
            r.contains("\n## System\n\nricercar 0.3.0\nLinux 6.9\n\n## Network\n\nStatus: off\n")
        );
    }
}
