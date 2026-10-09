use gpui::{Hsla, Rgba};

use super::schema::ColorsConfig;

/// Resolved colors — the runtime type the renderer uses.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub background: Hsla,
    pub foreground: Hsla,
    pub cursor: Hsla,
    pub selection_bg: Hsla,
    /// Text inside a selection; `None` keeps each cell's own colour.
    pub selection_fg: Option<Hsla>,
    /// Indices 0-7 normal, 8-15 bright.
    pub ansi: [Hsla; 16],
    /// The preset this was built from, after light/dark resolution.
    pub preset: &'static str,
}

/// [background, foreground, cursor, selection_bg, ansi 0-15]
type Palette = [&'static str; 20];

const CATPPUCCIN_MOCHA: Palette = [
    "#11111b", "#cdd6f4", "#f5e0dc", "#414458", "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af",
    "#89b4fa", "#cba6f7", "#94e2d5", "#bac2de", "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af",
    "#89b4fa", "#cba6f7", "#94e2d5", "#a6adc8",
];
const CATPPUCCIN_LATTE: Palette = [
    "#eff1f5", "#4c4f69", "#dc8a78", "#bcc0cc", "#5c5f77", "#d20f39", "#40a02b", "#df8e1d",
    "#1e66f5", "#8839ef", "#179299", "#acb0be", "#6c6f85", "#d20f39", "#40a02b", "#df8e1d",
    "#1e66f5", "#8839ef", "#179299", "#bcc0cc",
];
const GRUVBOX_DARK: Palette = [
    "#282828", "#ebdbb2", "#ebdbb2", "#504945", "#282828", "#cc241d", "#98971a", "#d79921",
    "#458588", "#b16286", "#689d6a", "#a89984", "#928374", "#fb4934", "#b8bb26", "#fabd2f",
    "#83a598", "#d3869b", "#8ec07c", "#ebdbb2",
];
const TOKYONIGHT: Palette = [
    "#1a1b26", "#c0caf5", "#c0caf5", "#33467c", "#15161e", "#f7768e", "#9ece6a", "#e0af68",
    "#7aa2f7", "#bb9af7", "#7dcfff", "#a9b1d6", "#414868", "#f7768e", "#9ece6a", "#e0af68",
    "#7aa2f7", "#bb9af7", "#7dcfff", "#c0caf5",
];
const DRACULA: Palette = [
    "#282a36", "#f8f8f2", "#f8f8f2", "#44475a", "#21222c", "#ff5555", "#50fa7b", "#f1fa8c",
    "#bd93f9", "#ff79c6", "#8be9fd", "#f8f8f2", "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5",
    "#d6acff", "#ff92df", "#a4ffff", "#ffffff",
];
const NORD: Palette = [
    "#2e3440", "#d8dee9", "#d8dee9", "#434c5e", "#3b4252", "#bf616a", "#a3be8c", "#ebcb8b",
    "#81a1c1", "#b48ead", "#88c0d0", "#e5e9f0", "#4c566a", "#bf616a", "#a3be8c", "#ebcb8b",
    "#81a1c1", "#b48ead", "#8fbcbb", "#eceff4",
];
const SOLARIZED_DARK: Palette = [
    "#002b36", "#839496", "#839496", "#073642", "#073642", "#dc322f", "#859900", "#b58900",
    "#268bd2", "#d33682", "#2aa198", "#eee8d5", "#586e75", "#cb4b16", "#859900", "#b58900",
    "#268bd2", "#6c71c4", "#2aa198", "#fdf6e3",
];
const OMNIPTY: Palette = [
    "#100d0c", "#e8ddd5", "#fab387", "#4a3428", "#3a2e28", "#e2725b", "#a6b86a", "#e5a458",
    "#7d9bb8", "#c78a92", "#8fb0a0", "#c9bcb2", "#5c4a3d", "#f0876f", "#b8cc7a", "#f5b96a",
    "#93b3d4", "#dda0aa", "#a3c9b6", "#e8ddd5",
];
// Omarchy's built-in themes, mapped the way its alacritty template does:
// black = background, white = foreground, bright black = muted.
const ETHEREAL: Palette = [
    "#060b1e", "#ffcead", "#ffcead", "#252e56", "#060b1e", "#ed5b5a", "#92a593", "#e9bb4f",
    "#7d82d9", "#c89dc1", "#a3bfd1", "#ffcead", "#6d7db6", "#faaaa9", "#c4cfc4", "#f7dc9c",
    "#c2c4f0", "#ead7e7", "#dfeaf0", "#ffcead",
];
const EVERFOREST: Palette = [
    "#2d353b", "#d3c6aa", "#d3c6aa", "#3d484d", "#2d353b", "#e67e80", "#a7c080", "#dbbc7f",
    "#7fbbb3", "#d699b6", "#83c092", "#d3c6aa", "#475258", "#e67e80", "#a7c080", "#dbbc7f",
    "#7fbbb3", "#d699b6", "#83c092", "#d3c6aa",
];
const FLEXOKI_LIGHT: Palette = [
    "#fffcf0", "#100f0f", "#100f0f", "#cecdc3", "#fffcf0", "#d14d41", "#879a39", "#d0a215",
    "#205ea6", "#ce5d97", "#3aa99f", "#100f0f", "#b7b5ac", "#d14d41", "#879a39", "#d0a215",
    "#4385be", "#ce5d97", "#3aa99f", "#100f0f",
];
const HACKERMAN: Palette = [
    "#0b0c16", "#ddf7ff", "#ddf7ff", "#1f253a", "#0b0c16", "#50f872", "#4fe88f", "#50f7d4",
    "#829dd4", "#86a7df", "#7cf8f7", "#ddf7ff", "#2d3450", "#85ff9d", "#9cf7c2", "#a4ffec",
    "#c4d2ed", "#cddbf4", "#d1fffe", "#ddf7ff",
];
const KANAGAWA: Palette = [
    "#1f1f28", "#dcd7ba", "#dcd7ba", "#363646", "#1f1f28", "#c34043", "#76946a", "#c0a36e",
    "#7e9cd8", "#957fb8", "#6a9589", "#dcd7ba", "#54546d", "#e82424", "#98bb6c", "#e6c384",
    "#7fb4ca", "#938aa9", "#7aa89f", "#dcd7ba",
];
const LAST_HORIZON: Palette = [
    "#0c0b0c", "#fafcfb", "#e2dddc", "#584e51", "#0c0b0c", "#c38b7b", "#87a9b0", "#6b5e73",
    "#b59790", "#c4d8e2", "#a5a0b6", "#fafcfb", "#584e51", "#c38b7b", "#87a9b0", "#6b5e73",
    "#b59790", "#c4d8e2", "#a5a0b6", "#e2dddc",
];
const LUMON: Palette = [
    "#16242d", "#d6e2ee", "#f2fcff", "#243d56", "#16242d", "#4d86b0", "#5e95bc", "#6fa4c9",
    "#6fb8e3", "#8bc9eb", "#b4e4f6", "#d6e2ee", "#304860", "#73a6cb", "#86b7d8", "#9dcae5",
    "#f2fcff", "#b1d8ee", "#d1eef8", "#f2fcff",
];
const LUPINE: Palette = [
    "#fafafa", "#212121", "#000000", "#d0d0d0", "#fafafa", "#c900c4", "#4a2fd0", "#026fde",
    "#3264eb", "#8a4ad7", "#0c67de", "#212121", "#9e9e9e", "#f930fb", "#9f85e0", "#358fff",
    "#5482ff", "#b363ff", "#3986ff", "#000000",
];
const MATTE_BLACK: Palette = [
    "#121212", "#bebebe", "#bebebe", "#2a2a2a", "#121212", "#d35f5f", "#ffc107", "#b91c1c",
    "#e68e0d", "#d35f5f", "#bebebe", "#bebebe", "#333333", "#b91c1c", "#ffc107", "#b90a0a",
    "#f59e0b", "#b91c1c", "#eaeaea", "#bebebe",
];
const MIASMA: Palette = [
    "#222222", "#c2c2b0", "#c2c2b0", "#383838", "#222222", "#685742", "#5f875f", "#b36d43",
    "#78824b", "#bb7744", "#c9a554", "#c2c2b0", "#666666", "#685742", "#5f875f", "#b36d43",
    "#78824b", "#bb7744", "#c9a554", "#c2c2b0",
];
const OSAKA_JADE: Palette = [
    "#111c18", "#c1c497", "#f7e8b2", "#32473b", "#111c18", "#ff5345", "#549e6a", "#459451",
    "#509475", "#d2689c", "#2dd5b7", "#c1c497", "#53685b", "#db9f9c", "#63b07a", "#e5c736",
    "#acd4cf", "#75bbb3", "#8cd3cb", "#f7e8b2",
];
const RETRO_82: Palette = [
    "#05182e", "#f6dcac", "#f6dcac", "#134e5a", "#05182e", "#f85525", "#028391", "#e97b3c",
    "#3f8f8a", "#3f8f8a", "#8cbfb8", "#f6dcac", "#2a6b78", "#f85525", "#028391", "#e97b3c",
    "#faa968", "#3f8f8a", "#8cbfb8", "#f6dcac",
];
const RISTRETTO: Palette = [
    "#2c2525", "#e6d9db", "#e6d9db", "#403e41", "#2c2525", "#fd6883", "#adda78", "#f9cc6c",
    "#f38d70", "#a8a9eb", "#85dacc", "#e6d9db", "#72696a", "#ff8297", "#c8e292", "#fcd675",
    "#f8a788", "#bebffd", "#9bf1e1", "#e6d9db",
];
const ROSE_PINE_DAWN: Palette = [
    "#faf4ed", "#575279", "#575279", "#dfdad9", "#faf4ed", "#b4637a", "#286983", "#ea9d34",
    "#56949f", "#907aa9", "#d7827e", "#575279", "#cecacd", "#b4637a", "#286983", "#ea9d34",
    "#56949f", "#907aa9", "#d7827e", "#575279",
];
const SOLITUDE: Palette = [
    "#101315", "#cacccc", "#a5aeb4", "#343d41", "#101315", "#565d60", "#9fa5a9", "#d9dbdc",
    "#798186", "#aeaeae", "#707070", "#cacccc", "#4b4e55", "#de6145", "#343d41", "#c9c2b4",
    "#5d6367", "#9a9a9a", "#707070", "#a5aeb4",
];
const VANTABLACK: Palette = [
    "#000000", "#ffffff", "#ffffff", "#1a1a1a", "#000000", "#a4a4a4", "#b6b6b6", "#cecece",
    "#8d8d8d", "#9b9b9b", "#b0b0b0", "#ffffff", "#7a7a7a", "#a4a4a4", "#b6b6b6", "#cecece",
    "#8d8d8d", "#9b9b9b", "#b0b0b0", "#ffffff",
];
const WHITE: Palette = [
    "#ffffff", "#000000", "#000000", "#c0c0c0", "#ffffff", "#2a2a2a", "#3a3a3a", "#4a4a4a",
    "#1a1a1a", "#2e2e2e", "#3e3e3e", "#000000", "#808080", "#2a2a2a", "#3a3a3a", "#4a4a4a",
    "#1a1a1a", "#2e2e2e", "#3e3e3e", "#000000",
];

pub const PRESET_NAMES: &[&str] = &[
    "catppuccin-mocha",
    "catppuccin-latte",
    "dracula",
    "ethereal",
    "everforest",
    "flexoki-light",
    "gruvbox-dark",
    "hackerman",
    "kanagawa",
    "last-horizon",
    "lumon",
    "lupine",
    "matte-black",
    "miasma",
    "nord",
    "osaka-jade",
    "omnipty",
    "retro-82",
    "ristretto",
    "rose-pine-dawn",
    "solarized-dark",
    "solitude",
    "tokyonight",
    "vantablack",
    "white",
];

/// The listed name for a preset, or None for an unknown one. Accepts
/// `oxide`, the palette's name before the rename, so configs keep working.
pub fn canonical_preset(name: &str) -> Option<&'static str> {
    let name = if name == "oxide" { "omnipty" } else { name };
    PRESET_NAMES.iter().copied().find(|p| *p == name)
}

