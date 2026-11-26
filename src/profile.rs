use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Standard RGB color representation (0..=255 per channel)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl RgbColor {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub fn to_f64_tuple(self) -> (f64, f64, f64) {
        (
            self.r as f64 / 255.0,
            self.g as f64 / 255.0,
            self.b as f64 / 255.0,
        )
    }

    pub fn from_hex(hex: &str) -> Option<Self> {
        let s = hex.trim_start_matches('#');
        if s.len() != 6 {
            return None;
        }
        let r = u8::from_str_radix(&s[0..2], 16).ok()?;
        let g = u8::from_str_radix(&s[2..4], 16).ok()?;
        let b = u8::from_str_radix(&s[4..6], 16).ok()?;
        Some(Self { r, g, b })
    }

    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// Supported terminal emulation types ($TERM profiles)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EmulationType {
    /// Modern 256-color + truecolor xterm (`TERM=xterm-256color`)
    Xterm256Color,
    /// Standard xterm (`TERM=xterm`)
    Xterm,
    /// 16-color xterm (`TERM=xterm-color`)
    XtermColor,
    /// Classic DEC VT100 (`TERM=vt100`)
    Vt100,
    /// DEC VT220 (`TERM=vt220`)
    Vt220,
}

impl EmulationType {
    pub fn all() -> &'static [EmulationType] {
        &[
            EmulationType::Xterm256Color,
            EmulationType::Xterm,
            EmulationType::XtermColor,
            EmulationType::Vt100,
            EmulationType::Vt220,
        ]
    }

    pub fn term_env(&self) -> &'static str {
        match self {
            EmulationType::Xterm256Color => "xterm-256color",
            EmulationType::Xterm => "xterm",
            EmulationType::XtermColor => "xterm-color",
            EmulationType::Vt100 => "vt100",
            EmulationType::Vt220 => "vt220",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            EmulationType::Xterm256Color => "xterm-256color (Modern 256/TrueColor)",
            EmulationType::Xterm => "xterm (Standard X11 Terminal)",
            EmulationType::XtermColor => "xterm-color (ANSI 16-Color)",
            EmulationType::Vt100 => "vt100 (Classic DEC VT100)",
            EmulationType::Vt220 => "vt220 (DEC VT220)",
        }
    }

    pub fn from_term_str(s: &str) -> Self {
        match s.trim() {
            "xterm-256color" => EmulationType::Xterm256Color,
            "xterm" => EmulationType::Xterm,
            "xterm-color" => EmulationType::XtermColor,
            "vt100" => EmulationType::Vt100,
            "vt220" => EmulationType::Vt220,
            _ => EmulationType::Xterm256Color,
        }
    }

    /// Primary Device Attributes (DA1) response string for `CSI c` / `CSI 0 c`
    pub fn da1_response(&self) -> &'static [u8] {
        match self {
            // VT100 with Advanced Video Option (AVO)
            EmulationType::Vt100 => b"\x1b[?1;2c",
            // VT220
            EmulationType::Vt220 => b"\x1b[?62;1;2;6;7;8;9c",
            // xterm variants report VT220 / xterm attributes
            EmulationType::Xterm
            | EmulationType::XtermColor
            | EmulationType::Xterm256Color => b"\x1b[?62;1;2;6;7;8;9;15;22c",
        }
    }

    /// Secondary Device Attributes (DA2) response string for `CSI > c`
    pub fn da2_response(&self) -> &'static [u8] {
        match self {
            EmulationType::Vt100 => b"\x1b[>0;115;0c",
            EmulationType::Vt220 => b"\x1b[>1;10;0c",
            _ => b"\x1b[>41;354;0c",
        }
    }

    /// Whether 256-color and 24-bit truecolor SGR sequences are active
    pub fn supports_256_color(&self) -> bool {
        matches!(self, EmulationType::Xterm256Color | EmulationType::Xterm)
    }

    /// Whether ANSI 16 colors are supported (VT100 optionally supports monochrome or basic ANSI; we enable basic ANSI unless strict monochrome is chosen, or support 16-color on all except when clamped)
    pub fn supports_ansi_color(&self) -> bool {
        !matches!(self, EmulationType::Vt100)
    }

    /// Key sequence produced by Backspace key
    pub fn backspace_seq(&self) -> &'static [u8] {
        match self {
            EmulationType::Vt100 => b"\x08",
            _ => b"\x7f",
        }
    }
}

/// Complete Terminal Profile (emulation type, colors, font, OSC 52 permissions, scrollback)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalProfile {
    pub name: String,
    pub emulation: EmulationType,
    pub font_family: String,
    pub font_size: f64,
    pub foreground: RgbColor,
    pub background: RgbColor,
    pub cursor_color: RgbColor,
    pub palette: [RgbColor; 16],
    pub scrollback_lines: usize,
    pub allow_osc52_write: bool,
    pub allow_osc52_read: bool,
}

