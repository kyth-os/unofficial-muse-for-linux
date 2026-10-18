//! Screen context + harmless desktop actions via the XDG Desktop Portal.
//!
//! One-shot portal screenshots (the portal owns the consent dialog) with an
//! in-memory preview that is discarded on Clear, and `http(s)` URL opening
//! via OpenURI. No background capture, no persistence, no polling: every
//! portal call needs an explicit button press and runs off the GTK thread
//! with epoch guards, mirroring the mail page worker pattern.
#![allow(unexpected_cfgs)]

#[cfg(not(desktop_controller_only))]
#[allow(clippy::too_many_lines)]
mod native {
    use gtk::prelude::*;
    use gtk4 as gtk;
    use muse_desktop::{BusEndpoint, DesktopError, DesktopPortal, Screenshot};
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::sync::mpsc;
    use std::time::Duration;

    pub struct DesktopPage {
        pub root: gtk::Box,
        #[cfg(test)]
        pub(super) ui: Rc<Ui>,
    }

    enum Reply {
        Shot(Result<Screenshot, DesktopError>),
        Opened(Result<(), DesktopError>),
    }

    // pub(super) so the display smoke tests can stage UI state.
    pub(super) struct Ui {
        epoch: Cell<u64>,
        // pub(crate) so the display smoke test can stage the busy state.
        pub(crate) busy: Cell<bool>,
        bus: gtk::Entry,
        capture_notice: gtk::Label,
        preview: gtk::Picture,
        url: gtk::Entry,
        open_notice: gtk::Label,
        spinner: gtk::Spinner,
        shot: RefCell<Option<Screenshot>>,
    }

    fn label(text: &str) -> gtk::Label {
        let label = gtk::Label::new(Some(text));
        label.set_xalign(0.0);
        label.set_wrap(true);
        label.set_selectable(true);
        label
    }

    fn default_bus_path() -> String {
        std::env::var("XDG_RUNTIME_DIR")
            .map(|dir| format!("{dir}/bus"))
            .unwrap_or_default()
    }

    impl Ui {
        fn set_busy(&self, busy: bool) {
            self.busy.set(busy);
            if busy {
                self.spinner.start();
            } else {
                self.spinner.stop();
            }
        }

        fn bus_endpoint(&self) -> Result<BusEndpoint, DesktopError> {
            let path = self.bus.text().trim().to_string();
            if path.is_empty() || path.len() > 1024 || path.bytes().any(|b| b == 0) {
                return Err(DesktopError::InvalidArgument(
                    "Enter the session bus socket path explicitly.".into(),
                ));
            }
            Ok(BusEndpoint::unix_socket(&path))
        }

        fn poll(&self, ui: &Rc<Ui>, rx: mpsc::Receiver<Reply>, epoch: u64) {
            let weak = Rc::downgrade(ui);
            gtk::glib::timeout_add_local(Duration::from_millis(80), move || {
                let Some(ui) = weak.upgrade() else {
                    return gtk::glib::ControlFlow::Break;
                };
                match rx.try_recv() {
                    Ok(reply) => {
                        if ui.epoch.get() != epoch {
                            return gtk::glib::ControlFlow::Break;
                        }
                        ui.set_busy(false);
                        match reply {
                            Reply::Shot(result) => ui.finish_shot(result),
                            Reply::Opened(result) => ui.finish_open(result),
                        }
                        gtk::glib::ControlFlow::Break
                    }
                    Err(mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        if ui.epoch.get() == epoch {
                            ui.set_busy(false);
                            ui.capture_notice.set_text(
                                "Portal worker vanished before replying. Nothing was captured.",
                            );
                        }
                        gtk::glib::ControlFlow::Break
                    }
                }
            });
        }

        fn finish_shot(&self, result: Result<Screenshot, DesktopError>) {
            match result {
                Ok(shot) => {
                    let bytes = shot.bytes.len();
                    match render_png(&shot.bytes) {
                        Some(texture) => {
                            self.preview.set_paintable(Some(&texture));
                            self.shot.replace(Some(shot));
                            self.capture_notice.set_text(&format!(
                                "Screenshot captured for this view only ({}×{}, {} bytes). Clear discards it; nothing is stored.",
                                self.shot.borrow().as_ref().map(|s| s.width).unwrap_or(0),
                                self.shot.borrow().as_ref().map(|s| s.height).unwrap_or(0),
                                bytes,
                            ));
                        }
                        None => {
                            self.capture_notice.set_text(&format!(
                                "Portal returned {bytes} non-PNG bytes; discarded, nothing displayed or stored."
                            ));
                        }
                    }
                }
                Err(error) => {
                    self.capture_notice
                        .set_text(&format!("Capture failed: {error} Nothing was captured."));
                }
            }
        }

