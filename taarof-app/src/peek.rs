//! In-app file "peek" overlay — a QuickLook-style read-only viewer for the
//! terminal. Given a path (from Ctrl+Click, the recent-files picker, or the
//! `peek-file` keybinding) it renders the file instantly in a floating overlay:
//! syntax-highlighted source via GtkSourceView 5, images via GdkTexture, and a
//! safe summary (with an eject-to-editor button) for binaries and oversized
//! files. Esc closes and returns focus to the terminal; `e` ejects to the
//! configured editor at the same line; `c` copies content; `y` copies the path.
//!
//! Dismissal has three independent paths on purpose, because the overlay covers
//! the pane and a user who cannot close it is stuck (EXAMPLE-140): the header close
//! button, a click on the backdrop outside the card, and Escape. Escape is bound
//! twice — here, and window-level in `lib.rs` via `dismiss_if_visible` — because
//! a controller on this widget only sees keys while the overlay subtree holds
//! focus, and clicking the sidebar or task panel takes focus away. Only the
//! window-level binding survives that, so it is the one that matters; the local
//! one is a backstop. The content shortcuts `e`/`c`/`y` stay deliberately
//! focus-scoped so they never swallow typing in the sidebar search or task panel.
//!
//! Structure mirrors `inspector.rs` (backdrop + card + focusable container +
//! Escape handling). File IO happens off the GTK main thread via
//! `gio::spawn_blocking`, matching `terminal::validate_tmux_async`; a
//! generation counter drops results from a peek that was superseded before its
//! load finished. A single overlay handle is registered in a thread-local
//! (like `TOAST_OVERLAY`) so callers invoke the free `peek_path` with no
//! window/state threading.

use sourceview5::prelude::*;

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::AppState;

/// Files larger than this are not rendered; the overlay shows a summary with an
/// eject-to-editor button instead.
const PEEK_MAX_RENDER_BYTES: u64 = 1024 * 1024;

thread_local! {
    static PEEK: RefCell<Option<PeekOverlay>> = const { RefCell::new(None) };
}

/// The file currently shown in the overlay, retained so `e`/`c`/`y` know what
/// to act on after the async load completes.
struct PeekTarget {
    path: PathBuf,
    line: u32,
    col: Option<u32>,
    /// Loaded text content, populated once a text file finishes loading so `c`
    /// can copy it without re-reading from disk.
    content: Option<String>,
}

/// Result of loading a file off-thread, applied back on the main thread.
enum PeekLoad {
    Text(String),
    Image(Vec<u8>),
    Binary,
    TooLarge(u64),
    Error(String),
}

/// Coarse classification of a file, used to pick how to render it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeekClass {
    Text,
    Image,
    Binary,
    TooLarge,
}

#[derive(Clone)]
pub struct PeekOverlay {
    pub container: gtk::CenterBox,
    title: gtk::Label,
    subtitle: gtk::Label,
    stack: gtk::Stack,
    source_view: sourceview5::View,
    source_buffer: sourceview5::Buffer,
    image_picture: gtk::Picture,
    summary_body: gtk::Box,
    state: Rc<RefCell<AppState>>,
    current: Rc<RefCell<Option<PeekTarget>>>,
    generation: Rc<Cell<u64>>,
}