impl Default for TerminalProfile {
    fn default() -> Self {
        Self::midnight_dark(EmulationType::Xterm256Color)
    }
}

/// Default terminal font size in points
pub const DEFAULT_FONT_SIZE: f64 = 13.0;

impl TerminalProfile {
    /// Default Midnight Dark profile
    pub fn midnight_dark(emulation: EmulationType) -> Self {
        Self {
            name: format!("Default ({})", emulation.term_env()),
            emulation,
            font_family: "Monospace".to_string(),
            font_size: DEFAULT_FONT_SIZE,
            foreground: RgbColor::new(230, 230, 230),
            background: RgbColor::new(24, 26, 31),
            cursor_color: RgbColor::new(82, 139, 255),
            palette: [
                // Normal 0..7 (Tango-based)
                RgbColor::new(46, 52, 54),    // 0: Black
                RgbColor::new(204, 0, 0),     // 1: Red
                RgbColor::new(78, 154, 6),    // 2: Green
                RgbColor::new(196, 160, 0),   // 3: Yellow
                RgbColor::new(52, 101, 164),  // 4: Blue
                RgbColor::new(117, 80, 123),  // 5: Magenta
                RgbColor::new(6, 152, 154),   // 6: Cyan
                RgbColor::new(211, 215, 207), // 7: White
                // Bright 8..15
                RgbColor::new(85, 87, 83),    // 8: Bright Black
                RgbColor::new(239, 41, 41),   // 9: Bright Red
                RgbColor::new(138, 226, 52),  // 10: Bright Green
                RgbColor::new(252, 233, 79),  // 11: Bright Yellow
                RgbColor::new(114, 159, 207), // 12: Bright Blue
                RgbColor::new(173, 127, 168), // 13: Bright Magenta
                RgbColor::new(52, 226, 226),  // 14: Bright Cyan
                RgbColor::new(238, 238, 236), // 15: Bright White
            ],
            scrollback_lines: 10_000,
            allow_osc52_write: true,
            allow_osc52_read: true,
        }
    }

    pub fn solarized_dark(emulation: EmulationType) -> Self {
        let mut p = Self::midnight_dark(emulation);
        p.name = format!("Solarized Dark ({})", emulation.term_env());
        p.foreground = RgbColor::new(131, 148, 150);
        p.background = RgbColor::new(0, 43, 54);
        p.cursor_color = RgbColor::new(38, 139, 210);
        p
    }

    pub fn light_theme(emulation: EmulationType) -> Self {
        let mut p = Self::midnight_dark(emulation);
        p.name = format!("Light ({})", emulation.term_env());
        p.foreground = RgbColor::new(30, 34, 42);
        p.background = RgbColor::new(250, 250, 250);
        p.cursor_color = RgbColor::new(30, 102, 245);
        p
    }

    pub fn green_phosphor_vt100() -> Self {
        let mut p = Self::midnight_dark(EmulationType::Vt100);
        p.name = "DEC VT100 Green Phosphor".to_string();
        p.foreground = RgbColor::new(51, 255, 51);
        p.background = RgbColor::new(10, 20, 10);
        p.cursor_color = RgbColor::new(102, 255, 102);
        p
    }

    /// Resolve a 256-color index (0..=255) to an RgbColor using this profile's palette
    pub fn color_from_index(&self, idx: u8) -> RgbColor {
        if !self.emulation.supports_ansi_color() {
            // Classic VT100 monochrome fallback
            return self.foreground;
        }
        if idx < 16 {
            return self.palette[idx as usize];
        }
        if !self.emulation.supports_256_color() {
            // Map 16..255 down to basic 16 colors when in 16-color xterm-color mode
            return self.palette[(idx % 16) as usize];
        }
        if idx < 232 {
            // 6x6x6 color cube (indices 16..=231)
            let cube_idx = idx - 16;
            let r_idx = cube_idx / 36;
            let g_idx = (cube_idx % 36) / 6;
            let b_idx = cube_idx % 6;
            let to_val = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            RgbColor::new(to_val(r_idx), to_val(g_idx), to_val(b_idx))
        } else {
            // Grayscale ramp (indices 232..=255)
            let gray = 8 + (idx - 232) * 10;
            RgbColor::new(gray, gray, gray)
        }
    }

    pub fn config_path() -> Option<PathBuf> {
        std::env::var_os("HOME").map(|h| {
            PathBuf::from(h)
                .join(".config")
                .join("mterm")
                .join("config.toml")
        })
    }

    pub fn load_or_default() -> Self {
        if let Some(path) = Self::config_path() {
            if let Ok(contents) = fs::read_to_string(&path) {
                if let Ok(profile) = toml::from_str::<TerminalProfile>(&contents) {
                    return profile;
                }
            }
        }
        Self::default()
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::config_path().ok_or_else(|| "HOME not set".to_string())?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let data = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(path, data).map_err(|e| e.to_string())
    }
}