fn preset(name: &str) -> Option<&'static Palette> {
    match name {
        "catppuccin-latte" => Some(&CATPPUCCIN_LATTE),
        "catppuccin-mocha" => Some(&CATPPUCCIN_MOCHA),
        "dracula" => Some(&DRACULA),
        "ethereal" => Some(&ETHEREAL),
        "everforest" => Some(&EVERFOREST),
        "flexoki-light" => Some(&FLEXOKI_LIGHT),
        "gruvbox-dark" => Some(&GRUVBOX_DARK),
        "hackerman" => Some(&HACKERMAN),
        "kanagawa" => Some(&KANAGAWA),
        "last-horizon" => Some(&LAST_HORIZON),
        "lumon" => Some(&LUMON),
        "lupine" => Some(&LUPINE),
        "matte-black" => Some(&MATTE_BLACK),
        "miasma" => Some(&MIASMA),
        "nord" => Some(&NORD),
        "osaka-jade" => Some(&OSAKA_JADE),
        "omnipty" => Some(&OMNIPTY),
        "retro-82" => Some(&RETRO_82),
        "ristretto" => Some(&RISTRETTO),
        "rose-pine-dawn" => Some(&ROSE_PINE_DAWN),
        "solarized-dark" => Some(&SOLARIZED_DARK),
        "solitude" => Some(&SOLITUDE),
        "tokyonight" => Some(&TOKYONIGHT),
        "vantablack" => Some(&VANTABLACK),
        "white" => Some(&WHITE),
        _ => None,
    }
}

