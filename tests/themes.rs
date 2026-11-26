use mterm::profile::{EmulationType, RgbColor, TerminalProfile};

fn luminance(c: RgbColor) -> f64 {
    let ch = |v: u8| {
        let v = v as f64 / 255.0;
        if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * ch(c.r) + 0.7152 * ch(c.g) + 0.0722 * ch(c.b)
}

fn contrast(a: RgbColor, b: RgbColor) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

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