/// Build the peek overlay widget tree and register it in the `PEEK`
/// thread-local. Add the returned `container` to the window's top overlay.
pub fn build_peek_overlay(state: Rc<RefCell<AppState>>) -> PeekOverlay {
    let title = gtk::Label::new(None);
    title.set_halign(gtk::Align::Start);
    title.add_css_class("peek-title");
    title.set_ellipsize(gtk::pango::EllipsizeMode::Middle);

    let subtitle = gtk::Label::new(None);
    subtitle.set_halign(gtk::Align::Start);
    subtitle.add_css_class("peek-subtitle");
    subtitle.set_ellipsize(gtk::pango::EllipsizeMode::Middle);

    let title_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    title_box.set_hexpand(true);
    title_box.append(&title);
    title_box.append(&subtitle);

    let hint = gtk::Label::new(Some("Esc close · e editor · c copy · y path"));
    hint.add_css_class("peek-hint");
    hint.set_halign(gtk::Align::End);
    hint.set_valign(gtk::Align::Start);

    let close_button = gtk::Button::from_icon_name("window-close-symbolic");
    close_button.add_css_class("peek-close");
    close_button.set_halign(gtk::Align::End);
    close_button.set_valign(gtk::Align::Start);
    close_button.set_tooltip_text(Some("Close preview (Esc)"));

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    header.add_css_class("peek-header");
    header.append(&title_box);
    header.append(&hint);
    header.append(&close_button);

    // Read-only syntax-highlighted source view.
    let source_buffer = sourceview5::Buffer::new(None);
    let source_view = sourceview5::View::with_buffer(&source_buffer);
    source_view.set_editable(false);
    source_view.set_cursor_visible(false);
    source_view.set_monospace(true);
    source_view.set_show_line_numbers(true);
    source_view.set_highlight_current_line(true);

    let source_scroll = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .child(&source_view)
        .build();

    // Scaled image view. Picture's default content-fit is Contain, so with
    // can_shrink a large image scales down to fit while preserving aspect ratio.
    let image_picture = gtk::Picture::new();
    image_picture.set_can_shrink(true);
    image_picture.set_hexpand(true);
    image_picture.set_vexpand(true);

    // Summary page for binaries / oversized files / errors.
    let summary_body = gtk::Box::new(gtk::Orientation::Vertical, 12);
    summary_body.add_css_class("peek-summary");
    summary_body.set_halign(gtk::Align::Center);
    summary_body.set_valign(gtk::Align::Center);
    summary_body.set_hexpand(true);
    summary_body.set_vexpand(true);

    let loading = gtk::Label::new(Some("Loading…"));
    loading.add_css_class("peek-loading");
    loading.set_halign(gtk::Align::Center);
    loading.set_valign(gtk::Align::Center);
    loading.set_hexpand(true);
    loading.set_vexpand(true);

    let stack = gtk::Stack::new();
    stack.set_hexpand(true);
    stack.set_vexpand(true);
    stack.add_named(&loading, Some("loading"));
    stack.add_named(&source_scroll, Some("text"));
    stack.add_named(&image_picture, Some("image"));
    stack.add_named(&summary_body, Some("summary"));

    let card = gtk::Box::new(gtk::Orientation::Vertical, 12);
    card.add_css_class("peek-card");
    card.set_margin_top(16);
    card.set_margin_bottom(16);
    card.set_margin_start(16);
    card.set_margin_end(16);
    card.set_size_request(900, 640);
    card.append(&header);
    card.append(&stack);

    let container = gtk::CenterBox::new();
    container.set_orientation(gtk::Orientation::Vertical);
    container.add_css_class("peek-overlay-backdrop");
    container.set_hexpand(true);
    container.set_vexpand(true);
    container.set_focusable(true);
    container.set_visible(false);
    container.set_center_widget(Some(&card));

    let overlay = PeekOverlay {
        container,
        title,
        subtitle,
        stack,
        source_view,
        source_buffer,
        image_picture,
        summary_body,
        state,
        current: Rc::new(RefCell::new(None)),
        generation: Rc::new(Cell::new(0)),
    };

    {
        let overlay_for_close = overlay.clone();
        close_button.connect_clicked(move |_| overlay_for_close.hide());
    }

    {
        // Click the backdrop (anywhere outside the card) to dismiss. Bound in the
        // capture phase on the backdrop itself and gated on a hit test against
        // the card, so presses that land on content propagate untouched — text
        // selection in the source view and the summary button keep working. The
        // press, not the release, decides: a selection drag that starts on the
        // card and ends past its edge must not close the overlay.
        let overlay_for_backdrop = overlay.clone();
        let card_for_backdrop = card.clone();
        let backdrop_for_hit_test = overlay.container.clone();
        let gesture = gtk::GestureClick::new();
        gesture.set_button(gdk::BUTTON_PRIMARY);
        gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
        gesture.connect_pressed(move |gesture, _n_press, x, y| {
            if press_is_on_card(
                card_for_backdrop.compute_bounds(&backdrop_for_hit_test),
                x,
                y,
            ) {
                return;
            }
            gesture.set_state(gtk::EventSequenceState::Claimed);
            overlay_for_backdrop.hide();
        });
        overlay.container.add_controller(gesture);
    }

    {
        let overlay_for_keys = overlay.clone();
        let key_ctrl = gtk::EventControllerKey::new();
        // Capture phase so the overlay's shortcuts fire even when the focusable
        // source view (a descendant) holds keyboard focus. Non-shortcut keys
        // Proceed, so arrow/Page scrolling in the source view still works.
        //
        // Escape is handled both here and window-level in `lib.rs`. The capture
        // phase runs toplevel-down, so in practice the window controller sees it
        // first and this branch is a backstop — but both call the same `hide`, so
        // whichever fires the result is identical, and Escape cannot regress into
        // the unclosable state (EXAMPLE-140) if that ordering ever shifts. Only the
        // window-level one works once focus has left the overlay.
        key_ctrl.set_propagation_phase(gtk::PropagationPhase::Capture);
        key_ctrl.connect_key_pressed(move |_ctrl, key, _code, _mods| {
            if key == gdk::Key::Escape {
                overlay_for_keys.hide();
                return glib::Propagation::Stop;
            }
            match key.to_unicode() {
                Some('e') | Some('E') => {
                    overlay_for_keys.eject();
                    glib::Propagation::Stop
                }
                Some('c') | Some('C') => {
                    overlay_for_keys.copy_content();
                    glib::Propagation::Stop
                }
                Some('y') | Some('Y') => {
                    overlay_for_keys.copy_path();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        overlay.container.add_controller(key_ctrl);
    }

    PEEK.with(|p| *p.borrow_mut() = Some(overlay.clone()));
    overlay
}

impl PeekOverlay {
    /// Hide the overlay and restore focus to the active terminal.
    fn hide(&self) {
        self.container.set_visible(false);
        if let Some(terminal) = crate::get_active_terminal(&self.state) {
            terminal.grab_focus();
        }
    }

    /// Eject the current file to the configured editor at the peeked line, then
    /// close the overlay.
    fn eject(&self) {
        let target = self
            .current
            .borrow()
            .as_ref()
            .map(|t| (t.path.clone(), t.line, t.col));
        if let Some((path, line, col)) = target {
            self.hide();
            crate::terminal::open_path_in_editor_at(&path, line, col);
        }
    }

    /// Copy the loaded file content to the clipboard.
    fn copy_content(&self) {
        let content = self
            .current
            .borrow()
            .as_ref()
            .and_then(|t| t.content.clone());
        match content {
            Some(text) => {
                if let Some(display) = gdk::Display::default() {
                    display.clipboard().set_text(&text);
                    crate::show_toast("Copied file content");
                }
            }
            None => crate::show_toast("No text content to copy"),
        }
    }

    /// Copy the current file path to the clipboard.
    fn copy_path(&self) {
        let path = self
            .current
            .borrow()
            .as_ref()
            .map(|t| t.path.to_string_lossy().into_owned());
        if let Some(path) = path {
            if let Some(display) = gdk::Display::default() {
                display.clipboard().set_text(&path);
                crate::show_toast("Copied file path");
            }
        }
    }

    /// Show the overlay for `path`, kicking off an off-thread load. A generation
    /// counter guards against a superseded load overwriting a newer peek.
    fn show_for(&self, path: PathBuf, line: u32, col: Option<u32>) {
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);

        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        self.title.set_text(&name);
        let parent = path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.subtitle.set_text(&parent);

        *self.current.borrow_mut() = Some(PeekTarget {
            path: path.clone(),
            line,
            col,
            content: None,
        });

        self.stack.set_visible_child_name("loading");
        self.container.set_visible(true);

        // Defer the focus grab: the recent-files palette synchronously refocuses
        // the terminal after its activate handler returns, so an immediate grab
        // would be stolen. An idle grab runs after that and wins.
        {
            let container = self.container.clone();
            glib::idle_add_local_once(move || {
                container.grab_focus();
            });
        }

        let load_path = path.clone();
        glib::spawn_future_local(async move {
            let load =
                gio::spawn_blocking(move || load_peek_data(&load_path, PEEK_MAX_RENDER_BYTES))
                    .await
                    .unwrap_or_else(|_| PeekLoad::Error("peek load task failed".into()));
            let overlay = PEEK.with(|p| p.borrow().clone());
            if let Some(overlay) = overlay {
                if overlay.generation.get() == generation {
                    overlay.apply_load(load);
                }
            }
        });
    }

    /// Apply an off-thread load result to the overlay on the main thread.
    fn apply_load(&self, load: PeekLoad) {
        match load {
            PeekLoad::Text(text) => {
                let path = self.current.borrow().as_ref().map(|t| t.path.clone());
                let line = self.current.borrow().as_ref().map(|t| t.line).unwrap_or(1);
                let lang = path.as_ref().and_then(|p| {
                    sourceview5::LanguageManager::default().guess_language(Some(p), None)
                });
                self.source_buffer.set_language(lang.as_ref());
                if let Some(scheme) = pick_style_scheme() {
                    self.source_buffer.set_style_scheme(Some(&scheme));
                }
                self.source_buffer.set_text(&text);
                if let Some(target) = self.current.borrow_mut().as_mut() {
                    target.content = Some(text);
                }
                self.scroll_to_line(line);
                self.stack.set_visible_child_name("text");
            }
            PeekLoad::Image(bytes) => {
                let path = self.current.borrow().as_ref().map(|t| t.path.clone());
                let glib_bytes = glib::Bytes::from_owned(bytes);
                // from_bytes decodes raster formats in-memory; SVG and anything
                // it can't decode falls back to from_filename (bytes are already
                // size-capped, so this rare path reads a small file).
                let texture = match gdk::Texture::from_bytes(&glib_bytes) {
                    Ok(texture) => Ok(texture),
                    Err(err) => match path {
                        Some(path) => gdk::Texture::from_filename(&path),
                        None => Err(err),
                    },
                };
                match texture {
                    Ok(texture) => {
                        self.image_picture.set_paintable(Some(&texture));
                        self.stack.set_visible_child_name("image");
                    }
                    Err(err) => self.show_summary(&format!("Could not display image: {err}")),
                }
            }
            PeekLoad::Binary => {
                self.show_summary("This looks like a binary file, so it is not previewed.")
            }
            PeekLoad::TooLarge(size) => self.show_summary(&format!(
                "File is {} — too large to preview (limit {}).",
                human_size(size),
                human_size(PEEK_MAX_RENDER_BYTES)
            )),
            PeekLoad::Error(message) => self.show_summary(&message),
        }
    }

    /// Populate and show the summary page with `message` and an eject button.
    fn show_summary(&self, message: &str) {
        while let Some(child) = self.summary_body.first_child() {
            self.summary_body.remove(&child);
        }

        let label = gtk::Label::new(Some(message));
        label.set_wrap(true);
        label.set_justify(gtk::Justification::Center);
        label.set_max_width_chars(48);
        label.add_css_class("peek-summary-text");
        self.summary_body.append(&label);

        let button = gtk::Button::with_label("Open in editor");
        button.add_css_class("peek-summary-button");
        button.set_halign(gtk::Align::Center);
        {
            let overlay = self.clone();
            button.connect_clicked(move |_| overlay.eject());
        }
        self.summary_body.append(&button);

        self.stack.set_visible_child_name("summary");
    }

    /// Place the cursor at `line` and scroll it into view once the view has laid
    /// out. No-op for line 1 (already at the top).
    fn scroll_to_line(&self, line: u32) {
        if line <= 1 {
            return;
        }
        let Some(iter) = self.source_buffer.iter_at_line((line - 1) as i32) else {
            return;
        };
        self.source_buffer.place_cursor(&iter);
        let mark = self.source_buffer.create_mark(None, &iter, false);
        let view = self.source_view.clone();
        glib::idle_add_local_once(move || {
            view.scroll_to_mark(&mark, 0.0, true, 0.0, 0.3);
        });
    }
}

/// Peek `path` in the overlay, jumping to `line` when known. No-op with a log
/// line if the overlay has not been registered yet (mirrors `show_toast`'s
/// fallback).
pub fn peek_path(path: &Path, line: Option<u32>) {
    let overlay = PEEK.with(|p| p.borrow().clone());
    match overlay {
        Some(overlay) => overlay.show_for(path.to_path_buf(), line.unwrap_or(1), None),
        None => eprintln!(
            "taarof: peek overlay not registered; cannot peek {}",
            path.display()
        ),
    }
}

/// Hide the overlay if it is currently showing, reporting whether it was. Lets a
/// window-level Escape handler dismiss the overlay regardless of where focus sits
/// and know whether it consumed the key.
pub fn dismiss_if_visible() -> bool {
    // Clone out of the thread-local first: `hide` borrows `AppState` and must not
    // run while the `PEEK` borrow is live (matches `peek_path`).
    let overlay = PEEK.with(|p| p.borrow().clone());
    match overlay {
        Some(overlay) if overlay.container.is_visible() => {
            overlay.hide();
            true
        }
        _ => false,
    }
}

/// Whether a backdrop press at `(x, y)` — in backdrop coordinates — landed on the
/// card. Split from the gesture so the hit test is unit-testable without a
/// display. An unallocated card (never mapped) counts as a hit, so unknown
/// geometry can never dismiss the overlay out from under a click.
fn press_is_on_card(card_bounds: Option<gtk::graphene::Rect>, x: f64, y: f64) -> bool {
    match card_bounds {
        None => true,
        Some(bounds) => bounds.contains_point(&gtk::graphene::Point::new(x as f32, y as f32)),
    }
}

/// Pick a dark GtkSourceView style scheme, falling back to the first available
/// so a future scheme rename never renders unstyled.
fn pick_style_scheme() -> Option<sourceview5::StyleScheme> {
    let manager = sourceview5::StyleSchemeManager::default();
    for id in ["Adwaita-dark", "solarized-dark", "oblivion", "classic-dark"] {
        if let Some(scheme) = manager.scheme(id) {
            return Some(scheme);
        }
    }
    manager
        .scheme_ids()
        .first()
        .and_then(|id| manager.scheme(id.as_str()))
}

/// Load a file for peeking, off the main thread. Stats size first so an
/// oversized file never gets read into memory.
fn load_peek_data(path: &Path, cap: u64) -> PeekLoad {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) => return PeekLoad::Error(format!("Could not read file metadata: {err}")),
    };
    if !metadata.is_file() {
        return PeekLoad::Error("Could not peek path: not a regular file".to_string());
    }
    let size = metadata.len();
    if size > cap {
        return PeekLoad::TooLarge(size);
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) => return PeekLoad::Error(format!("Could not read file: {err}")),
    };
    match classify(path, size, &bytes, cap) {
        PeekClass::TooLarge => PeekLoad::TooLarge(size),
        PeekClass::Image => PeekLoad::Image(bytes),
        PeekClass::Binary => PeekLoad::Binary,
        PeekClass::Text => PeekLoad::Text(String::from_utf8_lossy(&bytes).into_owned()),
    }
}