pub fn parse_hex(s: &str) -> Option<Hsla> {
    Rgba::try_from(s.trim()).ok().map(Hsla::from)
}

impl Theme {
    /// The theme for the plain `preset` key, ignoring `follow_system`.
    pub fn from_config(c: &ColorsConfig) -> Self {
        Self::from_preset(c, c.preset.as_deref())
    }

    /// The preset name `[colors]` selects for the given appearance.
    pub fn preset_for(c: &ColorsConfig, dark: bool) -> Option<&str> {
        if !c.follow_system {
            return c.preset.as_deref();
        }
        let variant = if dark {
            c.preset_dark.as_deref()
        } else {
            c.preset_light.as_deref()
        };
        variant.or(c.preset.as_deref())
    }

    /// Resolve against the system appearance when `follow_system` is on:
    /// dark picks `preset_dark`, light picks `preset_light`, each falling
    /// back to `preset`. When the chosen variant isn't named, dark uses the
    /// default palette and light uses `catppuccin-latte`, so the bare
    /// `follow_system = true` does something visible.
    pub fn resolve(c: &ColorsConfig, dark: bool) -> Self {
        let name = Self::preset_for(c, dark)
            .map(str::to_string)
            .or_else(|| (c.follow_system && !dark).then(|| "catppuccin-latte".to_string()));
        Self::from_preset(c, name.as_deref())
    }

