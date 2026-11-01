//! CPU / RAM / GPU in the desktop's top bar, as a StatusNotifierItem (the freedesktop
//! system-tray protocol). Ubuntu's GNOME AppIndicator extension shows the
//! `XAyatanaLabel` text next to the icon; KDE and others show it as the tooltip.
//! Right-click opens a small menu (com.canonical.dbusmenu) with Show and Quit.

use std::collections::HashMap;

use zbus::blocking::{Connection, connection};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};

use crate::MainWindow;

const PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/MenuBar";
/// Also owned while the item is up, so a second launch can find this instance and
/// bring its window back instead of starting another copy (see `activate_running`).
const APP_NAME: &str = "io.github.taskmonitor";
/// Widest label we expect, so the panel reserves a stable width and doesn't jitter.
const GUIDE: &str = "CPU 100%  RAM 99.9G  GPU 100%";

type Pixmap = (i32, i32, Vec<u8>);

struct Item {
    label: String,
    icons: Vec<Pixmap>,
    ui: slint::Weak<MainWindow>,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    #[zbus(property)]
    fn category(&self) -> &str {
        "SystemServices"
    }
    #[zbus(property)]
    fn id(&self) -> &str {
        "taskmonitor"
    }
    #[zbus(property)]
    fn title(&self) -> &str {
        "TaskMonitor"
    }
    #[zbus(property)]
    fn status(&self) -> &str {
        "Active"
    }
    #[zbus(property)]
    fn window_id(&self) -> i32 {
        0
    }
    #[zbus(property)]
    fn icon_name(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<Pixmap> {
        self.icons.clone()
    }
    #[zbus(property)]
    fn tool_tip(&self) -> (String, Vec<Pixmap>, String, String) {
        (String::new(), Vec::new(), "TaskMonitor".into(), self.label.clone())
    }
    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn menu(&self) -> ObjectPath<'static> {
        ObjectPath::from_static_str_unchecked(MENU_PATH)
    }
    #[zbus(property, name = "XAyatanaLabel")]
    fn x_ayatana_label(&self) -> &str {
        &self.label
    }
    #[zbus(property, name = "XAyatanaLabelGuide")]
    fn x_ayatana_label_guide(&self) -> &str {
        GUIDE
    }

    /// Click: bring TaskMonitor forward.
    fn activate(&self, _x: i32, _y: i32) {
        let _ = self.ui.upgrade_in_event_loop(|ui| crate::show_window(&ui));
    }
    fn secondary_activate(&self, x: i32, y: i32) {
        self.activate(x, y);
    }
    fn context_menu(&self, _x: i32, _y: i32) {}
    fn scroll(&self, _delta: i32, _orientation: &str) {}

    #[zbus(signal)]
    async fn new_tool_tip(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal, name = "XAyatanaNewLabel")]
    async fn x_ayatana_new_label(emitter: &SignalEmitter<'_>, label: &str, guide: &str) -> zbus::Result<()>;
}

const MENU_SHOW: i32 = 1;
const MENU_QUIT: i32 = 2;

type MenuNode = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

/// The right-click menu. Static, so only the calls tray hosts actually make are answered.
struct Menu {
    ui: slint::Weak<MainWindow>,
}

fn menu_props(id: i32) -> HashMap<String, OwnedValue> {
    let mut props = HashMap::new();
    let text = match id {
        MENU_SHOW => "Show TaskMonitor",
        MENU_QUIT => "Quit",
        _ => {
            props.insert("children-display".into(), Value::from("submenu").try_into().unwrap());
            return props;
        }
    };
    props.insert("label".into(), Value::from(text).try_into().unwrap());
    props
}

fn menu_node(id: i32) -> MenuNode {
    let children = if id == 0 {
        [MENU_SHOW, MENU_QUIT].map(|c| Value::from((c, menu_props(c), Vec::<OwnedValue>::new())).try_into().unwrap()).into()
    } else {
        Vec::new()
    };
    (id, menu_props(id), children)
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl Menu {
    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }
    #[zbus(property)]
    fn text_direction(&self) -> &str {
        "ltr"
    }
    #[zbus(property)]
    fn status(&self) -> &str {
        "normal"
    }
    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }

    fn get_layout(&self, parent_id: i32, _depth: i32, _props: Vec<String>) -> (u32, MenuNode) {
        (1, menu_node(parent_id))
    }
    fn get_group_properties(&self, ids: Vec<i32>, _props: Vec<String>) -> Vec<(i32, HashMap<String, OwnedValue>)> {
        ids.into_iter().map(|id| (id, menu_props(id))).collect()
    }
    fn get_property(&self, id: i32, name: &str) -> OwnedValue {
        menu_props(id).remove(name).unwrap_or_else(|| Value::from("").try_into().unwrap())
    }
    fn event(&self, id: i32, event_id: &str, _data: Value<'_>, _timestamp: u32) {
        if event_id != "clicked" {
            return;
        }
        match id {
            MENU_SHOW => {
                let _ = self.ui.upgrade_in_event_loop(|ui| crate::show_window(&ui));
            }
            MENU_QUIT => {
                let _ = slint::invoke_from_event_loop(|| {
                    let _ = slint::quit_event_loop();
                });
            }
            _ => {}
        }
    }
    fn event_group(&self, events: Vec<(i32, String, OwnedValue, u32)>) -> Vec<i32> {
        for (id, event_id, data, timestamp) in events {
            self.event(id, &event_id, data.into(), timestamp);
        }
        Vec::new()
    }
    fn about_to_show(&self, _id: i32) -> bool {
        false
    }
    fn about_to_show_group(&self, _ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        (Vec::new(), Vec::new())
    }
}

