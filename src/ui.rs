use crate::profile::{AnsiPalette, EmulationType, RgbColor, TerminalProfile};
use crate::pty::{PtyEvent, PtyHandle};
use crate::terminal::{GridPoint, Osc52Event, TerminalEmulator, format_osc52_response};
use gtk4::gdk;
use gtk4::glib;
use gtk4::pango;
use gtk4::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

/// State of a single Terminal Tab
pub struct TabSession {
    pub id: u32,
    pub emulator: TerminalEmulator,
    pub pty: PtyHandle,
    pub custom_title: Option<String>,
    pub default_title: String,
    pub drawing_area: gtk4::DrawingArea,
    pub scrollbar: gtk4::Scrollbar,
    pub adjustment: gtk4::Adjustment,
    pub tab_label: gtk4::Label,
    pub root_box: gtk4::Box,
    pub cell_width: f64,
    pub cell_height: f64,
    pub is_selecting: bool,
    pub syncing_scrollbar: bool,
    pub exited: bool,
}

impl TabSession {
    pub fn effective_title(&self) -> String {
        if let Some(ref custom) = self.custom_title {
            if !custom.trim().is_empty() {
                return custom.clone();
            }
        }
        if let Some(ref osc_title) = self.emulator.title {
            if !osc_title.trim().is_empty() {
                return osc_title.clone();
            }
        }
        format!(
            "{} ({})",
            self.default_title,
            self.emulator.profile.emulation.term_env()
        )
    }

    pub fn update_tab_label(&self) {
        self.tab_label.set_text(&self.effective_title());
    }
}

/// Shared application state holding the notebook, tabs, and active profile template
pub struct AppState {
    pub window: gtk4::ApplicationWindow,
    pub notebook: gtk4::Notebook,
    pub tabs: Vec<Rc<RefCell<TabSession>>>,
    pub next_tab_id: u32,
    pub active_profile: TerminalProfile,
    pub menubar_visible: bool,
    pub menubar_box: gtk4::Box,
    pub status_label: gtk4::Label,
    pub rename_bar: gtk4::Box,
    pub rename_entry: gtk4::Entry,
    pub rename_target_tab: Option<Rc<RefCell<TabSession>>>,
    pub prefs_bar: gtk4::Box,
    pub osc52_bar: gtk4::Box,
}

impl AppState {
    pub fn current_tab(&self) -> Option<Rc<RefCell<TabSession>>> {
        let page_num = self.notebook.current_page()?;
        let page_widget = self.notebook.nth_page(Some(page_num))?;
        for tab in &self.tabs {
            if tab.borrow().root_box.upcast_ref::<gtk4::Widget>() == &page_widget {
                return Some(tab.clone());
            }
        }
        None
    }

    pub fn update_status_bar(&self) {
        if let Some(tab_rc) = self.current_tab() {
            let tab = tab_rc.borrow();
            let emu = &tab.emulator;
            let osc52_status = match (
                emu.profile.allow_osc52_write,
                emu.profile.allow_osc52_read,
            ) {
                (true, true) => "OSC 52: Read/Write",
                (true, false) => "OSC 52: Write-Only",
                (false, true) => "OSC 52: Read-Only",
                (false, false) => "OSC 52: Disabled",
            };
            let backend = gdk::Display::default()
                .map(|d| format!("{:?}", d.backend()))
                .unwrap_or_else(|| "X11/Wayland".to_string());
            self.status_label.set_text(&format!(
                "Profile: {}  |  TERM={}  |  Grid: {}x{}  |  {}  |  Display: {}",
                emu.profile.name,
                emu.profile.emulation.term_env(),
                emu.cols,
                emu.rows,
                osc52_status,
                backend
            ));
            self.window
                .set_title(Some(&format!("{} — Midnight Terminal", tab.effective_title())));
        }
    }
}

/// Measure monospace character cell dimensions using Pango
fn measure_cell_metrics(font_desc: &pango::FontDescription) -> (f64, f64) {
    let font_map = pangocairo::FontMap::default();
    let context = font_map.create_context();
    let layout = pango::Layout::new(&context);
    layout.set_font_description(Some(font_desc));
    layout.set_text("W");
    let (ink_rect, logical_rect) = layout.pixel_extents();
    let _ = ink_rect;
    let w = (logical_rect.width() as f64).max(6.0);
    let h = (logical_rect.height() as f64).max(12.0);
    (w, h)
}

/// Build Pango font description from a TerminalProfile
fn font_desc_from_profile(profile: &TerminalProfile, bold: bool, italic: bool) -> pango::FontDescription {
    let mut desc = pango::FontDescription::from_string(&format!(
        "{} {}",
        profile.font_family, profile.font_size
    ));
    if bold {
        desc.set_weight(pango::Weight::Bold);
    }
    if italic {
        desc.set_style(pango::Style::Italic);
    }
    desc
}

/// Translate GDK key press events into VT/xterm byte sequences according to active emulation profile
fn encode_key_event(
    keyval: gdk::Key,
    state: gdk::ModifierType,
    emu: &TerminalEmulator,
) -> Option<Vec<u8>> {
    let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
    let alt = state.contains(gdk::ModifierType::ALT_MASK);
    let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
    let app_cursor = emu.application_cursor_keys;

    let mut seq: Vec<u8> = match keyval {
        gdk::Key::Return | gdk::Key::KP_Enter => {
            if emu.newline_mode {
                b"\r\n".to_vec()
            } else {
                b"\r".to_vec()
            }
        }
        gdk::Key::BackSpace => {
            let mut s = emu.profile.emulation.backspace_seq().to_vec();
            if ctrl {
                s = vec![0x08];
            }
            s
        }
        gdk::Key::Tab | gdk::Key::ISO_Left_Tab => {
            if shift {
                b"\x1b[Z".to_vec()
            } else {
                b"\t".to_vec()
            }
        }
        gdk::Key::Escape => vec![0x1b],
        gdk::Key::Up | gdk::Key::KP_Up => {
            if app_cursor {
                b"\x1bOA".to_vec()
            } else {
                b"\x1b[A".to_vec()
            }
        }
        gdk::Key::Down | gdk::Key::KP_Down => {
            if app_cursor {
                b"\x1bOB".to_vec()
            } else {
                b"\x1b[B".to_vec()
            }
        }
        gdk::Key::Right | gdk::Key::KP_Right => {
            if app_cursor {
                b"\x1bOC".to_vec()
            } else {
                b"\x1b[C".to_vec()
            }
        }
        gdk::Key::Left | gdk::Key::KP_Left => {
            if app_cursor {
                b"\x1bOD".to_vec()
            } else {
                b"\x1b[D".to_vec()
            }
        }
        gdk::Key::Home | gdk::Key::KP_Home => {
            if app_cursor {
                b"\x1bOH".to_vec()
            } else {
                b"\x1b[H".to_vec()
            }
        }
        gdk::Key::End | gdk::Key::KP_End => {
            if app_cursor {
                b"\x1bOF".to_vec()
            } else {
                b"\x1b[F".to_vec()
            }
        }
        gdk::Key::Insert | gdk::Key::KP_Insert => b"\x1b[2~".to_vec(),
        gdk::Key::Delete | gdk::Key::KP_Delete => b"\x1b[3~".to_vec(),
        gdk::Key::Page_Up | gdk::Key::KP_Page_Up => b"\x1b[5~".to_vec(),
        gdk::Key::Page_Down | gdk::Key::KP_Page_Down => b"\x1b[6~".to_vec(),
        gdk::Key::F1 => {
            if matches!(emu.profile.emulation, EmulationType::Vt100) {
                b"\x1bOP".to_vec()
            } else {
                b"\x1bOP".to_vec()
            }
        }
        gdk::Key::F2 => b"\x1bOQ".to_vec(),
        gdk::Key::F3 => b"\x1bOR".to_vec(),
        gdk::Key::F4 => b"\x1bOS".to_vec(),
        gdk::Key::F5 => b"\x1b[15~".to_vec(),
        gdk::Key::F6 => b"\x1b[17~".to_vec(),
        gdk::Key::F7 => b"\x1b[18~".to_vec(),
        gdk::Key::F8 => b"\x1b[19~".to_vec(),
        gdk::Key::F9 => b"\x1b[20~".to_vec(),
        gdk::Key::F10 => b"\x1b[21~".to_vec(),
        gdk::Key::F11 => b"\x1b[23~".to_vec(),
        gdk::Key::F12 => b"\x1b[24~".to_vec(),
        _ => {
            if let Some(ch) = keyval.to_unicode() {
                if ctrl {
                    let upper = ch.to_ascii_uppercase();
                    if ('A'..='Z').contains(&upper) {
                        vec![(upper as u8) - b'A' + 1]
                    } else {
                        match ch {
                            ' ' | '@' | '2' => vec![0x00],
                            '[' | '3' => vec![0x1b],
                            '\\' | '4' => vec![0x1c],
                            ']' | '5' => vec![0x1d],
                            '^' | '6' => vec![0x1e],
                            '_' | '/' | '7' => vec![0x1f],
                            '?' | '8' => vec![0x7f],
                            _ => {
                                let mut buf = [0u8; 4];
                                ch.encode_utf8(&mut buf).as_bytes().to_vec()
                            }
                        }
                    }
                } else {
                    let mut buf = [0u8; 4];
                    ch.encode_utf8(&mut buf).as_bytes().to_vec()
                }
            } else {
                return None;
            }
        }
    };

    if alt && !seq.is_empty() {
        let mut prefixed = Vec::with_capacity(seq.len() + 1);
        prefixed.push(0x1b);
        prefixed.extend_from_slice(&seq);
        seq = prefixed;
    }

    Some(seq)
}

