//! GridCraft for Android.
//!
//! The same engine and egui interface as the desktop app (`apps/gridcraft`), compiled as a
//! `cdylib` that a GameActivity (see `android/`) loads. There is no web view: eframe draws with
//! wgpu (Vulkan, OpenGL ES fallback) straight into the activity's surface.
//!
//! What differs from the desktop app:
//!
//! * **Files.** Android hands apps `content://` URIs, not paths, and its pickers answer later, so
//!   the host uses the web build's asynchronous services: Open copies the workbook into the app's
//!   folder and delivers its bytes through the inbox; Save, Export and Download write a staging
//!   file that `Exports.kt` mirrors to the file the user named.
//! * **Input.** Fingers become mouse events in `gridcraft_ui_egui::touch` (long press = right
//!   click, two fingers scroll, one finger scrolls the sheet, pinch zooms…) plus an on-screen
//!   Shift/Ctrl/Alt bar and keyboard button. The soft keyboard opens only on a tap on text.
//! * **Clipboard.** Copy and Cut are mirrored to the system clipboard (HTML and text) and Paste
//!   reads it.
//! * **No** window geometry, control server, native print, drag and drop or title bar.
//!
//! Everything here is `cfg(target_os = "android")`; on other targets the crate is empty.
#![cfg(target_os = "android")]

mod bridge;
// The desktop app's logger (standard error → logcat, plus a log file); it only needs `log`.
#[path = "../../gridcraft/src/logging.rs"]
mod logging;

use std::sync::Arc;
use std::time::{Duration, Instant};

use android_activity::AndroidApp;
use android_activity::input::{Axis, ImeOptions, InputType, TextInputAction};
use bridge::{FileDialogRequest, take_opened};
use gridcraft_engine::Session;
use gridcraft_ui_egui::{Inbox, Services, SheetApp, UiState};

/// Entry point: android-activity calls this on its own thread, not the Java UI thread.
#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    prepare_environment(&app);
    bridge::set_loading_progress(&app, 10);
    let logger = logging::install();
    std::panic::set_hook(Box::new(|info| log::error!("internal error: {info}")));
    if let Some(logger) = logger {
        match log_dir() {
            Some(dir) => match logger.attach_dir(&dir) {
                Ok(path) => log::info!("GridCraft {} for Android, log file {}", env!("CARGO_PKG_VERSION"), path.display()),
                Err(e) => log::warn!("no log file: {e}"),
            },
            None => logger.no_file(),
        }
    }
    // Mouse wheel axes are off by default in GameActivity.
    app.enable_motion_axis(Axis::Hscroll);
    app.enable_motion_axis(Axis::Vscroll);
    bridge::set_loading_progress(&app, 30);

    let options = eframe::NativeOptions { android_app: Some(app.clone()), wgpu_options: wgpu_options(), ..Default::default() };
    let host = app.clone();
    let result = eframe::run_native(
        "GridCraft",
        options,
        Box::new(move |cc| {
            match &cc.wgpu_render_state {
                Some(rs) => log::info!("graphics: {:?}", rs.adapter.get_info()),
                None => log::info!("graphics: no wgpu device"),
            }
            bridge::set_loading_progress(&host, 50);
            bridge::set_context(cc.egui_ctx.clone());
            let mut session = Session::new();
            session.new_workbook();
            let mut sheet = SheetApp::new(session, services(&host));
            bridge::set_loading_progress(&host, 70);
            load_prefs(&mut sheet);
            bridge::set_loading_progress(&host, 85);
            recover_backup(&mut sheet);
            bridge::set_loading_progress(&host, 95);
            // Single-line text: Gboard shows ✓ Done for cell and formula editing, and no-suggestions
            // keeps it from composing words (it re-sent each word, with many Backspaces, per key).
            host.set_ime_editor_info(
                InputType::from_bits_retain(0x1 | 0x8_0000),
                TextInputAction::None,
                ImeOptions::from_bits_retain(0x4000_0000 | 0x1000_0000),
            );
            let h = host.clone();
            sheet.touch.haptic = Some(Box::new(move || bridge::haptic(&h)));
            Ok(Box::new(AndroidHost {
                app: sheet,
                android: host,
                last_copied: String::new(),
                fullscreen_applied: false,
                block_applied: false,
                loading_hidden: false,
                folder_poll: 0,
                last_backup: Instant::now(),
                last_prefs_check: Instant::now(),
                saved_prefs: Vec::new(),
            }))
        }),
    );
    if let Err(e) = result {
        log::error!("GridCraft could not start: {e}");
    }
}