/// If TaskMonitor is already running with its top-bar item, brings that window
/// forward and returns true.
pub fn activate_running() -> bool {
    let Ok(conn) = Connection::session() else { return false };
    conn.call_method(Some(APP_NAME), PATH, Some("org.kde.StatusNotifierItem"), "Activate", &(0i32, 0i32)).is_ok()
}

/// The registered top-bar item. Dropping it removes the item from the bar.
pub struct Tray {
    conn: Connection,
    label: String,
}

impl Tray {
    /// Registers the item with the desktop's tray host. Fails with a readable message
    /// when there is none (e.g. GNOME without the AppIndicator extension).
    pub fn start(ui: slint::Weak<MainWindow>) -> Result<Self, String> {
        let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
        let item = Item { label: String::new(), icons: icons(), ui: ui.clone() };
        let conn = connection::Builder::session()
            .and_then(|b| b.name(name.as_str()))
            .and_then(|b| b.serve_at(PATH, item))
            .and_then(|b| b.serve_at(MENU_PATH, Menu { ui }))
            .and_then(|b| b.build())
            .map_err(|e| format!("couldn't connect to the desktop: {e}"))?;
        conn.call_method(
            Some("org.kde.StatusNotifierWatcher"),
            "/StatusNotifierWatcher",
            Some("org.kde.StatusNotifierWatcher"),
            "RegisterStatusNotifierItem",
            &(name.as_str(),),
        )
        .map_err(|_| {
            "your desktop has no system tray. On GNOME, install the \"AppIndicator and KStatusNotifierItem Support\" extension".to_owned()
        })?;
        // Taken only if another copy already holds it; that copy keeps answering.
        let _ = conn.request_name(APP_NAME);
        Ok(Self { conn, label: String::new() })
    }

    /// Shows new numbers; does nothing if the text hasn't changed.
    pub fn set_label(&mut self, label: String) {
        if label == self.label {
            return;
        }
        self.label = label;
        let Ok(iface) = self.conn.object_server().interface::<_, Item>(PATH) else { return };
        iface.get_mut().label = self.label.clone();
        let emitter = iface.signal_emitter();
        let _ = zbus::block_on(Item::x_ayatana_new_label(emitter, &self.label, GUIDE));
        let _ = zbus::block_on(Item::new_tool_tip(emitter));
    }
}

/// "CPU 3%  RAM 4.7G  GPU 12%"; GPU is left out when no GPU reports usage.
pub fn label(cpu: f32, ram_used: u64, gpu: Option<f32>) -> String {
    let ram = format!("RAM {:.1}G", ram_used as f64 / (1u64 << 30) as f64);
    match gpu {
        Some(g) => format!("CPU {:.0}%  {ram}  GPU {:.0}%", cpu * 100.0, g * 100.0),
        None => format!("CPU {:.0}%  {ram}", cpu * 100.0),
    }
}

/// The app icon at common panel sizes, as ARGB32 in network byte order (the
/// protocol's format), straight alpha.
fn icons() -> Vec<Pixmap> {
    let sources: [&[u8]; 3] = [
        include_bytes!("../../../packaging/icons/hicolor/22x22/apps/io.github.taskmonitor.png"),
        include_bytes!("../../../packaging/icons/hicolor/32x32/apps/io.github.taskmonitor.png"),
        include_bytes!("../../../packaging/icons/hicolor/48x48/apps/io.github.taskmonitor.png"),
    ];
    sources
        .iter()
        .filter_map(|png| resvg::tiny_skia::Pixmap::decode_png(png).ok())
        .map(|p| {
            let argb = p
                .pixels()
                .iter()
                .flat_map(|px| {
                    let c = px.demultiply();
                    [c.alpha(), c.red(), c.green(), c.blue()]
                })
                .collect();
            (p.width() as i32, p.height() as i32, argb)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_icons() {
        assert_eq!(label(0.034, 5 * (1 << 30) + (1 << 29), Some(0.12)), "CPU 3%  RAM 5.5G  GPU 12%");
        assert_eq!(label(1.0, 0, None), "CPU 100%  RAM 0.0G");
        let icons = icons();
        assert_eq!(icons.iter().map(|i| i.0).collect::<Vec<_>>(), [22, 32, 48]);
        assert!(icons.iter().all(|(w, h, d)| d.len() == (*w * *h * 4) as usize));
    }
}