    fn from_preset(c: &ColorsConfig, name: Option<&str>) -> Self {
        // Unknown preset names fall back to the default palette; the config
        // loader surfaces a banner for that case.
        let (preset_name, base) = name
            .and_then(canonical_preset)
            .map(|p| (p, preset(p).unwrap()))
            .unwrap_or(("catppuccin-mocha", &CATPPUCCIN_MOCHA));
        let pick = |explicit: &Option<String>, base_ix: usize| -> Hsla {
            explicit
                .as_deref()
                .and_then(parse_hex)
                .unwrap_or_else(|| parse_hex(base[base_ix]).unwrap())
        };
        Self {
            background: pick(&c.background, 0),
            foreground: pick(&c.foreground, 1),
            cursor: pick(&c.cursor, 2),
            selection_bg: pick(&c.selection_bg, 3),
            selection_fg: c.selection_fg.as_deref().and_then(parse_hex),
            preset: preset_name,
            ansi: [
                pick(&c.black, 4),
                pick(&c.red, 5),
                pick(&c.green, 6),
                pick(&c.yellow, 7),
                pick(&c.blue, 8),
                pick(&c.magenta, 9),
                pick(&c.cyan, 10),
                pick(&c.white, 11),
                pick(&c.bright_black, 12),
                pick(&c.bright_red, 13),
                pick(&c.bright_green, 14),
                pick(&c.bright_yellow, 15),
                pick(&c.bright_blue, 16),
                pick(&c.bright_magenta, 17),
                pick(&c.bright_cyan, 18),
                pick(&c.bright_white, 19),
            ],
        }
    }
}