        fn finish_open(&self, result: Result<(), DesktopError>) {
            match result {
                Ok(()) => {
                    self.open_notice
                        .set_text("Portal accepted the URL; the desktop handles the open.");
                }
                Err(error) => {
                    self.open_notice
                        .set_text(&format!("Open failed: {error} Nothing was opened."));
                }
            }
        }

        fn capture(&self, ui: &Rc<Ui>) {
            if self.busy.get() {
                self.capture_notice
                    .set_text("A desktop request is already running. Wait for it to finish first.");
                return;
            }
            let bus = match self.bus_endpoint() {
                Ok(bus) => bus,
                Err(error) => {
                    self.capture_notice.set_text(&error.to_string());
                    return;
                }
            };
            self.epoch.set(self.epoch.get().wrapping_add(1));
            let epoch = self.epoch.get();
            // Per-request portal token; the epoch makes it unique per call.
            let token = format!("muse_cap_{epoch}");
            self.set_busy(true);
            self.capture_notice
                .set_text("Asking the portal; approve the capture in the system dialog…");
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let result = DesktopPortal::screenshot_via(&bus, &token);
                let _ = tx.send(Reply::Shot(result));
            });
            self.poll(ui, rx, epoch);
        }

        fn open_url(&self, ui: &Rc<Ui>) {
            if self.busy.get() {
                self.open_notice
                    .set_text("A desktop request is already running. Wait for it to finish first.");
                return;
            }
            let url = self.url.text().trim().to_string();
            if url.is_empty() {
                self.open_notice.set_text("Enter an http(s) URL to open.");
                return;
            }
            let bus = match self.bus_endpoint() {
                Ok(bus) => bus,
                Err(error) => {
                    self.open_notice.set_text(&error.to_string());
                    return;
                }
            };
            self.epoch.set(self.epoch.get().wrapping_add(1));
            let epoch = self.epoch.get();
            self.set_busy(true);
            self.open_notice
                .set_text("Asking the portal to open the URL…");
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let result = DesktopPortal::open_url_via(&bus, &url);
                let _ = tx.send(Reply::Opened(result));
            });
            self.poll(ui, rx, epoch);
        }

        fn forget(&self) {
            if self.busy.get() {
                self.capture_notice.set_text(
                    "A desktop request is already running. Wait for it to finish before clearing.",
                );
                return;
            }
            self.epoch.set(self.epoch.get().wrapping_add(1));
            self.shot.replace(None);
            self.preview
                .set_paintable(Option::<&gtk::gdk::Texture>::None);
            self.url.set_text("");
            self.capture_notice
                .set_text("Screenshot discarded. Nothing is stored.");
            self.open_notice.set_text("No URL pending.");
        }
    }

    /// Decode PNG bytes into a texture for the preview. Returns `None` for
    /// non-image payloads so they are never displayed.
    fn render_png(bytes: &[u8]) -> Option<gtk::gdk::Texture> {
        let loader = gdk_pixbuf::PixbufLoader::new();
        loader.write(bytes).ok()?;
        loader.close().ok()?;
        let pixbuf = loader.pixbuf()?;
        Some(gtk::gdk::Texture::for_pixbuf(&pixbuf))
    }

    pub fn build() -> DesktopPage {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        root.add_css_class("page-content");
        root.append(&label(
            "Desktop · portal screenshots and URL opening, always explicit",
        ));

        let settings = gtk::Expander::new(Some("Session bus (XDG portal, explicit path)"));
        settings.add_css_class("settings-disclosure");
        settings.set_expanded(true);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.add_css_class("settings-content");
        content.append(&label(
            "Portal calls go to this session bus socket only. Screenshot consent is asked by the system dialog on every capture; URLs open through the portal handler.",
        ));
        let bus = gtk::Entry::new();
        bus.set_text(&default_bus_path());
        bus.set_placeholder_text(Some("$XDG_RUNTIME_DIR/bus"));
        content.append(&bus);
        settings.set_child(Some(&content));
        root.append(&settings);

        let capture_heading = label("Screen context (one shot, never background)");
        capture_heading.add_css_class("section-heading");
        root.append(&capture_heading);
        let capture_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        capture_row.add_css_class("toolbar");
        let capture = gtk::Button::with_label("Capture screenshot");
        capture.add_css_class("suggested-action");
        let clear = gtk::Button::with_label("Clear");
        capture_row.append(&capture);
        capture_row.append(&clear);
        root.append(&capture_row);
        let capture_row2 = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let spinner = gtk::Spinner::new();
        capture_row2.append(&spinner);
        let capture_notice = label("No screenshot. Capture asks the system dialog every time.");
        capture_notice.set_hexpand(true);
        capture_row2.append(&capture_notice);
        root.append(&capture_row2);
        let preview = gtk::Picture::new();
        preview.set_widget_name("desktop-shot-preview");
        preview.set_can_shrink(true);
        preview.set_hexpand(true);
        preview.set_vexpand(true);
        let preview_scroll = gtk::ScrolledWindow::new();
        preview_scroll.set_min_content_height(160);
        preview_scroll.set_child(Some(&preview));
        root.append(&preview_scroll);

        let open_heading = label("Open URL (http/https only)");
        open_heading.add_css_class("section-heading");
        root.append(&open_heading);
        let url = gtk::Entry::new();
        url.set_placeholder_text(Some("https://example.com"));
        root.append(&url);
        let open = gtk::Button::with_label("Open URL");
        root.append(&open);
        let open_notice = label("No URL pending. Only http(s) addresses are accepted.");
        root.append(&open_notice);

        let ui = Rc::new(Ui {
            epoch: Cell::new(0),
            busy: Cell::new(false),
            bus,
            capture_notice,
            preview,
            url,
            open_notice,
            spinner,
            shot: RefCell::new(None),
        });
        {
            let ui = Rc::clone(&ui);
            capture.connect_clicked(move |_| ui.capture(&ui));
        }
        {
            let ui = Rc::clone(&ui);
            open.connect_clicked(move |_| ui.open_url(&ui));
        }
        {
            let ui = Rc::clone(&ui);
            clear.connect_clicked(move |_| ui.forget());
        }

        DesktopPage {
            root,
            #[cfg(test)]
            ui,
        }
    }
}