/// Set local clipboard text on X11/Wayland (both CLIPBOARD and/or PRIMARY selection)
fn write_system_clipboard(selection: &str, text: &str) {
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let sel = if selection.is_empty() { "c" } else { selection };
    for ch in sel.chars() {
        match ch {
            'p' | 's' => {
                display.primary_clipboard().set_text(text);
            }
            'c' | '0'..='7' | _ => {
                display.clipboard().set_text(text);
            }
        }
    }
}

/// Read local clipboard text asynchronously and send an OSC 52 reply over the PTY
fn handle_osc52_read_query(selection: String, tab_rc: Rc<RefCell<TabSession>>) {
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let use_primary = selection.contains('p') || selection.contains('s');
    let clipboard = if use_primary {
        display.primary_clipboard()
    } else {
        display.clipboard()
    };

    clipboard.read_text_async(None::<&gtk4::gio::Cancellable>, move |res| {
        let text = match res {
            Ok(Some(gs)) => gs.to_string(),
            _ => String::new(),
        };
        let payload = format_osc52_response(&selection, &text);
        let tab = tab_rc.borrow();
        let _ = tab.pty.write_all(&payload);
    });
}

/// Copy currently selected text in the active tab to the system clipboard
pub fn copy_selection_from_current_tab(app_state: &Rc<RefCell<AppState>>) {
    let state = app_state.borrow();
    if let Some(tab_rc) = state.current_tab() {
        let tab = tab_rc.borrow();
        if let Some(text) = tab.emulator.selected_text() {
            write_system_clipboard("c", &text);
        }
    }
}

/// Paste text from the system clipboard into the active tab's PTY
pub fn paste_into_current_tab(app_state: &Rc<RefCell<AppState>>, use_primary: bool) {
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let Some(tab_rc) = app_state.borrow().current_tab() else {
        return;
    };
    let clipboard = if use_primary {
        display.primary_clipboard()
    } else {
        display.clipboard()
    };

    clipboard.read_text_async(None::<&gtk4::gio::Cancellable>, move |res| {
        if let Ok(Some(gs)) = res {
            let text = gs.to_string();
            if !text.is_empty() {
                let mut tab = tab_rc.borrow_mut();
                let bytes = tab.emulator.encode_paste(&text);
                let _ = tab.pty.write_all(&bytes);
                tab.emulator.display_offset = 0;
                tab.drawing_area.queue_draw();
            }
        }
    });
}

/// Show inline banner allowing the user to rename a tab without opening a secondary top-level window
pub fn show_rename_tab_dialog(
    app_state: &Rc<RefCell<AppState>>,
    target_tab: Rc<RefCell<TabSession>>,
) {
    let current_title = target_tab
        .borrow()
        .custom_title
        .clone()
        .unwrap_or_else(|| target_tab.borrow().effective_title());

    let (rename_bar, rename_entry, prefs_bar, osc52_bar) = {
        let mut state = app_state.borrow_mut();
        state.rename_target_tab = Some(target_tab);
        (
            state.rename_bar.clone(),
            state.rename_entry.clone(),
            state.prefs_bar.clone(),
            state.osc52_bar.clone(),
        )
    };

    prefs_bar.set_visible(false);
    osc52_bar.set_visible(false);
    rename_entry.set_text(&current_title);
    rename_bar.set_visible(true);
    rename_entry.grab_focus();
}

/// Build the persistent inline Rename Tab panel
fn build_inline_rename_bar(app_state: &Rc<RefCell<AppState>>) -> (gtk4::Box, gtk4::Entry) {
    let bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    bar.set_margin_top(6);
    bar.set_margin_bottom(6);
    bar.set_margin_start(10);
    bar.set_margin_end(10);
    bar.set_visible(false);

    let label = gtk4::Label::new(Some("Tab Title:"));
    let entry = gtk4::Entry::new();
    entry.set_hexpand(true);
    entry.set_placeholder_text(Some("Enter custom tab title (leave blank to reset)"));

    let save_btn = gtk4::Button::with_label("Save");
    save_btn.add_css_class("suggested-action");
    let reset_btn = gtk4::Button::with_label("Reset");
    let cancel_btn = gtk4::Button::with_label("Close");

    bar.append(&label);
    bar.append(&entry);
    bar.append(&save_btn);
    bar.append(&reset_btn);
    bar.append(&cancel_btn);

    let apply_title = {
        let state_save = app_state.clone();
        let entry_ref = entry.clone();
        let bar_ref = bar.clone();
        Rc::new(move || {
            let text = entry_ref.text().to_string();
            let target = state_save.borrow().rename_target_tab.clone();
            if let Some(tab_rc) = target {
                let mut t = tab_rc.borrow_mut();
                if text.trim().is_empty() {
                    t.custom_title = None;
                } else {
                    t.custom_title = Some(text);
                }
                t.update_tab_label();
            }
            bar_ref.set_visible(false);
            if let Some(cur) = state_save.borrow().current_tab() {
                cur.borrow().drawing_area.grab_focus();
            }
            state_save.borrow().update_status_bar();
        })
    };

    let apply_click = apply_title.clone();
    save_btn.connect_clicked(move |_| {
        apply_click();
    });

    let apply_activate = apply_title.clone();
    entry.connect_activate(move |_| {
        apply_activate();
    });

    {
        let state_reset = app_state.clone();
        let bar_ref = bar.clone();
        reset_btn.connect_clicked(move |_| {
            let target = state_reset.borrow().rename_target_tab.clone();
            if let Some(tab_rc) = target {
                let mut t = tab_rc.borrow_mut();
                t.custom_title = None;
                t.update_tab_label();
            }
            bar_ref.set_visible(false);
            if let Some(cur) = state_reset.borrow().current_tab() {
                cur.borrow().drawing_area.grab_focus();
            }
            state_reset.borrow().update_status_bar();
        });
    }

    {
        let state_cancel = app_state.clone();
        let bar_ref = bar.clone();
        cancel_btn.connect_clicked(move |_| {
            bar_ref.set_visible(false);
            if let Some(cur) = state_cancel.borrow().current_tab() {
                cur.borrow().drawing_area.grab_focus();
            }
        });
    }

    (bar, entry)
}

/// Toggle the inline Preferences & Profiles panel
pub fn show_preferences_dialog(app_state: &Rc<RefCell<AppState>>) {
    let (prefs_bar, rename_bar, osc52_bar) = {
        let state = app_state.borrow();
        (
            state.prefs_bar.clone(),
            state.rename_bar.clone(),
            state.osc52_bar.clone(),
        )
    };
    rename_bar.set_visible(false);
    osc52_bar.set_visible(false);
    let now_visible = !prefs_bar.is_visible();
    prefs_bar.set_visible(now_visible);
    if !now_visible {
        if let Some(cur) = app_state.borrow().current_tab() {
            cur.borrow().drawing_area.grab_focus();
        }
    }
}