/// `$HOME`, the temp folder and the settings folder must point inside the app's own storage;
/// the desktop defaults (`~/.config`, `/data/local/tmp`) don't exist or aren't writable here.
/// The settings then live in `<files>/.config/GridCraft`, the folder Android backs up.
fn prepare_environment(app: &AndroidApp) {
    let Some(data) = app.internal_data_path() else { return };
    let tmp = data.join("tmp");
    let _ = std::fs::create_dir_all(&tmp);
    // SAFETY: first thing `android_main` does, before this app spawns any thread of its own.
    unsafe {
        std::env::set_var("HOME", &data);
        std::env::set_var("TMPDIR", &tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
    }
}

/// Ask the adapter for its own texture limit (up to 16384 px) so a modest phone GPU still opens
/// the app. Vulkan first, OpenGL ES as the fallback.
fn wgpu_options() -> eframe::WgpuConfiguration {
    use eframe::egui_wgpu::WgpuSetup;
    let mut config = eframe::WgpuConfiguration::default();
    if let WgpuSetup::CreateNew(setup) = &mut config.wgpu_setup {
        setup.instance_descriptor.backends = eframe::wgpu::Backends::VULKAN | eframe::wgpu::Backends::GL;
        setup.device_descriptor = Arc::new(|adapter| {
            let limits = adapter.limits();
            eframe::wgpu::DeviceDescriptor {
                label: Some("GridCraft device"),
                required_limits: eframe::wgpu::Limits { max_texture_dimension_2d: limits.max_texture_dimension_2d.min(16384), ..limits },
                ..Default::default()
            }
        });
    }
    config
}

// ---- services --------------------------------------------------------------------------------

/// Wait (off the UI thread) for a picker's answer and hand the file's bytes to `inbox`.
fn deliver_picked(host: &AndroidApp, purpose: &str, inbox: Inbox) {
    let rx = bridge::file_dialog(host, FileDialogRequest::Open { purpose: purpose.to_owned() });
    std::thread::spawn(move || {
        match rx.recv() {
            Ok(Ok(Some(path))) => {
                let path = std::path::PathBuf::from(path);
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Book1.xlsx".into());
                match std::fs::read(&path) {
                    Ok(bytes) => inbox.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push((name, bytes)),
                    Err(e) => log::warn!("could not read {}: {e}", path.display()),
                }
                // The workbook is open in memory now; the imported copy is not needed.
                if let Some(dir) = path.parent().filter(|d| d.starts_with(imports_dir())) {
                    let _ = std::fs::remove_dir_all(dir);
                }
            }
            Ok(Ok(None)) => {}
            Ok(Err(e)) => log::warn!("file picker: {e}"),
            Err(e) => log::warn!("file picker: {e}"),
        }
        bridge::wake();
    });
}

fn imports_dir() -> std::path::PathBuf {
    std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default().join("imports")
}

fn services(host: &AndroidApp) -> Services {
    let (open, picture, save, link) = (host.clone(), host.clone(), host.clone(), host.clone());
    let (copy, paste_html, paste_text) = (host.clone(), host.clone(), host.clone());
    let inbox = Inbox::default();
    let open_inbox = inbox.clone();
    Services {
        inbox: Some(inbox),
        open_async: Some(Box::new(move || deliver_picked(&open, "document", open_inbox.clone()))),
        pick_picture_async: Some(Box::new(move |picture_inbox| deliver_picked(&picture, "picture", picture_inbox))),
        // Save As, exports and PDF: ask where, then write the bytes into the staging file the
        // system picker is mirrored from (`Exports.kt`).
        download: Some(Box::new(move |name: &str, bytes: &[u8]| {
            let rx = bridge::file_dialog(&save, FileDialogRequest::Save { name: name.to_owned() });
            let bytes = bytes.to_vec();
            std::thread::spawn(move || match rx.recv() {
                Ok(Ok(Some(path))) => {
                    if let Err(e) = std::fs::write(&path, &bytes) {
                        log::warn!("could not write {path}: {e}");
                    }
                }
                Ok(Ok(None)) => {}
                Ok(Err(e)) => log::warn!("save picker: {e}"),
                Err(e) => log::warn!("save picker: {e}"),
            });
        })),
        open_url: Some(Box::new(move |url: &str| bridge::open_url(&link, url))),
        copy_html: Some(Box::new(move |html: &str, text: &str| {
            bridge::clipboard_set_html(&copy, html, text);
            Ok(())
        })),
        paste_html: Some(Box::new(move || bridge::clipboard_get_html(&paste_html))),
        paste_text: Some(Box::new(move || bridge::clipboard_get(&paste_text))),
        ..Default::default()
    }
}

// ---- crash recovery ----------------------------------------------------------------------
// While there are unsaved changes, a copy of the workbook is kept in the app's own folder every
// few seconds. A normal exit deletes it; if the app crashes or is killed it is still there at the
// next start and reopens in a new workbook.

const BACKUP_EVERY: Duration = Duration::from_secs(15);
const PREFS_EVERY: Duration = Duration::from_secs(3);

fn config_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config").join("GridCraft"))
}

