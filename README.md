# Midnight Terminal (mterm)

A GTK4 terminal emulator for MidnightBSD, written in Rust. It provides tabs,
color themes, selectable ANSI palettes, Unicode support, and OSC 52 remote
copy/paste.

## Features

- **Tabs**: open, close, rename, reorder, and switch tabs. Tab titles follow
  the title the shell or program sets (OSC 0/2) unless you set a custom one.
- **Terminal emulation**: choose `$TERM` per tab: `xterm-256color` (default),
  `xterm`, `xterm-color`, `vt100`, or `vt220`. Supports 256-color and 24-bit
  color, the alternate screen, scroll regions, DEC line drawing, and
  bracketed paste.
- **Unicode**: wide (CJK) characters and combining marks.
- **Color themes**: Midnight Dark (default), Solarized Dark, Daylight Light,
  VT100 Green, Amber, Novel, and Ocean.
- **ANSI palettes**: keep each theme's tuned palette or swap in MidnightBSD
  Console, Tango, XTerm, Linux Console, Rxvt, Solarized, or Pastel. The
  Preferences panel warns when a palette has colors that are hard to read on
  the current background.
- **OSC 52 clipboard**: programs on remote hosts (for example inside `ssh`
  or `tmux`) can copy to, and optionally read from, your local clipboard.
- **Scrollback**: configurable length, mouse wheel and Shift+PageUp/PageDown.

## Requirements

- Rust 1.85 or newer (the crate uses the 2024 edition)
- GTK 4.10 or newer, plus Pango and Cairo
- `pkg-config` (or `pkgconf`)

On MidnightBSD:

```sh
mport install rust gtk4 pkgconf
```

On other systems, install your distribution's Rust toolchain (or use
[rustup](https://rustup.rs)) and its GTK 4 development package.

## Building

```sh
git clone https://github.com/MidnightBSD/mterm.git
cd mterm
cargo build --release
```

The binary is written to `target/release/mterm`. To install it for your user:

```sh
cargo install --path .
```

This places `mterm` in `~/.cargo/bin`.

### Desktop menu entry

`data/org.midnightbsd.mterm.desktop` adds Midnight Terminal to desktop
application menus (under System). It runs `mterm` from your `PATH`. Install
it system-wide:

```sh
install -m 644 data/org.midnightbsd.mterm.desktop /usr/local/share/applications/
update-desktop-database /usr/local/share/applications
```

or for just your user:

```sh
install -m 644 data/org.midnightbsd.mterm.desktop ~/.local/share/applications/
```

Run the tests with:

```sh
cargo test
```

## Usage

Start it with `mterm`. It launches your login shell from `$SHELL`, or
`/bin/sh` if that is unset. Shells see `TERM` set to the selected emulation,
`COLORTERM=truecolor` for color-capable emulations, and `TERM_PROGRAM=mterm`.

### Keyboard and mouse

| Action | Shortcut |
|---|---|
| New tab | Ctrl+Shift+T |
| Close tab | Ctrl+Shift+W |
| Copy | Ctrl+Shift+C |
| Paste | Ctrl+Shift+V |
| Paste primary selection | Shift+Insert, or middle-click |
| Rename tab | Ctrl+Shift+R, or double-click the tab |
| Previous / next tab | Ctrl+PageUp / Ctrl+PageDown |
| Go to tab 1–9 | Alt+1 … Alt+9 |
| Scroll back / forward | Shift+PageUp / Shift+PageDown, or the mouse wheel |

Selecting text with the mouse copies it to the primary selection.

### Preferences

Open **Edit → Preferences & Profiles** to change the emulation, font,
colors, scrollback length, OSC 52 permissions, and ANSI palette. Click a
color preset, optionally adjust it, then **Apply & Save Profile**.

Settings are saved to `~/.config/mterm/config.toml` and used for new tabs.

### OSC 52 remote copy/paste

When a remote program sends an OSC 52 sequence, mterm puts the text on your
local clipboard. Both copying (write) and clipboard queries (read) are
enabled by default; turn either off in Preferences. **Terminal → OSC 52
Remote Clipboard** has a short guide and a live test.

For `tmux`, enable clipboard passthrough in `~/.tmux.conf`:

```
set -g set-clipboard on
```

## Notes

- **Renderer**: mterm uses GTK's Cairo renderer by default. To try the GL
  renderer, run `GSK_RENDERER=gl mterm`.
- **IBus on GTK 4.24**: the IBus GTK4 input-method module in IBus 1.5.34
  and earlier recurses infinitely when used with GTK 4.24, crashing any GTK4
  application that has a text field. MidnightBSD's `textproc/ibus` port
  carries a fix
  ([MidnightBSD/mports#3595](https://github.com/MidnightBSD/mports/pull/3595)).
  On an unpatched system, start mterm with
  `GTK_IM_MODULE=gtk-im-context-simple` as a workaround.

## License

BSD 2-Clause. See [LICENSE](LICENSE).
