//! Status-notifier tray icon (KDE/freedesktop SNI: waybar, Plasma, GNOME
//! with the AppIndicator extension…).

use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{MenuItem, StandardItem};

use crate::app::post;
use crate::text::t;

pub struct Tray {
    pub playing: bool,
    pub title: String,
    pub visible: bool,
}

fn icon() -> Vec<ksni::Icon> {
    let Ok(img) = image::load_from_memory(include_bytes!("../assets/tray-64.png")) else {
        return Vec::new();
    };
    let rgba = img.to_rgba8();
    // SNI wants ARGB32 in network byte order.
    let data = rgba
        .pixels()
        .flat_map(|p| [p.0[3], p.0[0], p.0[1], p.0[2]])
        .collect();
    vec![ksni::Icon {
        width: rgba.width() as i32,
        height: rgba.height() as i32,
        data,
    }]
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "ricercar".into()
    }
    fn title(&self) -> String {
        if self.title.is_empty() {
            "ricercar".into()
        } else {
            format!("ricercar — {}", self.title)
        }
    }
    fn icon_name(&self) -> String {
        "ricercar".into()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        icon()
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "ricercar".into(),
            description: self.title.clone(),
            ..Default::default()
        }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        post(crate::app::toggle_window);
    }
    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        post(|ui| ui.ctx.ctl.toggle());
    }
    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: if self.visible {
                    t("Hide window")
                } else {
                    t("Show window")
                }
                .into(),
                activate: Box::new(|_: &mut Self| post(crate::app::toggle_window)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: if self.playing { t("Pause") } else { t("Play") }.into(),
                icon_name: if self.playing {
                    "media-playback-pause"
                } else {
                    "media-playback-start"
                }
                .into(),
                activate: Box::new(|_: &mut Self| post(|ui| ui.ctx.ctl.toggle())),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: t("Next").into(),
                icon_name: "media-skip-forward".into(),
                activate: Box::new(|_: &mut Self| post(|ui| ui.ctx.ctl.next())),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: t("Previous").into(),
                icon_name: "media-skip-backward".into(),
                activate: Box::new(|_: &mut Self| post(|ui| ui.ctx.ctl.prev())),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: t("Quit").into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|_: &mut Self| {
                    let _ = slint::invoke_from_event_loop(|| {
                        let _ = slint::quit_event_loop();
                    });
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

pub fn spawn() -> Option<Handle<Tray>> {
    let tray = Tray {
        playing: false,
        title: String::new(),
        visible: true,
    };
    match tray.spawn() {
        Ok(h) => Some(h),
        Err(e) => {
            tracing::info!("no tray: {e}");
            None
        }
    }
}