/// Build the persistent inline Preferences & Profile Configuration panel
/// Uses zero popup/popover widgets (all inline toggle buttons) to avoid GTK 4.24 secondary surface hangs/crashes
fn build_inline_preferences_bar(app_state: &Rc<RefCell<AppState>>) -> gtk4::Box {
    let current_profile = app_state.borrow().active_profile.clone();

    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    vbox.set_margin_top(8);
    vbox.set_margin_bottom(8);
    vbox.set_margin_start(12);
    vbox.set_margin_end(12);
    vbox.set_visible(false);

    let selected_emu = Rc::new(RefCell::new(current_profile.emulation));

    // Row 0: Emulation Type ($TERM) inline radio buttons
    let emu_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let emu_label = gtk4::Label::builder()
        .label("Emulation ($TERM):")
        .halign(gtk4::Align::Start)
        .build();
    emu_row.append(&emu_label);

    let mut first_emu_btn: Option<gtk4::CheckButton> = None;
    for emu in EmulationType::all() {
        let e = *emu;
        let btn = gtk4::CheckButton::with_label(e.term_env());
        if let Some(ref grp) = first_emu_btn {
            btn.set_group(Some(grp));
        } else {
            first_emu_btn = Some(btn.clone());
        }
        if e == current_profile.emulation {
            btn.set_active(true);
        }
        let sel_ref = selected_emu.clone();
        btn.connect_toggled(move |b| {
            if b.is_active() {
                *sel_ref.borrow_mut() = e;
            }
        });
        emu_row.append(&btn);
    }
    vbox.append(&emu_row);

    let grid = gtk4::Grid::builder()
        .row_spacing(6)
        .column_spacing(10)
        .build();

    // Row 1: Font Family, Font Size, Scrollback
    let font_label = gtk4::Label::builder()
        .label("Font Family:")
        .halign(gtk4::Align::Start)
        .build();
    let font_entry = gtk4::Entry::new();
    font_entry.set_text(&current_profile.font_family);
    grid.attach(&font_label, 0, 0, 1, 1);
    grid.attach(&font_entry, 1, 0, 1, 1);

    let size_label = gtk4::Label::builder()
        .label("Font Size (pt):")
        .halign(gtk4::Align::Start)
        .build();
    let size_spin = gtk4::SpinButton::with_range(6.0, 48.0, 1.0);
    size_spin.set_value(current_profile.font_size);
    grid.attach(&size_label, 2, 0, 1, 1);
    grid.attach(&size_spin, 3, 0, 1, 1);

    // Row 2: Foreground, Background
    let fg_label = gtk4::Label::builder()
        .label("Foreground (#RRGGBB):")
        .halign(gtk4::Align::Start)
        .build();
    let fg_entry = gtk4::Entry::new();
    fg_entry.set_text(&current_profile.foreground.to_hex());
    grid.attach(&fg_label, 0, 1, 1, 1);
    grid.attach(&fg_entry, 1, 1, 1, 1);

    let bg_label = gtk4::Label::builder()
        .label("Background (#RRGGBB):")
        .halign(gtk4::Align::Start)
        .build();
    let bg_entry = gtk4::Entry::new();
    bg_entry.set_text(&current_profile.background.to_hex());
    grid.attach(&bg_label, 2, 1, 1, 1);
    grid.attach(&bg_entry, 3, 1, 1, 1);

    // Row 3: Scrollback & OSC 52 toggles
    let sb_label = gtk4::Label::builder()
        .label("Scrollback Lines:")
        .halign(gtk4::Align::Start)
        .build();
    let sb_spin = gtk4::SpinButton::with_range(100.0, 100_000.0, 500.0);
    sb_spin.set_value(current_profile.scrollback_lines as f64);
    grid.attach(&sb_label, 0, 2, 1, 1);
    grid.attach(&sb_spin, 1, 2, 1, 1);

    let osc_write_chk = gtk4::CheckButton::with_label("OSC 52 Remote Copy (Write)");
    osc_write_chk.set_active(current_profile.allow_osc52_write);
    grid.attach(&osc_write_chk, 2, 2, 1, 1);

    let osc_read_chk = gtk4::CheckButton::with_label("OSC 52 Remote Paste (Query)");
    osc_read_chk.set_active(current_profile.allow_osc52_read);
    grid.attach(&osc_read_chk, 3, 2, 1, 1);

    // Row 4: ANSI palette override + readability hint
    let palette_label = gtk4::Label::builder()
        .label("ANSI Palette:")
        .halign(gtk4::Align::Start)
        .build();
    let palette_names: Vec<&str> = AnsiPalette::all().iter().map(|p| p.label()).collect();
    let palette_dropdown = gtk4::DropDown::from_strings(&palette_names);
    let current_palette_idx = AnsiPalette::all()
        .iter()
        .position(|p| *p == current_profile.ansi_palette)
        .unwrap_or(0);
    palette_dropdown.set_selected(current_palette_idx as u32);
    let palette_hint = gtk4::Label::builder()
        .halign(gtk4::Align::Start)
        .wrap(true)
        .build();
    palette_hint.add_css_class("dim-label");
    grid.attach(&palette_label, 0, 3, 1, 1);
    grid.attach(&palette_dropdown, 1, 3, 1, 1);
    grid.attach(&palette_hint, 2, 3, 2, 1);

    vbox.append(&grid);

    // The last preset clicked; Apply takes its cursor color and theme palette, since only the
    // foreground and background have editable fields.
    let chosen_preset: Rc<RefCell<Option<TerminalProfile>>> = Rc::new(RefCell::new(None));

    // Show which ANSI colors would be hard to read with the chosen palette and background.
    let update_palette_hint: Rc<dyn Fn()> = {
        let state = app_state.clone();
        let bg_e = bg_entry.clone();
        let dropdown = palette_dropdown.clone();
        let chosen = chosen_preset.clone();
        let hint = palette_hint.clone();
        Rc::new(move || {
            const NAMES: [&str; 16] = [
                "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
                "bright black", "bright red", "bright green", "bright yellow", "bright blue",
                "bright magenta", "bright cyan", "bright white",
            ];
            let Ok(st) = state.try_borrow() else {
                return;
            };
            let mut preview = st.active_profile.clone();
            drop(st);
            if let Some(ref preset) = *chosen.borrow() {
                preview.palette = preset.palette;
            }
            if let Some(bg) = RgbColor::from_hex(&bg_e.text()) {
                preview.background = bg;
            }
            preview.ansi_palette = AnsiPalette::all()
                .get(dropdown.selected() as usize)
                .copied()
                .unwrap_or_default();
            let low = preview.low_contrast_colors();
            if low.is_empty() {
                hint.set_text("");
            } else {
                let names: Vec<&str> = low.iter().map(|&i| NAMES[i]).collect();
                hint.set_text(&format!("Hard to read on this background: {}", names.join(", ")));
            }
        })
    };
    {
        let f = update_palette_hint.clone();
        palette_dropdown.connect_selected_notify(move |_| f());
        let f = update_palette_hint.clone();
        bg_entry.connect_changed(move |_| f());
    }
    update_palette_hint();

    // Theme preset quick-fill buttons (inline, zero popups)
    let preset_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    preset_row.append(&gtk4::Label::new(Some("Color Presets:")));
    let presets: &[(&str, fn(EmulationType) -> TerminalProfile)] = &[
        ("Midnight Dark", TerminalProfile::midnight_dark),
        ("Solarized Dark", TerminalProfile::solarized_dark),
        ("Daylight Light", TerminalProfile::light_theme),
        ("VT100 Green", |_| TerminalProfile::green_phosphor_vt100()),
        ("Amber", TerminalProfile::amber),
        ("Novel", TerminalProfile::novel),
        ("Ocean", TerminalProfile::ocean),
    ];
    for (label, builder_fn) in presets {
        let p_btn = gtk4::Button::with_label(label);
        let fg_e = fg_entry.clone();
        let bg_e = bg_entry.clone();
        let sel_ref = selected_emu.clone();
        let chosen = chosen_preset.clone();
        let refresh_hint = update_palette_hint.clone();
        let f = *builder_fn;
        p_btn.connect_clicked(move |_| {
            let p = f(*sel_ref.borrow());
            let (fg, bg) = (p.foreground.to_hex(), p.background.to_hex());
            *chosen.borrow_mut() = Some(p);
            fg_e.set_text(&fg);
            bg_e.set_text(&bg);
            refresh_hint();
        });
        preset_row.append(&p_btn);
    }
    vbox.append(&preset_row);

    let btn_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    btn_box.set_halign(gtk4::Align::End);
    let cancel_btn = gtk4::Button::with_label("Close");
    let apply_btn = gtk4::Button::with_label("Apply & Save Profile");
    apply_btn.add_css_class("suggested-action");
    btn_box.append(&cancel_btn);
    btn_box.append(&apply_btn);
    vbox.append(&btn_box);

    let bar_cancel = vbox.clone();
    let state_cancel = app_state.clone();
    cancel_btn.connect_clicked(move |_| {
        bar_cancel.set_visible(false);
        if let Some(cur) = state_cancel.borrow().current_tab() {
            cur.borrow().drawing_area.grab_focus();
        }
    });

    let bar_apply = vbox.clone();
    let state_apply = app_state.clone();
    apply_btn.connect_clicked(move |_| {
        let emu = *selected_emu.borrow();
        let mut updated = state_apply.borrow().active_profile.clone();
        updated.emulation = emu;
        updated.name = format!("Profile ({})", emu.term_env());
        updated.font_family = font_entry.text().to_string();
        updated.font_size = size_spin.value();
        if let Some(fg) = RgbColor::from_hex(&fg_entry.text()) {
            updated.foreground = fg;
        }
        if let Some(bg) = RgbColor::from_hex(&bg_entry.text()) {
            updated.background = bg;
        }
        if let Some(preset) = chosen_preset.borrow_mut().take() {
            updated.cursor_color = preset.cursor_color;
            updated.palette = preset.palette;
        }
        updated.ansi_palette = AnsiPalette::all()
            .get(palette_dropdown.selected() as usize)
            .copied()
            .unwrap_or_default();
        updated.scrollback_lines = sb_spin.value() as usize;
        updated.allow_osc52_write = osc_write_chk.is_active();
        updated.allow_osc52_read = osc_read_chk.is_active();

        let _ = updated.save();
        apply_profile_to_current_tab(&state_apply, updated);
        bar_apply.set_visible(false);
        if let Some(cur) = state_apply.borrow().current_tab() {
            cur.borrow().drawing_area.grab_focus();
        }
    });

    vbox
}