fn backup_file() -> Option<std::path::PathBuf> {
    Some(config_dir()?.join("recovery").join("backup.xlsx"))
}

fn clear_backup() {
    if let Some(f) = backup_file() {
        let _ = std::fs::remove_file(f);
    }
}

fn recover_backup(app: &mut SheetApp) {
    let Some(file) = backup_file().filter(|f| f.exists()) else { return };
    let Ok(bytes) = std::fs::read(&file) else { return };
    let b64 = gridcraft_engine::io::base64_encode(&bytes);
    // Opened without a path: Save asks where it goes, so the copy is never overwritten as if it were the user's file.
    match app.run("file.open", serde_json::json!({"name": "Recovered.xlsx", "base64": b64})) {
        Ok(_) => {
            app.after_engine();
            app.message = Some(("GridCraft".into(), "Recovered your unsaved changes from the last session. Save to keep them.".into()));
            log::info!("recovered {}", file.display());
        }
        Err(e) => log::warn!("could not recover {}: {e}", file.display()),
    }
    clear_backup();
}

impl AndroidHost {
    /// Settings are written soon after they change, not only on a clean exit, so a crash or a
    /// killed app keeps them (the touch toggles, interface size, theme, recent files…).
    fn save_prefs_if_changed(&mut self) {
        if self.last_prefs_check.elapsed() < PREFS_EVERY {
            return;
        }
        self.last_prefs_check = Instant::now();
        self.app.collect_touch_prefs();
        let Some(dir) = config_dir() else { return };
        let Ok(bytes) = serde_json::to_vec_pretty(&self.app.ui) else { return };
        if bytes == self.saved_prefs {
            return;
        }
        let _ = std::fs::create_dir_all(&dir);
        // Write beside the file and rename, so a crash mid-write never leaves half a settings file.
        let path = dir.join("ui.json");
        let tmp = dir.join("ui.json.tmp");
        if std::fs::write(&tmp, &bytes).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
            self.saved_prefs = bytes;
        }
    }

    fn backup_if_due(&mut self) {
        if self.last_backup.elapsed() < BACKUP_EVERY {
            return;
        }
        self.last_backup = Instant::now();
        // A cell being edited is not part of the workbook yet; only committed changes are copied.
        let dirty = self.app.session.active().is_some_and(|d| d.is_dirty());
        if !dirty {
            return;
        }
        let Some(file) = backup_file() else { return };
        match self.app.session.run("file.saveBytes", serde_json::json!({"format": "xlsx"})) {
            Ok(r) => {
                let bytes = r.get("base64").and_then(serde_json::Value::as_str).and_then(gridcraft_engine::io::base64_decode);
                if let Some(bytes) = bytes {
                    if let Some(dir) = file.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    let _ = std::fs::write(&file, bytes);
                }
            }
            Err(e) => log::warn!("crash-recovery copy failed: {e}"),
        }
    }
}

// ---- preferences (the desktop app's `main.rs` helpers; they are private there) ---------------

fn log_dir() -> Option<std::path::PathBuf> {
    Some(config_dir()?.join("logs"))
}

fn load_prefs(app: &mut SheetApp) {
    let Some(dir) = config_dir() else { return };
    app.ui.language = gridcraft_ui_egui::i18n::Language::system();
    app.ui.system_theme = true;
    if let Ok(b) = std::fs::read(dir.join("ui.json"))
        && let Ok(ui) = serde_json::from_slice::<UiState>(&b)
    {
        let saved = serde_json::from_slice(&b).ok().and_then(|v| gridcraft_ui_egui::i18n::saved_language(&v));
        let language = saved.unwrap_or(app.ui.language);
        app.ui = ui;
        app.ui.language = language;
    }
    if let Ok(b) = std::fs::read(dir.join("prefs.json"))
        && let Ok(p) = serde_json::from_slice(&b)
    {
        app.session.prefs = p;
    }
    app.apply_touch_prefs();
}

