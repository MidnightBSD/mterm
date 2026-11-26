use mterm::profile::{AnsiPalette, EmulationType, RgbColor, TerminalProfile};

#[test]
fn tango_matches_midnight_dark_theme() {
    let theme = TerminalProfile::midnight_dark(EmulationType::Xterm256Color);
    assert_eq!(AnsiPalette::Tango.colors().unwrap(), theme.palette);
}

#[test]
fn midnightbsd_console_matches_kernel_table() {
    // Percentages from sys/dev/vt/colors/vt_termcolors.c; the kernel scales them as 255 * pct / 100.
    let pct: [(u32, u32, u32); 16] = [
        (0, 0, 0), (50, 0, 0), (0, 50, 0), (77, 63, 0),
        (20, 40, 64), (50, 0, 50), (0, 50, 50), (75, 75, 75),
        (18, 20, 21), (100, 0, 0), (0, 100, 0), (100, 100, 0),
        (45, 62, 81), (100, 0, 100), (0, 100, 100), (100, 100, 100),
    ];
    let scale = |p: u32| (255 * p / 100) as u8;
    let expected: Vec<RgbColor> = pct
        .iter()
        .map(|&(r, g, b)| RgbColor::new(scale(r), scale(g), scale(b)))
        .collect();
    assert_eq!(AnsiPalette::MidnightBsdConsole.colors().unwrap().to_vec(), expected);
}

#[test]
fn theme_default_uses_theme_palette_and_override_replaces_it() {
    let mut p = TerminalProfile::novel(EmulationType::Xterm256Color);
    assert_eq!(p.ansi_colors(), p.palette);
    assert_eq!(p.color_from_index(1), p.palette[1]);

    p.ansi_palette = AnsiPalette::LinuxConsole;
    let vga = AnsiPalette::LinuxConsole.colors().unwrap();
    assert_eq!(p.color_from_index(1), vga[1]);
    assert_eq!(p.color_from_index(12), vga[12]);
    // The theme's own palette is kept so "Theme default" can restore it.
    assert_ne!(p.palette, vga);
}

#[test]
fn every_palette_has_a_label_and_round_trips_through_config() {
    for pal in AnsiPalette::all() {
        assert!(!pal.label().is_empty());
        let mut p = TerminalProfile::default();
        p.ansi_palette = *pal;
        let text = toml::to_string(&p).unwrap();
        let back: TerminalProfile = toml::from_str(&text).unwrap();
        assert_eq!(back.ansi_palette, *pal);
    }
}

#[test]
fn config_saved_before_palettes_existed_still_loads() {
    let mut text = toml::to_string(&TerminalProfile::default()).unwrap();
    text = text
        .lines()
        .filter(|l| !l.starts_with("ansi_palette"))
        .collect::<Vec<_>>()
        .join("\n");
    let p: TerminalProfile = toml::from_str(&text).unwrap();
    assert_eq!(p.ansi_palette, AnsiPalette::ThemeDefault);
}

#[test]
fn readability_hint_flags_clashing_palettes() {
    let mut p = TerminalProfile::midnight_dark(EmulationType::Xterm256Color);
    assert!(p.low_contrast_colors().is_empty(), "{:?}", p.low_contrast_colors());
    // XTerm's pure blue is famously hard to read on dark backgrounds.
    p.ansi_palette = AnsiPalette::XTerm;
    assert!(p.low_contrast_colors().contains(&4));
}