/// Apply a TerminalProfile to the currently active tab and save as default for new tabs
pub fn apply_profile_to_current_tab(
    app_state: &Rc<RefCell<AppState>>,
    profile: TerminalProfile,
) {
    {
        let mut state = app_state.borrow_mut();
        state.active_profile = profile.clone();
        if let Some(tab_rc) = state.current_tab() {
            let mut tab = tab_rc.borrow_mut();
            let font_desc = font_desc_from_profile(&profile, false, false);
            let (cw, ch) = measure_cell_metrics(&font_desc);
            tab.cell_width = cw;
            tab.cell_height = ch;
            tab.emulator.set_profile(profile);
            tab.update_tab_label();
            tab.drawing_area.queue_draw();
        }
    }
    app_state.borrow().update_status_bar();
}

/// Toggle the inline OSC 52 Remote Copy/Paste helper bar
pub fn show_osc52_helper_dialog(app_state: &Rc<RefCell<AppState>>) {
    let (osc52_bar, rename_bar, prefs_bar) = {
        let state = app_state.borrow();
        (
            state.osc52_bar.clone(),
            state.rename_bar.clone(),
            state.prefs_bar.clone(),
        )
    };
    rename_bar.set_visible(false);
    prefs_bar.set_visible(false);
    let now_visible = !osc52_bar.is_visible();
    osc52_bar.set_visible(now_visible);
    if !now_visible {
        if let Some(cur) = app_state.borrow().current_tab() {
            cur.borrow().drawing_area.grab_focus();
        }
    }
}

/// Build the persistent inline OSC 52 helper bar
fn build_inline_osc52_bar(app_state: &Rc<RefCell<AppState>>) -> gtk4::Box {
    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    vbox.set_margin_top(8);
    vbox.set_margin_bottom(8);
    vbox.set_margin_start(12);
    vbox.set_margin_end(12);
    vbox.set_visible(false);

    let info = gtk4::Label::builder()
        .label(
            "OSC 52 Remote Copy/Paste Protocol:\n\
            • Remote Copy (Write): printf '\\033]52;c;%s\\a' \"$(printf 'Hello from remote' | base64)\"\n\
            • Remote Paste (Query): printf '\\033]52;c;?\\a'",
        )
        .halign(gtk4::Align::Start)
        .selectable(true)
        .build();
    vbox.append(&info);

    let btn_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    btn_box.set_halign(gtk4::Align::End);

    let test_btn = gtk4::Button::with_label("Run Live OSC 52 Copy Test in Current Tab");
    let close_btn = gtk4::Button::with_label("Close");
    btn_box.append(&test_btn);
    btn_box.append(&close_btn);
    vbox.append(&btn_box);

    let bar_close = vbox.clone();
    let state_close = app_state.clone();
    close_btn.connect_clicked(move |_| {
        bar_close.set_visible(false);
        if let Some(cur) = state_close.borrow().current_tab() {
            cur.borrow().drawing_area.grab_focus();
        }
    });

    let state_test = app_state.clone();
    let bar_test = vbox.clone();
    test_btn.connect_clicked(move |_| {
        if let Some(tab_rc) = state_test.borrow().current_tab() {
            let tab = tab_rc.borrow();
            let cmd = b"printf '\\033]52;c;%s\\a' \"$(printf 'OSC 52 Remote Clipboard Verified!' | base64)\" && echo 'Sent OSC 52 clipboard payload!'\r";
            let _ = tab.pty.write_all(cmd);
        }
        bar_test.set_visible(false);
        if let Some(cur) = state_test.borrow().current_tab() {
            cur.borrow().drawing_area.grab_focus();
        }
    });

    vbox
}