/// Convert to 8-bit RGB, for answering OSC color queries.
pub fn hsla_to_rgb8(color: Hsla) -> (u8, u8, u8) {
    let rgba: Rgba = color.into();
    (
        (rgba.r * 255.0).round() as u8,
        (rgba.g * 255.0).round() as u8,
        (rgba.b * 255.0).round() as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_six_digit_hex() {
        let c = parse_hex("#ff0000").unwrap();
        let rgba: Rgba = c.into();
        assert!((rgba.r - 1.0).abs() < 0.01 && rgba.g < 0.01 && rgba.b < 0.01);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_hex("red").is_none());
        assert!(parse_hex("#12345").is_none());
    }

    #[test]
    fn all_presets_parse() {
        for name in PRESET_NAMES {
            let config = ColorsConfig {
                preset: Some(name.to_string()),
                ..Default::default()
            };
            let _ = Theme::from_config(&config); // pick() unwraps on bad hex
        }
    }

    #[test]
    fn old_name_resolves_to_the_renamed_palette() {
        assert_eq!(canonical_preset("oxide"), Some("omnipty"));
        assert_eq!(canonical_preset("nord"), Some("nord"));
        assert_eq!(canonical_preset("nope"), None);
        let c = ColorsConfig {
            preset: Some("oxide".into()),
            ..Default::default()
        };
        let theme = Theme::from_config(&c);
        assert_eq!(theme.preset, "omnipty");
        assert_eq!(theme.background, parse_hex(OMNIPTY[0]).unwrap());
    }

    #[test]
    fn explicit_color_overrides_preset() {
        let config = ColorsConfig {
            preset: Some("nord".into()),
            background: Some("#000000".into()),
            ..Default::default()
        };
        let theme = Theme::from_config(&config);
        let rgba: Rgba = theme.background.into();
        assert!(rgba.r < 0.01 && rgba.g < 0.01 && rgba.b < 0.01);
        // Foreground still comes from nord.
        assert_eq!(theme.foreground, parse_hex("#d8dee9").unwrap());
        assert_eq!(theme.preset, "nord");
        assert!(theme.selection_fg.is_none());
    }

    #[test]
    fn follow_system_picks_the_variant_for_the_appearance() {
        let config = ColorsConfig {
            preset: Some("nord".into()),
            preset_dark: Some("gruvbox-dark".into()),
            preset_light: Some("catppuccin-latte".into()),
            follow_system: true,
            selection_fg: Some("#000000".into()),
            ..Default::default()
        };
        assert_eq!(Theme::resolve(&config, true).preset, "gruvbox-dark");
        assert_eq!(Theme::resolve(&config, false).preset, "catppuccin-latte");
        assert!(
            Theme::resolve(&config, false).selection_fg.is_some(),
            "overrides apply to both variants"
        );
        // Off: the plain preset, whatever the appearance.
        let off = ColorsConfig {
            follow_system: false,
            ..config.clone()
        };
        assert_eq!(Theme::resolve(&off, false).preset, "nord");
        // On with only `preset`: dark keeps it, light falls back to latte.
        let bare = ColorsConfig {
            preset: Some("nord".into()),
            follow_system: true,
            ..Default::default()
        };
        assert_eq!(Theme::resolve(&bare, true).preset, "nord");
        assert_eq!(Theme::resolve(&bare, false).preset, "nord");
        let bare = ColorsConfig {
            follow_system: true,
            ..Default::default()
        };
        assert_eq!(Theme::resolve(&bare, true).preset, "catppuccin-mocha");
        assert_eq!(Theme::resolve(&bare, false).preset, "catppuccin-latte");
    }
}