/// Whether `path`'s extension maps to an image MIME type.
fn is_image_path(path: &Path) -> bool {
    mime_guess::from_path(path)
        .first_or_octet_stream()
        .type_()
        .as_str()
        == "image"
}

/// Heuristic binary-file guard: a NUL byte in the content is the simple,
/// reliable signal that a file is not human-readable text.
fn looks_binary(head: &[u8]) -> bool {
    head.contains(&0)
}

/// Classify a file by size, extension, and a peek at its bytes.
fn classify(path: &Path, size: u64, head: &[u8], cap: u64) -> PeekClass {
    if size > cap {
        PeekClass::TooLarge
    } else if is_image_path(path) {
        PeekClass::Image
    } else if looks_binary(head) {
        PeekClass::Binary
    } else {
        PeekClass::Text
    }
}

/// Human-readable byte size (e.g. `1.5 KB`, `2.0 MB`).
fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let value = bytes as f64;
    if value >= GB {
        format!("{:.1} GB", value / GB)
    } else if value >= MB {
        format!("{:.1} MB", value / MB)
    } else if value >= KB {
        format!("{:.1} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_dir(prefix: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn image_paths_are_detected_by_extension() {
        assert!(is_image_path(Path::new("/tmp/a.png")));
        assert!(is_image_path(Path::new("/tmp/a.jpg")));
        assert!(is_image_path(Path::new("/tmp/a.jpeg")));
        assert!(is_image_path(Path::new("/tmp/a.svg")));
        assert!(!is_image_path(Path::new("/tmp/a.rs")));
        assert!(!is_image_path(Path::new("/tmp/a.md")));
        assert!(!is_image_path(Path::new("/tmp/a.txt")));
    }

    #[test]
    fn binary_detection_uses_nul_byte() {
        assert!(!looks_binary(b"hello world"));
        assert!(!looks_binary(b""));
        assert!(looks_binary(&[0x68, 0x00, 0x69]));
        assert!(looks_binary(&[0x00]));
    }

    #[test]
    fn classify_covers_each_case() {
        let cap = PEEK_MAX_RENDER_BYTES;
        // Oversized wins regardless of type.
        assert_eq!(
            classify(
                Path::new("/tmp/big.rs"),
                2 * 1024 * 1024,
                b"fn main() {}",
                cap
            ),
            PeekClass::TooLarge
        );
        // Small image by extension.
        assert_eq!(
            classify(
                Path::new("/tmp/logo.png"),
                512,
                &[0x89, 0x50, 0x4e, 0x47],
                cap
            ),
            PeekClass::Image
        );
        // Small file with a NUL byte is binary.
        assert_eq!(
            classify(Path::new("/tmp/data.bin"), 3, &[0x00, 0x01, 0x02], cap),
            PeekClass::Binary
        );
        // Small source file is text.
        assert_eq!(
            classify(Path::new("/tmp/main.rs"), 12, b"fn main() {}", cap),
            PeekClass::Text
        );
    }

    #[test]
    fn backdrop_press_hit_test_separates_card_from_backdrop() {
        // Card inset within the backdrop, as CenterBox centers it.
        let card = gtk::graphene::Rect::new(100.0, 60.0, 400.0, 300.0);

        // Inside the card: the press belongs to the content, never a dismiss.
        assert!(press_is_on_card(Some(card), 300.0, 200.0));
        assert!(press_is_on_card(Some(card), 100.0, 60.0));

        // Outside on every side: backdrop press, dismisses.
        assert!(!press_is_on_card(Some(card), 50.0, 200.0));
        assert!(!press_is_on_card(Some(card), 600.0, 200.0));
        assert!(!press_is_on_card(Some(card), 300.0, 10.0));
        assert!(!press_is_on_card(Some(card), 300.0, 500.0));

        // Unallocated card: treat as a hit so unknown geometry cannot dismiss.
        assert!(press_is_on_card(None, 0.0, 0.0));
    }

    #[test]
    fn human_size_formats_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(2 * 1024 * 1024), "2.0 MB");
    }

    #[test]
    fn load_peek_data_classifies_files() {
        let dir = unique_temp_dir("taarof-peek-test");
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let text_path = dir.join("hello.rs");
        std::fs::write(&text_path, b"fn main() {}\n").expect("write text");
        assert!(matches!(
            load_peek_data(&text_path, PEEK_MAX_RENDER_BYTES),
            PeekLoad::Text(_)
        ));

        let bin_path = dir.join("blob.dat");
        std::fs::write(&bin_path, [0x00, 0x01, 0x02, 0x03]).expect("write binary");
        assert!(matches!(
            load_peek_data(&bin_path, PEEK_MAX_RENDER_BYTES),
            PeekLoad::Binary
        ));

        let big_path = dir.join("big.txt");
        std::fs::write(&big_path, vec![b'a'; 2048]).expect("write big");
        assert!(matches!(
            load_peek_data(&big_path, 1024),
            PeekLoad::TooLarge(2048)
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_peek_data_rejects_non_regular_files_explicitly() {
        let dir = unique_temp_dir("taarof-peek-directory-test");
        std::fs::create_dir_all(&dir).expect("create temp dir");

        assert!(
            matches!(
                load_peek_data(&dir, PEEK_MAX_RENDER_BYTES),
                PeekLoad::Error(message)
                    if message == "Could not peek path: not a regular file"
            ),
            "direct callers should receive a clear non-regular-file error",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