/// Spawn and attach a new Terminal Tab inside the Notebook
pub fn create_new_tab(app_state: &Rc<RefCell<AppState>>, profile_override: Option<TerminalProfile>) {
    let (tab_id, profile) = {
        let mut state = app_state.borrow_mut();
        let id = state.next_tab_id;
        state.next_tab_id += 1;
        let prof = profile_override.unwrap_or_else(|| state.active_profile.clone());
        (id, prof)
    };

    let initial_cols = 80usize;
    let initial_rows = 24usize;
    let font_desc = font_desc_from_profile(&profile, false, false);
    let (cell_width, cell_height) = measure_cell_metrics(&font_desc);

    let pty = match PtyHandle::spawn(
        initial_cols as u16,
        initial_rows as u16,
        profile.emulation,
        None,
    ) {
        Ok(p) => p,
        Err(err) => {
            eprintln!("Failed to spawn PTY: {}", err);
            return;
        }
    };

    let emulator = TerminalEmulator::new(initial_cols, initial_rows, profile);

    let drawing_area = gtk4::DrawingArea::builder()
        .hexpand(true)
        .vexpand(true)
        .focusable(true)
        .can_focus(true)
        .build();

    let adjustment = gtk4::Adjustment::new(0.0, 0.0, 24.0, 1.0, 10.0, 24.0);
    let scrollbar = gtk4::Scrollbar::new(gtk4::Orientation::Vertical, Some(&adjustment));

    let root_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    root_box.append(&drawing_area);
    root_box.append(&scrollbar);

    // Tab Header widget: title label + rename button + close button
    let tab_header = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let default_title = format!("Terminal {}", tab_id);
    let tab_label = gtk4::Label::new(Some(&default_title));
    // An ellipsizing label's minimum width is just "…", and the scrollable notebook shrinks
    // tabs to their minimum, so reserve room for a readable title.
    tab_label.set_ellipsize(pango::EllipsizeMode::End);
    tab_label.set_width_chars(28);
    tab_label.set_max_width_chars(40);

    let rename_btn = gtk4::Button::builder()
        .icon_name("document-edit-symbolic")
        .has_frame(false)
        .build();

    let close_btn = gtk4::Button::builder()
        .icon_name("window-close-symbolic")
        .has_frame(false)
        .build();

    tab_header.append(&tab_label);
    tab_header.append(&rename_btn);
    tab_header.append(&close_btn);

    let session = Rc::new(RefCell::new(TabSession {
        id: tab_id,
        emulator,
        pty,
        custom_title: None,
        default_title,
        drawing_area: drawing_area.clone(),
        scrollbar: scrollbar.clone(),
        adjustment: adjustment.clone(),
        tab_label: tab_label.clone(),
        root_box: root_box.clone(),
        cell_width,
        cell_height,
        is_selecting: false,
        syncing_scrollbar: false,
        exited: false,
    }));

    session.borrow().update_tab_label();

    // Add to notebook (clone notebook reference before dropping borrow_mut so switch_page signal doesn't re-entrantly borrow AppState)
    let notebook = {
        let mut state = app_state.borrow_mut();
        state.tabs.push(session.clone());
        state.notebook.clone()
    };
    let page_num = notebook.append_page(&root_box, Some(&tab_header));
    notebook.set_tab_reorderable(&root_box, true);
    notebook.set_current_page(Some(page_num));
    let st_init = app_state.clone();
    glib::idle_add_local_once(move || {
        if let Ok(st) = st_init.try_borrow() {
            st.update_status_bar();
        }
    });

    // Double-click on tab header or click edit icon opens rename tab dialog
    {
        let state_ref = app_state.clone();
        let tab_ref = session.clone();
        rename_btn.connect_clicked(move |_| {
            show_rename_tab_dialog(&state_ref, tab_ref.clone());
        });
    }
    {
        let gesture = gtk4::GestureClick::new();
        let state_ref = app_state.clone();
        let tab_ref = session.clone();
        gesture.connect_pressed(move |_, n_press, _, _| {
            if n_press == 2 {
                show_rename_tab_dialog(&state_ref, tab_ref.clone());
            }
        });
        tab_header.add_controller(gesture);
    }

    // Close button on tab header
    {
        let state_ref = app_state.clone();
        let tab_ref = session.clone();
        close_btn.connect_clicked(move |_| {
            close_tab(&state_ref, &tab_ref);
        });
    }

    // Cairo Draw Function for rendering the terminal grid, Unicode glyphs, selection, and cursor
    {
        let tab_draw = session.clone();
        drawing_area.set_draw_func(move |_area, cr, width, height| {
            let tab = tab_draw.borrow();
            let emu = &tab.emulator;
            let profile = &emu.profile;
            let cw = tab.cell_width;
            let ch = tab.cell_height;

            // Fill terminal background
            let (bg_r, bg_g, bg_b) = profile.background.to_f64_tuple();
            cr.set_source_rgb(bg_r, bg_g, bg_b);
            let _ = cr.paint();

            let layout = pangocairo::functions::create_layout(cr);
            let font_normal = font_desc_from_profile(profile, false, false);
            let font_bold = font_desc_from_profile(profile, true, false);
            let font_italic = font_desc_from_profile(profile, false, true);
            let font_bold_italic = font_desc_from_profile(profile, true, true);

            for view_row in 0..emu.rows {
                let y = view_row as f64 * ch;
                if y >= height as f64 {
                    break;
                }
                let (row, abs_line) = emu.visible_row(view_row);

                // First pass: cell backgrounds & selection highlight
                for col in 0..emu.cols.min(row.cells.len()) {
                    let cell = &row.cells[col];
                    if cell.width == 0 {
                        continue;
                    }
                    let x = col as f64 * cw;
                    if x >= width as f64 {
                        break;
                    }
                    let cell_pixel_w = (cell.width.max(1) as f64) * cw;

                    let is_selected = emu.is_cell_selected(abs_line, col);
                    let mut fg_rgb = cell.attrs.fg.resolve(profile, cell.attrs.bold);
                    let mut bg_rgb = cell.attrs.bg.resolve(profile, false);

                    if cell.attrs.inverse ^ is_selected {
                        std::mem::swap(&mut fg_rgb, &mut bg_rgb);
                    }

                    if bg_rgb != profile.background || is_selected || cell.attrs.inverse {
                        let (r, g, b) = bg_rgb.to_f64_tuple();
                        cr.set_source_rgb(r, g, b);
                        cr.rectangle(x, y, cell_pixel_w.ceil(), ch.ceil());
                        let _ = cr.fill();
                    }
                }

                // Second pass: foreground glyphs and underlines/strikethroughs
                for col in 0..emu.cols.min(row.cells.len()) {
                    let cell = &row.cells[col];
                    if cell.width == 0 || cell.attrs.hidden {
                        continue;
                    }
                    let x = col as f64 * cw;
                    if x >= width as f64 {
                        break;
                    }
                    let cell_pixel_w = (cell.width.max(1) as f64) * cw;
                    let is_selected = emu.is_cell_selected(abs_line, col);

                    let mut fg_rgb = cell.attrs.fg.resolve(profile, cell.attrs.bold);
                    let mut bg_rgb = cell.attrs.bg.resolve(profile, false);
                    if cell.attrs.inverse ^ is_selected {
                        std::mem::swap(&mut fg_rgb, &mut bg_rgb);
                    }

                    let (mut r, mut g, mut b) = fg_rgb.to_f64_tuple();
                    if cell.attrs.dim {
                        r *= 0.65;
                        g *= 0.65;
                        b *= 0.65;
                    }

                    if cell.ch != ' ' || !cell.combining.is_empty() {
                        cr.set_source_rgb(r, g, b);
                        let fdesc = match (cell.attrs.bold, cell.attrs.italic) {
                            (false, false) => &font_normal,
                            (true, false) => &font_bold,
                            (false, true) => &font_italic,
                            (true, true) => &font_bold_italic,
                        };
                        layout.set_font_description(Some(fdesc));
                        let text = cell.display_text();
                        layout.set_text(&text);
                        cr.move_to(x, y);
                        pangocairo::functions::show_layout(cr, &layout);
                    }

                    if cell.attrs.underline {
                        cr.set_source_rgb(r, g, b);
                        cr.set_line_width(1.0);
                        let uy = y + ch - 1.5;
                        cr.move_to(x, uy);
                        cr.line_to(x + cell_pixel_w, uy);
                        let _ = cr.stroke();
                    }

                    if cell.attrs.strikethrough {
                        cr.set_source_rgb(r, g, b);
                        cr.set_line_width(1.0);
                        let sy = y + ch * 0.5;
                        cr.move_to(x, sy);
                        cr.line_to(x + cell_pixel_w, sy);
                        let _ = cr.stroke();
                    }
                }
            }

            // Draw Cursor when at live bottom screen
            if emu.cursor_visible && emu.display_offset == 0 {
                let cx = emu.cursor_col as f64 * cw;
                let cy = emu.cursor_row as f64 * ch;
                let (cr_r, cr_g, cr_b) = profile.cursor_color.to_f64_tuple();
                cr.set_source_rgba(cr_r, cr_g, cr_b, 0.75);
                cr.rectangle(cx, cy, cw.ceil(), ch.ceil());
                let _ = cr.fill();

                // Redraw character under cursor in background color for legibility
                if emu.cursor_row < emu.grid.len()
                    && emu.cursor_col < emu.grid[emu.cursor_row].cells.len()
                {
                    let cell = &emu.grid[emu.cursor_row].cells[emu.cursor_col];
                    if cell.ch != ' ' && cell.width > 0 {
                        cr.set_source_rgb(bg_r, bg_g, bg_b);
                        layout.set_font_description(Some(&font_normal));
                        let text = cell.display_text();
                        layout.set_text(&text);
                        cr.move_to(cx, cy);
                        pangocairo::functions::show_layout(cr, &layout);
                    }
                }
            }
        });
    }

    // Resize handler: compute new cols x rows and inform both emulator and PTY
    // IMPORTANT: Never call queue_draw() or set_text()/set_title() synchronously inside connect_resize,
    // because GTK4 invokes connect_resize during gtk_widget_size_allocate(), and mutating labels/window title
    // during size allocation triggers a recursive relayout until the stack overflows!
    {
        let tab_resize = session.clone();
        let state_resize = app_state.clone();
        drawing_area.connect_resize(move |_area, width, height| {
            if width <= 0 || height <= 0 {
                return;
            }
            let mut changed = false;
            if let Ok(mut tab) = tab_resize.try_borrow_mut() {
                let new_cols = ((width as f64 / tab.cell_width).floor() as usize).max(10);
                let new_rows = ((height as f64 / tab.cell_height).floor() as usize).max(4);
                if new_cols != tab.emulator.cols || new_rows != tab.emulator.rows {
                    tab.emulator.resize(new_cols, new_rows);
                    let _ = tab.pty.resize(new_cols as u16, new_rows as u16);
                    changed = true;
                }
            }
            if changed {
                let st_deferred = state_resize.clone();
                glib::idle_add_local_once(move || {
                    if let Ok(st) = st_deferred.try_borrow() {
                        st.update_status_bar();
                    }
                });
            }
        });
    }

    // Keyboard input controller
    {
        let key_controller = gtk4::EventControllerKey::new();
        let tab_key = session.clone();
        let state_key = app_state.clone();
        key_controller.connect_key_pressed(move |_, keyval, _keycode, modifier| {
            let ctrl = modifier.contains(gdk::ModifierType::CONTROL_MASK);
            let shift = modifier.contains(gdk::ModifierType::SHIFT_MASK);
            let alt = modifier.contains(gdk::ModifierType::ALT_MASK);

            // Keyboard shortcuts:
            // Ctrl+Shift+T: New Tab
            // Ctrl+Shift+W: Close Tab
            // Ctrl+Shift+C: Copy
            // Ctrl+Shift+V: Paste
            // Ctrl+Shift+R or F2 (with Ctrl+Shift): Rename Tab
            // Ctrl+PageUp / Ctrl+PageDown: Switch Tabs
            // Shift+PageUp / Shift+PageDown: Scrollback
            if ctrl && shift {
                match keyval {
                    gdk::Key::T | gdk::Key::t => {
                        create_new_tab(&state_key, None);
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::W | gdk::Key::w => {
                        close_tab(&state_key, &tab_key);
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::C | gdk::Key::c => {
                        copy_selection_from_current_tab(&state_key);
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::V | gdk::Key::v => {
                        paste_into_current_tab(&state_key, false);
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::R | gdk::Key::r => {
                        show_rename_tab_dialog(&state_key, tab_key.clone());
                        return glib::Propagation::Stop;
                    }
                    _ => {}
                }
            }

            if ctrl && !shift && !alt {
                if keyval == gdk::Key::Page_Up {
                    let nb = state_key.borrow().notebook.clone();
                    if let Some(cur) = nb.current_page() {
                        if cur > 0 {
                            nb.set_current_page(Some(cur - 1));
                        }
                    }
                    return glib::Propagation::Stop;
                } else if keyval == gdk::Key::Page_Down {
                    let nb = state_key.borrow().notebook.clone();
                    if let Some(cur) = nb.current_page() {
                        if cur + 1 < nb.n_pages() {
                            nb.set_current_page(Some(cur + 1));
                        }
                    }
                    return glib::Propagation::Stop;
                }
            }

            if shift && !ctrl && !alt {
                if keyval == gdk::Key::Page_Up {
                    let mut tab = tab_key.borrow_mut();
                    let page = tab.emulator.rows.saturating_sub(2);
                    let max_sb = tab.emulator.scrollback.len();
                    tab.emulator.display_offset =
                        (tab.emulator.display_offset + page).min(max_sb);
                    tab.drawing_area.queue_draw();
                    return glib::Propagation::Stop;
                } else if keyval == gdk::Key::Page_Down {
                    let mut tab = tab_key.borrow_mut();
                    let page = tab.emulator.rows.saturating_sub(2);
                    tab.emulator.display_offset =
                        tab.emulator.display_offset.saturating_sub(page);
                    tab.drawing_area.queue_draw();
                    return glib::Propagation::Stop;
                } else if keyval == gdk::Key::Insert {
                    paste_into_current_tab(&state_key, true);
                    return glib::Propagation::Stop;
                }
            }

            // Alt+1..9 tab switching
            if alt && !ctrl && !shift {
                if let Some(ch) = keyval.to_unicode() {
                    if ('1'..='9').contains(&ch) {
                        let idx = (ch as u32) - ('1' as u32);
                        let nb = state_key.borrow().notebook.clone();
                        if idx < nb.n_pages() {
                            nb.set_current_page(Some(idx));
                            return glib::Propagation::Stop;
                        }
                    }
                }
            }

            let mut tab = tab_key.borrow_mut();
            if let Some(bytes) = encode_key_event(keyval, modifier, &tab.emulator) {
                let _ = tab.pty.write_all(&bytes);
                if tab.emulator.display_offset != 0 {
                    tab.emulator.display_offset = 0;
                    tab.drawing_area.queue_draw();
                }
                return glib::Propagation::Stop;
            }

            glib::Propagation::Proceed
        });
        drawing_area.add_controller(key_controller);
    }

    // Mouse Click + Drag selection & Middle-click PRIMARY paste
    {
        let click_gesture = gtk4::GestureClick::new();
        click_gesture.set_button(0); // Listen to all mouse buttons
        let tab_click = session.clone();
        let state_click = app_state.clone();
        click_gesture.connect_pressed(move |gesture, _n_press, x, y| {
            let button = gesture.current_button();
            {
                let tab = tab_click.borrow();
                tab.drawing_area.grab_focus();
            }
            if button == 1 {
                // Left-click starts text selection
                let mut tab = tab_click.borrow_mut();
                let col = ((x / tab.cell_width).floor() as usize)
                    .min(tab.emulator.cols.saturating_sub(1));
                let view_row = ((y / tab.cell_height).floor() as usize)
                    .min(tab.emulator.rows.saturating_sub(1));
                let (_, abs_line) = tab.emulator.visible_row(view_row);
                let pt = GridPoint { abs_line, col };
                tab.emulator.selection_start = Some(pt);
                tab.emulator.selection_end = Some(pt);
                tab.is_selecting = true;
                tab.drawing_area.queue_draw();
            } else if button == 2 {
                // Middle-click pastes from PRIMARY selection (X11/Wayland standard)
                paste_into_current_tab(&state_click, true);
            }
        });

        let tab_release = session.clone();
        click_gesture.connect_released(move |gesture, _, _, _| {
            if gesture.current_button() == 1 {
                let mut tab = tab_release.borrow_mut();
                tab.is_selecting = false;
                if let Some(text) = tab.emulator.selected_text() {
                    // Populate PRIMARY selection automatically on mouse selection
                    write_system_clipboard("p", &text);
                }
            }
        });
        drawing_area.add_controller(click_gesture);
    }

    // Mouse motion controller for drag selection
    {
        let motion_controller = gtk4::EventControllerMotion::new();
        let tab_motion = session.clone();
        motion_controller.connect_motion(move |_, x, y| {
            let mut tab = tab_motion.borrow_mut();
            if !tab.is_selecting {
                return;
            }
            let col = ((x.max(0.0) / tab.cell_width).floor() as usize)
                .min(tab.emulator.cols.saturating_sub(1));
            let view_row = ((y.max(0.0) / tab.cell_height).floor() as usize)
                .min(tab.emulator.rows.saturating_sub(1));
            let (_, abs_line) = tab.emulator.visible_row(view_row);
            tab.emulator.selection_end = Some(GridPoint { abs_line, col });
            tab.drawing_area.queue_draw();
        });
        drawing_area.add_controller(motion_controller);
    }

    // Mouse wheel scroll controller
    {
        let scroll_controller = gtk4::EventControllerScroll::new(
            gtk4::EventControllerScrollFlags::VERTICAL | gtk4::EventControllerScrollFlags::DISCRETE,
        );
        let tab_scroll = session.clone();
        scroll_controller.connect_scroll(move |controller, _dx, dy| {
            if dy == 0.0 {
                return glib::Propagation::Proceed;
            }

            let mut tab = tab_scroll.borrow_mut();
            if let Some(event) = controller.current_event()
                && let Some(native) = tab.drawing_area.native()
                && let Some((surface_x, surface_y)) = event.position().or_else(|| {
                    // Scroll events may omit coordinates. Query the scrolling device.
                    let (x, y, _) = native.surface()?.device_position(&event.device()?)?;
                    Some((x, y))
                })
            {
                let (offset_x, offset_y) = native.surface_transform();
                let point = gtk4::graphene::Point::new(
                    (surface_x + offset_x) as f32,
                    (surface_y + offset_y) as f32,
                );
                if let Some(point) = native.compute_point(&tab.drawing_area, &point) {
                    let col = (f64::from(point.x()) / tab.cell_width) as usize;
                    let row = (f64::from(point.y()) / tab.cell_height) as usize;
                    let state = event.modifier_state();
                    let modifiers = (u8::from(state.contains(gdk::ModifierType::SHIFT_MASK)) << 2)
                        | (u8::from(state.contains(gdk::ModifierType::ALT_MASK)) << 3)
                        | (u8::from(state.contains(gdk::ModifierType::CONTROL_MASK)) << 4);
                    if let Some(report) =
                        tab.emulator.encode_mouse_wheel(dy < 0.0, row, col, modifiers)
                    {
                        for _ in 0..dy.abs() as usize {
                            let _ = tab.pty.write_all(&report);
                        }
                        return glib::Propagation::Stop;
                    }
                }
            }
            if tab.emulator.alt_screen_active {
                return glib::Propagation::Proceed;
            }
            let max_sb = tab.emulator.scrollback.len();
            if dy < 0.0 {
                tab.emulator.display_offset = (tab.emulator.display_offset + 3).min(max_sb);
            } else if dy > 0.0 {
                tab.emulator.display_offset = tab.emulator.display_offset.saturating_sub(3);
            }
            tab.drawing_area.queue_draw();
            glib::Propagation::Stop
        });
        drawing_area.add_controller(scroll_controller);
    }

    // Scrollbar value change synchronization
    {
        let tab_adj = session.clone();
        adjustment.connect_value_changed(move |adj| {
            let Ok(mut tab) = tab_adj.try_borrow_mut() else {
                return;
            };
            if tab.syncing_scrollbar || tab.emulator.alt_screen_active {
                return;
            }
            let max_sb = tab.emulator.scrollback.len();
            let val = adj.value().round() as usize;
            let new_offset = max_sb.saturating_sub(val.min(max_sb));
            if tab.emulator.display_offset != new_offset {
                tab.emulator.display_offset = new_offset;
                tab.drawing_area.queue_draw();
            }
        });
    }

    // Poll PTY output queue on the GLib main loop (10ms tick)
    {
        let tab_poll = session.clone();
        let state_poll = app_state.clone();
        glib::timeout_add_local(Duration::from_millis(10), move || {
            let mut received_any = false;
            let mut child_exited = false;
            let mut osc52_events = Vec::new();
            let mut title_changed = false;

            {
                let mut tab = tab_poll.borrow_mut();
                let prev_title = tab.emulator.title.clone();
                while let Ok(ev) = tab.pty.rx.try_recv() {
                    match ev {
                        PtyEvent::Data(bytes) => {
                            tab.emulator.process_bytes(&bytes);
                            received_any = true;
                        }
                        PtyEvent::Exited => {
                            child_exited = true;
                            tab.exited = true;
                        }
                    }
                }

                if received_any {
                    if !tab.emulator.pty_write_queue.is_empty() {
                        let reply: Vec<u8> = tab.emulator.pty_write_queue.drain(..).collect();
                        let _ = tab.pty.write_all(&reply);
                    }
                    if !tab.emulator.osc52_events.is_empty() {
                        osc52_events.append(&mut tab.emulator.osc52_events);
                    }
                    if tab.emulator.title != prev_title {
                        title_changed = true;
                        tab.update_tab_label();
                    }
                }
            }

            if received_any {
                let (adj, drawing_area, sb_len, rows_f, offset_f) = {
                    let mut tab = tab_poll.borrow_mut();
                    tab.syncing_scrollbar = true;
                    (
                        tab.adjustment.clone(),
                        tab.drawing_area.clone(),
                        tab.emulator.scrollback.len() as f64,
                        tab.emulator.rows as f64,
                        tab.emulator.display_offset as f64,
                    )
                };
                adj.configure(
                    sb_len - offset_f,
                    0.0,
                    sb_len + rows_f,
                    1.0,
                    (rows_f - 2.0).max(1.0),
                    rows_f,
                );
                tab_poll.borrow_mut().syncing_scrollbar = false;
                drawing_area.queue_draw();
            }

            // Process OSC 52 Remote Copy/Paste events outside borrow_mut
            for ev in osc52_events {
                match ev {
                    Osc52Event::WriteClipboard { selection, text } => {
                        write_system_clipboard(&selection, &text);
                    }
                    Osc52Event::ReadClipboardQuery { selection } => {
                        handle_osc52_read_query(selection, tab_poll.clone());
                    }
                }
            }

            if title_changed {
                state_poll.borrow().update_status_bar();
            }

            if child_exited {
                close_tab(&state_poll, &tab_poll);
                return glib::ControlFlow::Break;
            }

            glib::ControlFlow::Continue
        });
    }

    drawing_area.grab_focus();
}

/// Close a specific tab; if it was the last tab, close the application window
pub fn close_tab(app_state: &Rc<RefCell<AppState>>, target_tab: &Rc<RefCell<TabSession>>) {
    let target_id = target_tab.borrow().id;
    let root_widget = target_tab.borrow().root_box.clone();

    let (notebook, should_quit) = {
        let mut state = app_state.borrow_mut();
        state.tabs.retain(|t| t.borrow().id != target_id);
        (state.notebook.clone(), state.tabs.is_empty())
    };

    if let Some(page_num) = notebook.page_num(&root_widget) {
        notebook.remove_page(Some(page_num));
    }

    if should_quit {
        app_state.borrow().window.close();
    } else {
        app_state.borrow().update_status_bar();
    }
}

/// Build the Midnight Terminal menu bar (File, Edit, View, Terminal, Tabs, Help).
/// Menu items trigger `win.*` actions registered on the main window; accelerator labels are
/// display-only because the terminal's key controller already handles those shortcuts.
fn build_menubar(app_state: &Rc<RefCell<AppState>>) -> gtk4::Box {
    use gtk4::gio;

    let window = app_state.borrow().window.clone();

    // Register a parameterless window action.
    let add_action = |name: &str, f: Box<dyn Fn()>| {
        let action = gio::SimpleAction::new(name, None);
        action.connect_activate(move |_, _| f());
        window.add_action(&action);
    };
    // Register a window action taking the $TERM name of an emulation type.
    let add_emu_action = |name: &str, f: Box<dyn Fn(EmulationType)>| {
        let action = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
        action.connect_activate(move |_, param| {
            let Some(term) = param.and_then(|p| p.get::<String>()) else {
                return;
            };
            if let Some(emu) = EmulationType::all().iter().find(|e| e.term_env() == term) {
                f(*emu);
            }
        });
        window.add_action(&action);
    };

    // Run a closure on the current tab, if any.
    fn with_current_tab(st: &Rc<RefCell<AppState>>, f: impl FnOnce(&Rc<RefCell<TabSession>>)) {
        let cur = st.borrow().current_tab();
        if let Some(tab) = cur {
            f(&tab);
        }
    }

    fn zoom(st: &Rc<RefCell<AppState>>, f: impl FnOnce(f64) -> f64) {
        let mut prof = st.borrow().active_profile.clone();
        prof.font_size = f(prof.font_size).clamp(6.0, 48.0);
        apply_profile_to_current_tab(st, prof);
    }

    fn switch_tab(st: &Rc<RefCell<AppState>>, forward: bool) {
        let nb = st.borrow().notebook.clone();
        if let Some(cur) = nb.current_page() {
            if forward && cur + 1 < nb.n_pages() {
                nb.set_current_page(Some(cur + 1));
            } else if !forward && cur > 0 {
                nb.set_current_page(Some(cur - 1));
            }
        }
    }

    fn profile_for(st: &Rc<RefCell<AppState>>, emu: EmulationType) -> TerminalProfile {
        let mut prof = st.borrow().active_profile.clone();
        prof.emulation = emu;
        prof.name = format!("Profile ({})", emu.term_env());
        prof
    }

    // File
    let st = app_state.clone();
    add_action("new-tab", Box::new(move || create_new_tab(&st, None)));
    let st = app_state.clone();
    add_emu_action(
        "new-tab-emulation",
        Box::new(move |emu| create_new_tab(&st, Some(profile_for(&st, emu)))),
    );
    let st = app_state.clone();
    add_action(
        "close-tab",
        Box::new(move || with_current_tab(&st, |tab| close_tab(&st, tab))),
    );
    let st = app_state.clone();
    add_action("quit", Box::new(move || st.borrow().window.close()));

    // Edit
    let st = app_state.clone();
    add_action("copy", Box::new(move || copy_selection_from_current_tab(&st)));
    let st = app_state.clone();
    add_action("paste", Box::new(move || paste_into_current_tab(&st, false)));
    let st = app_state.clone();
    add_action(
        "paste-primary",
        Box::new(move || paste_into_current_tab(&st, true)),
    );
    let st = app_state.clone();
    add_action(
        "select-all",
        Box::new(move || {
            with_current_tab(&st, |tab_rc| {
                let mut tab = tab_rc.borrow_mut();
                let total = tab.emulator.total_lines();
                let cols = tab.emulator.cols;
                tab.emulator.selection_start = Some(GridPoint {
                    abs_line: 0,
                    col: 0,
                });
                tab.emulator.selection_end = Some(GridPoint {
                    abs_line: total.saturating_sub(1),
                    col: cols.saturating_sub(1),
                });
                tab.drawing_area.queue_draw();
            })
        }),
    );
    let st = app_state.clone();
    add_action("preferences", Box::new(move || show_preferences_dialog(&st)));

    // View
    let st = app_state.clone();
    add_action("zoom-in", Box::new(move || zoom(&st, |s| s + 1.0)));
    let st = app_state.clone();
    add_action("zoom-out", Box::new(move || zoom(&st, |s| s - 1.0)));
    let st = app_state.clone();
    add_action(
        "zoom-reset",
        Box::new(move || zoom(&st, |_| crate::profile::DEFAULT_FONT_SIZE)),
    );
    let st = app_state.clone();
    add_action(
        "fullscreen",
        Box::new(move || {
            let win = st.borrow().window.clone();
            if win.is_fullscreen() {
                win.unfullscreen();
            } else {
                win.fullscreen();
            }
        }),
    );

    // Terminal / Tabs / Help
    let st = app_state.clone();
    add_action(
        "rename-tab",
        Box::new(move || with_current_tab(&st, |tab| show_rename_tab_dialog(&st, tab.clone()))),
    );
    let st = app_state.clone();
    add_emu_action(
        "use-emulation",
        Box::new(move |emu| apply_profile_to_current_tab(&st, profile_for(&st, emu))),
    );
    let st = app_state.clone();
    add_action("osc52", Box::new(move || show_osc52_helper_dialog(&st)));
    let st = app_state.clone();
    add_action(
        "reset",
        Box::new(move || {
            with_current_tab(&st, |tab_rc| {
                let mut tab = tab_rc.borrow_mut();
                tab.emulator.process_bytes(b"\x1bc\x1b[3J");
                tab.drawing_area.queue_draw();
            })
        }),
    );
    let st = app_state.clone();
    add_action("prev-tab", Box::new(move || switch_tab(&st, false)));
    let st = app_state.clone();
    add_action("next-tab", Box::new(move || switch_tab(&st, true)));

    // Menu model
    let item = |label: &str, action: &str, accel: Option<&str>| {
        let it = gio::MenuItem::new(Some(label), Some(action));
        if let Some(a) = accel {
            it.set_attribute_value("accel", Some(&a.to_variant()));
        }
        it
    };
    let emu_section = |label_prefix: &str, action: &str| {
        let section = gio::Menu::new();
        for emu in EmulationType::all() {
            let it = gio::MenuItem::new(
                Some(&format!("{label_prefix} {}", emu.term_env())),
                None,
            );
            it.set_action_and_target_value(Some(action), Some(&emu.term_env().to_variant()));
            section.append_item(&it);
        }
        section
    };
    let section = |items: &[gio::MenuItem]| {
        let s = gio::Menu::new();
        for it in items {
            s.append_item(it);
        }
        s
    };

    let file = gio::Menu::new();
    file.append_section(None, &section(&[item("New Tab", "win.new-tab", Some("<Ctrl><Shift>t"))]));
    file.append_section(None, &emu_section("New", "win.new-tab-emulation"));
    file.append_section(
        None,
        &section(&[
            item("Close Tab", "win.close-tab", Some("<Ctrl><Shift>w")),
            item("Quit", "win.quit", None),
        ]),
    );

    let edit = gio::Menu::new();
    edit.append_section(
        None,
        &section(&[
            item("Copy", "win.copy", Some("<Ctrl><Shift>c")),
            item("Paste", "win.paste", Some("<Ctrl><Shift>v")),
            item("Paste Selection", "win.paste-primary", Some("<Shift>Insert")),
            item("Select All", "win.select-all", None),
        ]),
    );
    edit.append_section(
        None,
        &section(&[item("Preferences & Profiles", "win.preferences", None)]),
    );

    let view = gio::Menu::new();
    view.append_section(
        None,
        &section(&[
            item("Zoom In", "win.zoom-in", None),
            item("Zoom Out", "win.zoom-out", None),
            item(
                &format!("Normal Size ({} pt)", crate::profile::DEFAULT_FONT_SIZE),
                "win.zoom-reset",
                None,
            ),
        ]),
    );
    view.append_section(None, &section(&[item("Fullscreen", "win.fullscreen", None)]));

    let terminal = gio::Menu::new();
    terminal.append_section(
        None,
        &section(&[item("Set Title…", "win.rename-tab", Some("<Ctrl><Shift>r"))]),
    );
    terminal.append_section(None, &emu_section("Use", "win.use-emulation"));
    terminal.append_section(
        None,
        &section(&[
            item("OSC 52 Remote Clipboard…", "win.osc52", None),
            item("Reset and Clear", "win.reset", None),
        ]),
    );

    let tabs = gio::Menu::new();
    tabs.append_section(
        None,
        &section(&[
            item("Previous Tab", "win.prev-tab", Some("<Ctrl>Page_Up")),
            item("Next Tab", "win.next-tab", Some("<Ctrl>Page_Down")),
        ]),
    );
    tabs.append_section(
        None,
        &section(&[item("Rename Tab…", "win.rename-tab", Some("<Ctrl><Shift>r"))]),
    );

    let help = gio::Menu::new();
    help.append_item(&item(
        "OSC 52 Remote Copy/Paste Guide…",
        "win.osc52",
        None,
    ));

    let model = gio::Menu::new();
    model.append_submenu(Some("_File"), &file);
    model.append_submenu(Some("_Edit"), &edit);
    model.append_submenu(Some("_View"), &view);
    model.append_submenu(Some("_Terminal"), &terminal);
    model.append_submenu(Some("T_abs"), &tabs);
    model.append_submenu(Some("_Help"), &help);

    let menubar = gtk4::PopoverMenuBar::from_model(Some(&model));
    menubar.set_hexpand(true);

    let container = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    container.append(&menubar);
    container
}

/// Build and launch the main Midnight Terminal window
pub fn build_ui(app: &gtk4::Application) {
    let window = gtk4::ApplicationWindow::builder()
        .application(app)
        .title("Midnight Terminal")
        .default_width(920)
        .default_height(600)
        .build();

    let main_vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

    let notebook = gtk4::Notebook::builder()
        .scrollable(true)
        .show_border(false)
        .hexpand(true)
        .vexpand(true)
        .build();

    let status_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    status_bar.set_margin_top(2);
    status_bar.set_margin_bottom(2);
    status_bar.set_margin_start(8);
    status_bar.set_margin_end(8);
    let status_label = gtk4::Label::builder()
        .label("Ready")
        .halign(gtk4::Align::Start)
        .ellipsize(pango::EllipsizeMode::End)
        .hexpand(true)
        .build();
    status_bar.append(&status_label);

    let placeholder_menubar = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    let placeholder_rename = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    let placeholder_entry = gtk4::Entry::new();
    let placeholder_prefs = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let placeholder_osc52 = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let initial_profile = TerminalProfile::load_or_default();

    let app_state = Rc::new(RefCell::new(AppState {
        window: window.clone(),
        notebook: notebook.clone(),
        tabs: Vec::new(),
        next_tab_id: 1,
        active_profile: initial_profile,
        menubar_visible: true,
        menubar_box: placeholder_menubar,
        status_label,
        rename_bar: placeholder_rename,
        rename_entry: placeholder_entry,
        rename_target_tab: None,
        prefs_bar: placeholder_prefs,
        osc52_bar: placeholder_osc52,
    }));

    let menubar = build_menubar(&app_state);
    let (rename_bar, rename_entry) = build_inline_rename_bar(&app_state);
    let prefs_bar = build_inline_preferences_bar(&app_state);
    let osc52_bar = build_inline_osc52_bar(&app_state);
    {
        let mut st = app_state.borrow_mut();
        st.menubar_box = menubar.clone();
        st.rename_bar = rename_bar.clone();
        st.rename_entry = rename_entry;
        st.prefs_bar = prefs_bar.clone();
        st.osc52_bar = osc52_bar.clone();
    }

    main_vbox.append(&menubar);
    main_vbox.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
    main_vbox.append(&rename_bar);
    main_vbox.append(&prefs_bar);
    main_vbox.append(&osc52_bar);
    main_vbox.append(&notebook);
    main_vbox.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
    main_vbox.append(&status_bar);

    window.set_child(Some(&main_vbox));

    // Update status bar and focus terminal canvas when user switches tabs
    {
        let st_switch = app_state.clone();
        notebook.connect_switch_page(move |nb, page, _page_num| {
            let page_widget = page.clone();
            let st_clone = st_switch.clone();
            let _ = nb;
            glib::idle_add_local_once(move || {
                if let Ok(state) = st_clone.try_borrow() {
                    for tab in &state.tabs {
                        if let Ok(t) = tab.try_borrow() {
                            if t.root_box.upcast_ref::<gtk4::Widget>() == &page_widget {
                                t.drawing_area.grab_focus();
                                break;
                            }
                        }
                    }
                    state.update_status_bar();
                }
            });
        });
    }

    // Create initial tab
    create_new_tab(&app_state, None);

    window.present();
}
