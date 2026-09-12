//! Semantic picker styles, palettes, and terminal background detection.
//!
//! Palettes use 256-color indices: terminal themes remap the 16 base ANSI
//! colors, and bright black is nearly invisible on Solarized-style dark themes.

use std::{
    env,
    ffi::OsStr,
    io::{self, Write},
    sync::OnceLock,
};

use crossterm::{
    queue,
    style::{Attribute, Color, SetAttribute, SetBackgroundColor, SetForegroundColor},
};

use super::render::DetailStyle;

static ACTIVE_THEME: OnceLock<Theme> = OnceLock::new();
static DEFAULT_THEME: Theme = Theme::for_palette(Palette::Dark);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Palette {
    Dark,
    Light,
    Mono,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Background {
    Dark,
    Light,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Style {
    pub(super) foreground: Option<Color>,
    pub(super) background: Option<Color>,
    pub(super) bold: bool,
    pub(super) dim: bool,
    pub(super) underline: bool,
    pub(super) reverse: bool,
}

impl Style {
    const PLAIN: Self = Self {
        foreground: None,
        background: None,
        bold: false,
        dim: false,
        underline: false,
        reverse: false,
    };

    const fn fg(index: u8) -> Self {
        Self {
            foreground: Some(Color::AnsiValue(index)),
            ..Self::PLAIN
        }
    }

    const fn bold(self) -> Self {
        Self { bold: true, ..self }
    }

    const fn dim(self) -> Self {
        Self { dim: true, ..self }
    }

    const fn underline(self) -> Self {
        Self {
            underline: true,
            ..self
        }
    }

    const fn reverse(self) -> Self {
        Self {
            reverse: true,
            ..self
        }
    }

    /// Queues colors and attributes. Callers reset after the styled text.
    pub(super) fn queue(self, output: &mut impl Write) -> io::Result<()> {
        if let Some(color) = self.foreground {
            queue!(output, SetForegroundColor(color))?;
        }
        if let Some(color) = self.background {
            queue!(output, SetBackgroundColor(color))?;
        }
        for (enabled, attribute) in [
            (self.bold, Attribute::Bold),
            (self.dim, Attribute::Dim),
            (self.underline, Attribute::Underlined),
            (self.reverse, Attribute::Reverse),
        ] {
            if enabled {
                queue!(output, SetAttribute(attribute))?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Theme {
    pub(super) palette: Palette,
    normal: Style,
    muted: Style,
    accent: Style,
    strong: Style,
    selected: Style,
    danger: Style,
    warning: Style,
    match_highlight: Style,
    selected_match: Style,
    border: Style,
}

impl Theme {
    pub(super) const fn for_palette(palette: Palette) -> Self {
        match palette {
            // Greys and accents stay at least 4.5:1 on black and Solarized dark.
            Palette::Dark => Self {
                palette,
                normal: Style::PLAIN,
                muted: Style::fg(247),
                accent: Style::fg(80),
                strong: Style::PLAIN.bold(),
                selected: Style::fg(114).bold().reverse(),
                danger: Style::fg(203).bold(),
                warning: Style::fg(221),
                match_highlight: Style::fg(221).bold(),
                selected_match: Style::fg(221).bold().underline().reverse(),
                border: Style::fg(80),
            },
            // Text colors stay at least 4.5:1 on white and Solarized light; no yellow text.
            Palette::Light => Self {
                palette,
                normal: Style::PLAIN,
                muted: Style::fg(241),
                accent: Style::fg(24),
                strong: Style::PLAIN.bold(),
                selected: Style::fg(22).bold().reverse(),
                danger: Style::fg(160).bold(),
                warning: Style::fg(94),
                match_highlight: Style::fg(127).bold(),
                selected_match: Style::fg(127).bold().underline().reverse(),
                border: Style::fg(24),
            },
            Palette::Mono => Self {
                palette,
                normal: Style::PLAIN,
                muted: Style::PLAIN.dim(),
                accent: Style::PLAIN.bold(),
                strong: Style::PLAIN.bold(),
                selected: Style::PLAIN.bold().reverse(),
                danger: Style::PLAIN.bold(),
                warning: Style::PLAIN.bold(),
                match_highlight: Style::PLAIN.bold().underline(),
                selected_match: Style::PLAIN.bold().underline().reverse(),
                border: Style::PLAIN,
            },
        }
    }

    pub(super) const fn style(&self, role: DetailStyle) -> Style {
        match role {
            DetailStyle::Normal => self.normal,
            DetailStyle::Muted => self.muted,
            DetailStyle::Accent => self.accent,
            DetailStyle::Strong => self.strong,
            DetailStyle::Selected => self.selected,
            DetailStyle::Danger => self.danger,
            DetailStyle::Warning => self.warning,
            DetailStyle::Match => self.match_highlight,
            DetailStyle::SelectedMatch => self.selected_match,
            DetailStyle::Border => self.border,
        }
    }
}

/// Active picker theme, or the dark default before [`init`] runs (tests never detect).
pub(super) fn current() -> &'static Theme {
    ACTIVE_THEME.get().unwrap_or(&DEFAULT_THEME)
}

/// Chooses the process-wide theme once. Call after raw mode is on and before
/// crossterm polls input, so terminal replies are not parsed as keys.
pub(super) fn init() {
    ACTIVE_THEME.get_or_init(|| {
        let no_color = env::var_os("NO_COLOR");
        let omni_theme = env::var_os("OMNI_THEME");
        let colorfgbg = env::var_os("COLORFGBG");
        Theme::for_palette(select_palette(
            ThemeEnv {
                no_color: no_color.as_deref(),
                omni_theme: omni_theme.as_deref(),
                colorfgbg: colorfgbg.as_deref(),
            },
            query_terminal_background,
        ))
    });
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ThemeEnv<'a> {
    pub(super) no_color: Option<&'a OsStr>,
    pub(super) omni_theme: Option<&'a OsStr>,
    pub(super) colorfgbg: Option<&'a OsStr>,
}

/// Non-empty `NO_COLOR` wins, then `OMNI_THEME=dark|light|mono`. Auto (default,
/// or any other value) checks `COLORFGBG`, then the terminal, then falls back to dark.
pub(super) fn select_palette(
    vars: ThemeEnv<'_>,
    query_background: impl FnOnce() -> Option<Background>,
) -> Palette {
    if vars.no_color.is_some_and(|value| !value.is_empty()) {
        return Palette::Mono;
    }
    let requested = vars.omni_theme.and_then(OsStr::to_str).map(str::trim);
    for (name, palette) in [
        ("dark", Palette::Dark),
        ("light", Palette::Light),
        ("mono", Palette::Mono),
    ] {
        if requested.is_some_and(|value| value.eq_ignore_ascii_case(name)) {
            return palette;
        }
    }
    let background = vars
        .colorfgbg
        .and_then(OsStr::to_str)
        .and_then(colorfgbg_background)
        .or_else(query_background)
        .unwrap_or(Background::Dark);
    match background {
        Background::Dark => Palette::Dark,
        Background::Light => Palette::Light,
    }
}

/// `COLORFGBG` is `fg;bg` (rxvt may add a middle field); the last field is the background.
pub(super) fn colorfgbg_background(value: &str) -> Option<Background> {
    match value.rsplit(';').next()?.trim().parse::<u8>().ok()? {
        0..=6 | 8 => Some(Background::Dark),
        7 | 9..=15 => Some(Background::Light),
        _ => None,
    }
}

/// Parses an OSC 11 reply (`ESC ] 11 ; rgb:R/G/B` ended by BEL or `ESC \`).
#[cfg(any(unix, test))]
pub(super) fn parse_background_reply(reply: &[u8]) -> Option<Background> {
    background_luminance(reply).map(|luminance| {
        if luminance < 0.5 {
            Background::Dark
        } else {
            Background::Light
        }
    })
}

#[cfg(any(unix, test))]
pub(super) fn background_luminance(reply: &[u8]) -> Option<f64> {
    const PREFIX: &[u8] = b"\x1b]11;";
    let start = reply
        .windows(PREFIX.len())
        .position(|window| window == PREFIX)?
        + PREFIX.len();
    let body = &reply[start..];
    let end = body.iter().position(|byte| matches!(byte, 0x07 | 0x1b))?;
    if body[end] == 0x1b && body.get(end + 1) != Some(&b'\\') {
        return None;
    }
    let mut channels = std::str::from_utf8(&body[..end])
        .ok()?
        .strip_prefix("rgb:")?
        .split('/')
        .map(channel_intensity);
    let red = channels.next()??;
    let green = channels.next()??;
    let blue = channels.next()??;
    if channels.next().is_some() {
        return None;
    }
    Some(relative_luminance(red, green, blue))
}

#[cfg(any(unix, test))]
fn channel_intensity(hex: &str) -> Option<f64> {
    if !(1..=4).contains(&hex.len()) || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let value = u16::from_str_radix(hex, 16).ok()?;
    let max = u16::MAX >> (16 - 4 * hex.len());
    Some(f64::from(value) / f64::from(max))
}

/// WCAG relative luminance of sRGB channels in `0.0..=1.0`.
#[cfg(any(unix, test))]
pub(super) fn relative_luminance(red: f64, green: f64, blue: f64) -> f64 {
    let linear = |channel: f64| {
        if channel <= 0.040_45 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(red) + 0.7152 * linear(green) + 0.0722 * linear(blue)
}

/// True once the buffer ends with a primary device attributes reply (`ESC [ ? 62 ; 22 c`).
#[cfg(any(unix, test))]
pub(super) fn ends_with_device_attributes(reply: &[u8]) -> bool {
    let Some((&b'c', body)) = reply.split_last() else {
        return false;
    };
    let Some(start) = body.windows(3).rposition(|window| window == b"\x1b[?") else {
        return false;
    };
    let params = &body[start + 3..];
    !params.is_empty()
        && params
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b';')
}

/// Sends OSC 11 then DA1. Every terminal answers DA1, so the DA1 reply ends the
/// wait early when OSC 11 is unsupported; the timeout covers silent terminals.
#[cfg(unix)]
fn query_terminal_background() -> Option<Background> {
    use std::{
        io::IsTerminal,
        time::{Duration, Instant},
    };

    use rustix::{
        event::{PollFd, PollFlags, Timespec, poll},
        io::{Errno, read},
    };

    const QUERY: &[u8] = b"\x1b]11;?\x1b\\\x1b[c";
    const TIMEOUT: Duration = Duration::from_millis(100);
    const MAX_REPLY_BYTES: usize = 512;

    let stdin = io::stdin();
    if !stdin.is_terminal() || !io::stdout().is_terminal() {
        return None;
    }
    // False on timeout, error, or hangup without input.
    let readable = |timeout: Duration| loop {
        let Ok(timeout) = Timespec::try_from(timeout) else {
            return false;
        };
        let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
        match poll(&mut fds, Some(&timeout)) {
            Ok(_) => return fds[0].revents().contains(PollFlags::IN),
            Err(Errno::INTR) => {}
            Err(_) => return false,
        }
    };
    // Type-ahead would sit before the reply and be consumed; keep it for the picker instead.
    if readable(Duration::ZERO) {
        return None;
    }
    {
        let mut stdout = io::stdout().lock();
        if stdout.write_all(QUERY).is_err() || stdout.flush().is_err() {
            return None;
        }
    }
    let deadline = Instant::now() + TIMEOUT;
    let mut reply = Vec::with_capacity(64);
    // One byte per read, so keys typed after the DA1 reply stay queued for crossterm.
    while reply.len() < MAX_REPLY_BYTES && !ends_with_device_attributes(&reply) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || !readable(remaining) {
            break;
        }
        let mut byte = [0_u8; 1];
        match read(&stdin, &mut byte) {
            Ok(1) => reply.push(byte[0]),
            Err(Errno::INTR | Errno::AGAIN) => {}
            _ => break,
        }
    }
    parse_background_reply(&reply)
}

#[cfg(not(unix))]
const fn query_terminal_background() -> Option<Background> {
    None
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    const ROLES: [DetailStyle; 10] = [
        DetailStyle::Normal,
        DetailStyle::Muted,
        DetailStyle::Accent,
        DetailStyle::Strong,
        DetailStyle::Selected,
        DetailStyle::Danger,
        DetailStyle::Warning,
        DetailStyle::Match,
        DetailStyle::SelectedMatch,
        DetailStyle::Border,
    ];

    fn vars<'a>(
        no_color: Option<&'a str>,
        omni_theme: Option<&'a str>,
        colorfgbg: Option<&'a str>,
    ) -> ThemeEnv<'a> {
        ThemeEnv {
            no_color: no_color.map(OsStr::new),
            omni_theme: omni_theme.map(OsStr::new),
            colorfgbg: colorfgbg.map(OsStr::new),
        }
    }

    fn no_query() -> Option<Background> {
        panic!("terminal query must not run");
    }

    #[test]
    fn no_color_and_explicit_theme_skip_detection() {
        let light = Some("0;15");
        assert_eq!(
            select_palette(vars(Some("1"), Some("light"), light), no_query),
            Palette::Mono
        );
        assert_eq!(
            select_palette(vars(Some(""), Some("light"), None), no_query),
            Palette::Light
        );
        assert_eq!(
            select_palette(vars(None, Some("dark"), light), no_query),
            Palette::Dark
        );
        assert_eq!(
            select_palette(vars(None, Some(" MONO "), light), no_query),
            Palette::Mono
        );
    }

    #[test]
    fn auto_theme_prefers_colorfgbg_then_terminal_then_dark() {
        for theme in [None, Some("auto"), Some("solarized")] {
            assert_eq!(
                select_palette(vars(None, theme, Some("0;15")), no_query),
                Palette::Light
            );
            assert_eq!(
                select_palette(vars(None, theme, Some("15;default;0")), no_query),
                Palette::Dark
            );
        }
        let queried = Cell::new(false);
        let query_light = || {
            queried.set(true);
            Some(Background::Light)
        };
        assert_eq!(
            select_palette(vars(None, None, Some("15;default")), query_light),
            Palette::Light
        );
        assert!(queried.get());
        assert_eq!(
            select_palette(vars(None, None, None), || None),
            Palette::Dark
        );
    }

    #[test]
    fn colorfgbg_maps_last_field_to_background() {
        for dark in ["15;0", "7;6", "0;8", "15;default;0"] {
            assert_eq!(colorfgbg_background(dark), Some(Background::Dark), "{dark}");
        }
        for light in ["0;7", "0;9", "0;15", "0;default;15"] {
            assert_eq!(
                colorfgbg_background(light),
                Some(Background::Light),
                "{light}"
            );
        }
        for unknown in ["", "default", "0;16", "0;-1", "0;x"] {
            assert_eq!(colorfgbg_background(unknown), None, "{unknown}");
        }
    }

    #[test]
    fn osc11_reply_parses_channel_widths_and_terminators() {
        let cases: [(&[u8], Background); 6] = [
            (b"\x1b]11;rgb:0000/0000/0000\x1b\\", Background::Dark),
            (b"\x1b]11;rgb:ffff/ffff/ffff\x07", Background::Light),
            (b"\x1b]11;rgb:00/2b/36\x07", Background::Dark),
            (b"\x1b]11;rgb:fd/f6/e3\x1b\\", Background::Light),
            (b"\x1b]11;rgb:f/f/f\x07", Background::Light),
            (b"\x1b]11;rgb:333/333/333\x07", Background::Dark),
        ];
        for (reply, expected) in cases {
            assert_eq!(parse_background_reply(reply), Some(expected), "{reply:?}");
        }
        let luminance = background_luminance(b"\x1b]11;rgb:ffff/ffff/ffff\x07").expect("white");
        assert!((luminance - 1.0).abs() < 1e-9);
        // Mid greys straddle the 0.5 luminance threshold.
        assert_eq!(
            parse_background_reply(b"\x1b]11;rgb:bbbb/bbbb/bbbb\x07"),
            Some(Background::Dark)
        );
        assert_eq!(
            parse_background_reply(b"\x1b]11;rgb:c0c0/c0c0/c0c0\x07"),
            Some(Background::Light)
        );
        assert_eq!(
            parse_background_reply(b"typed\x1b]11;rgb:ffff/ffff/ffff\x1b\\\x1b[?62;22c"),
            Some(Background::Light)
        );
    }

    #[test]
    fn osc11_reply_rejects_garbage() {
        let garbage: [&[u8]; 11] = [
            b"",
            b"\x1b[?62;22c",
            b"\x1b]11;rgb:ffff/ffff/ffff",
            b"\x1b]11;rgb:ffff/ffff/ffff\x1bX",
            b"\x1b]11;rgb:ffff/ffff\x07",
            b"\x1b]11;rgb:ffff/ffff/ffff/ffff\x07",
            b"\x1b]11;rgb:fffff/ffff/ffff\x07",
            b"\x1b]11;rgb:gg/00/00\x07",
            b"\x1b]11;rgb://\x07",
            b"\x1b]11;?\x1b\\",
            b"\x1b]10;rgb:ffff/ffff/ffff\x07",
        ];
        for reply in garbage {
            assert_eq!(parse_background_reply(reply), None, "{reply:?}");
        }
    }

    #[test]
    fn device_attributes_reply_ends_the_query() {
        assert!(ends_with_device_attributes(b"\x1b[?62;22c"));
        assert!(ends_with_device_attributes(b"\x1b[?6c"));
        assert!(ends_with_device_attributes(
            b"\x1b]11;rgb:0000/0000/0000\x1b\\\x1b[?1;2c"
        ));
        for partial in [
            &b""[..],
            b"c",
            b"\x1b[c",
            b"\x1b[?c",
            b"\x1b[?62;2",
            b"\x1b[?62;22cq",
            b"\x1b[?62;22u",
            b"\x1b[?62:22c",
            b"\x1b]11;rgb:0000/0000/0000\x1b\\abc",
        ] {
            assert!(!ends_with_device_attributes(partial), "{partial:?}");
        }
    }

    fn xterm_rgb(index: u8) -> (u8, u8, u8) {
        const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        match index {
            16..=231 => {
                let cube = index - 16;
                (
                    LEVELS[usize::from(cube / 36)],
                    LEVELS[usize::from(cube / 6 % 6)],
                    LEVELS[usize::from(cube % 6)],
                )
            }
            232..=255 => {
                let grey = 8 + (index - 232) * 10;
                (grey, grey, grey)
            }
            _ => panic!("base ANSI color {index} is remapped by terminal themes"),
        }
    }

    fn ansi_index(color: Color) -> u8 {
        let Color::AnsiValue(index) = color else {
            panic!("palette colors must be 256-color indices: {color:?}");
        };
        index
    }

    fn contrast(color: Color, background: (u8, u8, u8)) -> f64 {
        let luminance = |(red, green, blue): (u8, u8, u8)| {
            relative_luminance(
                f64::from(red) / 255.0,
                f64::from(green) / 255.0,
                f64::from(blue) / 255.0,
            )
        };
        let (first, second) = (
            luminance(xterm_rgb(ansi_index(color))),
            luminance(background),
        );
        (first.max(second) + 0.05) / (first.min(second) + 0.05)
    }

    fn colors(theme: &Theme) -> impl Iterator<Item = Color> + '_ {
        ROLES.into_iter().flat_map(|role| {
            let style = theme.style(role);
            style.foreground.into_iter().chain(style.background)
        })
    }

    #[test]
    fn palettes_use_256_colors_with_readable_muted_text() {
        const WHITE: (u8, u8, u8) = (255, 255, 255);
        const SOLARIZED_LIGHT: (u8, u8, u8) = (0xfd, 0xf6, 0xe3);
        const BLACK: (u8, u8, u8) = (0, 0, 0);
        const SOLARIZED_DARK: (u8, u8, u8) = (0x00, 0x2b, 0x36);
        for palette in [Palette::Dark, Palette::Light] {
            for color in colors(&Theme::for_palette(palette)) {
                // Panics for DarkGrey, ANSI 8, and every other remappable base color.
                xterm_rgb(ansi_index(color));
            }
        }
        let dark = Theme::for_palette(Palette::Dark);
        let light = Theme::for_palette(Palette::Light);
        let muted = |theme: &Theme| {
            theme
                .style(DetailStyle::Muted)
                .foreground
                .expect("muted color")
        };
        for background in [BLACK, SOLARIZED_DARK] {
            assert!(contrast(muted(&dark), background) >= 4.5);
        }
        for background in [WHITE, SOLARIZED_LIGHT] {
            for role in [
                DetailStyle::Muted,
                DetailStyle::Accent,
                DetailStyle::Danger,
                DetailStyle::Warning,
                DetailStyle::Match,
            ] {
                let color = light.style(role).foreground.expect("light text color");
                assert!(contrast(color, background) >= 4.5, "{role:?}");
            }
        }
        for color in colors(&light) {
            let index = ansi_index(color);
            let (red, green, blue) = xterm_rgb(index);
            assert!(
                !(red >= 135 && green >= 135 && blue < red.min(green)),
                "light palette uses yellow {index}"
            );
        }
    }

    #[test]
    fn mono_palette_emits_attributes_without_color() {
        let mono = Theme::for_palette(Palette::Mono);
        assert_eq!(colors(&mono).count(), 0);
        for role in ROLES {
            let mut output = Vec::new();
            mono.style(role).queue(&mut output).expect("queue style");
            let sequences = String::from_utf8(output).expect("ascii escapes");
            for sequence in sequences.split("\x1b[").filter(|part| !part.is_empty()) {
                assert!(
                    matches!(sequence, "1m" | "2m" | "4m" | "7m"),
                    "{role:?} emitted {sequence:?}"
                );
            }
        }
        let selected = mono.style(DetailStyle::Selected);
        assert!(selected.bold && selected.reverse);
        assert!(mono.style(DetailStyle::Muted).dim);
        assert!(mono.style(DetailStyle::Match).underline);
    }

    #[test]
    fn dark_muted_emits_256_color_grey() {
        let mut output = Vec::new();
        Theme::for_palette(Palette::Dark)
            .style(DetailStyle::Muted)
            .queue(&mut output)
            .expect("queue style");
        assert_eq!(output, b"\x1b[38;5;247m");
        assert_eq!(current().palette, Palette::Dark);
    }
}
