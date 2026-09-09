use openwave_core::model::{OperationError, Result};
use openwave_runtime::paths::RuntimePaths;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
};

/// Rendering choices never replace the icon name stored in a source or preference.
#[derive(Clone)]
pub struct Icons {
    supplied: Rc<HashMap<String, Result<PathBuf>>>,
    resolved: Rc<RefCell<HashMap<String, String>>>,
    watched: Rc<Cell<bool>>,
}

impl Icons {
    pub fn new(paths: RuntimePaths) -> Self {
        let supplied = [
            "openwave",
            "openwave-white",
            "openwave-black",
            "openwave-red",
        ]
        .into_iter()
        .map(|name| {
            (
                name.to_owned(),
                paths.data_file(&format!("icons/{name}.svg")),
            )
        })
        .collect();
        let resolved = Rc::new(RefCell::new(HashMap::new()));
        Self {
            supplied: Rc::new(supplied),
            resolved,
            watched: Rc::new(Cell::new(false)),
        }
    }

    pub fn supplied_path(&self, name: &str) -> Result<PathBuf> {
        self.supplied
            .get(name.strip_suffix(".svg").unwrap_or(name))
            .cloned()
            .unwrap_or_else(|| Err(OperationError::invalid("Not supplied OpenWave artwork")))
    }

    pub fn image(&self, name: &str, size: i32) -> gtk::Image {
        let image = if let Ok(path) = self.supplied_path(name) {
            gtk::Image::from_file(path)
        } else {
            gtk::Image::from_icon_name(&self.resolve(name))
        };
        image.set_pixel_size(size);
        image
    }

    pub(crate) fn theme_path(&self, name: &str) -> String {
        self.supplied_path(name)
            .ok()
            .and_then(|path| path.parent().map(|p| p.to_string_lossy().into_owned()))
            .unwrap_or_default()
    }

    pub(crate) fn resolve(&self, name: &str) -> String {
        if let Some(chosen) = self.resolved.borrow().get(name) {
            return chosen.clone();
        }
        let Some(display) = gtk::gdk::Display::default() else {
            return name.to_owned();
        };
        let theme = gtk::IconTheme::for_display(&display);
        if !self.watched.replace(true) {
            let cache = Rc::downgrade(&self.resolved);
            theme.connect_changed(move |_| {
                if let Some(cache) = cache.upgrade() {
                    cache.borrow_mut().clear();
                }
            });
        }
        let alternatives: &[&str] = match name {
            "web-browser-symbolic" => &[
                "internet-web-browser-symbolic",
                "applications-internet-symbolic",
                "globe-symbolic",
            ],
            "input-gaming-symbolic" => &["applications-games-symbolic", "input-gamepad-symbolic"],
            "audio-x-generic-symbolic" => {
                &["multimedia-player-symbolic", "media-optical-audio-symbolic"]
            }
            "list-drag-handle-symbolic" => &["view-list-symbolic", "open-menu-symbolic"],
            "network-transmit-symbolic" => &["network-wired-symbolic", "network-connect-symbolic"],
            "preferences-desktop-multimedia-symbolic" => &[
                "multimedia-player-symbolic",
                "applications-multimedia-symbolic",
            ],
            "video-display-symbolic" => {
                &["computer-symbolic", "preferences-desktop-display-symbolic"]
            }
            _ => &[],
        };
        let chosen = if theme.has_icon(name) {
            name
        } else {
            alternatives
                .iter()
                .copied()
                .find(|candidate| theme.has_icon(candidate))
                .unwrap_or(name)
        }
        .to_owned();
        self.resolved
            .borrow_mut()
            .insert(name.to_owned(), chosen.clone());
        chosen
    }
}
