use mterm::profile::{EmulationType, TerminalProfile, contrast_ratio as contrast};

#[test]
fn new_themes_are_readable() {
    let emu = EmulationType::Xterm256Color;
    for p in [TerminalProfile::amber(emu), TerminalProfile::novel(emu), TerminalProfile::ocean(emu)] {
        // Body text meets WCAG AA (4.5:1) against the background.
        let body = contrast(p.foreground, p.background);
        assert!(body >= 4.5, "{}: text contrast {body:.2}", p.name);
        // Every palette color except "black" (index 0, usually used as a background) stays legible.
        for (i, c) in p.palette.iter().enumerate().skip(1) {
            let r = contrast(*c, p.background);
            assert!(r >= 3.0, "{}: palette[{i}] contrast {r:.2}", p.name);
        }
    }
}