#[cfg(not(desktop_controller_only))]
pub use native::{DesktopPage, build};

#[cfg(test)]
mod tests {
    use super::native::build;
    use gtk::prelude::*;
    use gtk4 as gtk;

    fn button(root: &gtk::Widget, name: &str) -> gtk::Button {
        let mut found = None;
        root.observe_children();
        let mut stack = vec![root.clone()];
        while let Some(widget) = stack.pop() {
            if let Ok(btn) = widget.clone().downcast::<gtk::Button>()
                && btn.label().as_deref() == Some(name)
            {
                found = Some(btn);
                break;
            }
            let mut child = widget.first_child();
            while let Some(current) = child {
                stack.push(current.clone());
                child = current.next_sibling();
            }
        }
        found.unwrap_or_else(|| panic!("button {name} not found"))
    }

    fn has_label(root: &gtk::Widget, fragment: &str) -> bool {
        let mut stack = vec![root.clone()];
        while let Some(widget) = stack.pop() {
            if let Ok(lbl) = widget.clone().downcast::<gtk::Label>()
                && lbl.text().contains(fragment)
            {
                return true;
            }
            let mut child = widget.first_child();
            while let Some(current) = child {
                stack.push(current.clone());
                child = current.next_sibling();
            }
        }
        false
    }

    #[test]
    #[ignore = "requires GTK display; run under xvfb-run --test-threads=1"]
    fn capture_refuses_while_busy_and_clear_discards() {
        gtk::init().unwrap();
        let page = build();
        page.ui.busy.set(true);
        button(page.root.upcast_ref(), "Capture screenshot").emit_clicked();
        assert!(has_label(page.root.upcast_ref(), "already running"));
        assert!(page.ui.busy.get(), "busy work must not be cancelled");
        page.ui.busy.set(false);
        button(page.root.upcast_ref(), "Clear").emit_clicked();
        assert!(has_label(page.root.upcast_ref(), "Nothing is stored"));
    }
}