fn save_prefs(app: &mut SheetApp) {
    app.collect_touch_prefs();
    let Some(dir) = config_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(b) = serde_json::to_vec_pretty(&app.ui) {
        let _ = std::fs::write(dir.join("ui.json"), b);
    }
    if let Ok(b) = serde_json::to_vec_pretty(&app.session.prefs) {
        let _ = std::fs::write(dir.join("prefs.json"), b);
    }
}

// ---- the eframe app --------------------------------------------------------------------------

struct AndroidHost {
    app: SheetApp,
    android: AndroidApp,
    /// The last text mirrored to the system clipboard (so an unchanged copy isn't sent twice).
    last_copied: String,
    /// Whether Full Screen is what Android was last told.
    fullscreen_applied: bool,
    /// Whether Block Keyboard is what Android was last told.
    block_applied: bool,
    /// The startup loading bar was removed.
    loading_hidden: bool,
    /// Frames until the documents folder state is read from Android again.
    folder_poll: u32,
    /// When the crash-recovery copy was last written.
    last_backup: Instant,
    /// When preferences were last checked for changes, and the text last written.
    last_prefs_check: Instant,
    saved_prefs: Vec<u8>,
}

impl eframe::App for AndroidHost {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        // One finger on the sheet scrolls it; selecting a range is a tap, then Shift (on the bar) and a tap.
        self.app.touch.process(ctx, raw, self.app.grid.cells_rect, &[]);
        self.app.raw_input_hook(raw);
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        for path in take_opened() {
            if let Ok(bytes) = std::fs::read(&path) {
                let name = std::path::Path::new(&path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                if let Some(inbox) = &self.app.services.inbox {
                    inbox.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push((name, bytes));
                }
                let _ = std::fs::remove_file(&path);
            }
        }
        self.app.logic(ctx);
        self.backup_if_due();
        self.save_prefs_if_changed();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.ui(ui);
        let ctx = ui.ctx().clone();
        self.app.touch_frame(&ctx);
        if self.app.touch.fullscreen != self.fullscreen_applied {
            self.fullscreen_applied = self.app.touch.fullscreen;
            bridge::set_fullscreen(&self.android, self.fullscreen_applied);
        }
        if let Some(on) = self.app.touch.docs_folder_request.take() {
            bridge::set_documents_folder(&self.android, on);
            self.folder_poll = 0;
        }
        // The folder picker answers later, so ask Android now and then what the state is.
        if self.folder_poll == 0 {
            if let Some(set) = bridge::documents_folder_set(&self.android) {
                self.app.touch.docs_folder = set;
            }
            self.folder_poll = 120;
        }
        self.folder_poll -= 1;
        // Block Keyboard: the keyboard is never asked for (`Touch::filter_ime`), so there is nothing
        // to close each frame. Closing happens once, when the switch is turned on while Gboard is
        // up. The bar's KB button opens it by force.
        if std::mem::take(&mut self.app.touch.keyboard_request) {
            self.app.touch.block_keyboard = false;
            self.block_applied = false;
            bridge::force_show_soft_keyboard(&self.android);
        }
        if self.app.touch.block_keyboard != self.block_applied {
            self.block_applied = self.app.touch.block_keyboard;
            if self.block_applied && bridge::soft_keyboard_visible(&self.android) == Some(true) {
                bridge::hide_soft_keyboard(&self.android);
            }
        }
        // The first frame is drawn: take the loading bar away.
        if !self.loading_hidden {
            self.loading_hidden = true;
            bridge::set_loading_progress(&self.android, 100);
            bridge::hide_loading(&self.android);
        }
        // Copy / Cut of plain text → the system clipboard (cells go through `copy_html`).
        let copied = ctx.output(|o| {
            o.commands.iter().rev().find_map(|c| match c {
                egui::OutputCommand::CopyText(t) => Some(t.clone()),
                _ => None,
            })
        });
        if let Some(text) = copied.filter(|t| *t != self.last_copied) {
            bridge::clipboard_set(&self.android, &text);
            self.last_copied = text;
        }
    }

    fn on_exit(&mut self) {
        // A normal exit leaves nothing to recover.
        clear_backup();
        save_prefs(&mut self.app);
    }
}
