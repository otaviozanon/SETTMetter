use winaudio::Win32::Foundation::COLORREF;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    Dark,
    Light,
}

/// Follows the Windows app theme, the same one the taskbar widget uses.
pub fn current() -> Theme {
    Theme::from_mode(if crate::settings::light_theme() {
        ThemeMode::Light
    } else {
        ThemeMode::Dark
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgb(hex: u32) -> Self {
        Self {
            r: ((hex >> 16) & 0xFF) as u8,
            g: ((hex >> 8) & 0xFF) as u8,
            b: (hex & 0xFF) as u8,
            a: 255,
        }
    }

    pub const fn rgba(hex: u32) -> Self {
        Self {
            r: ((hex >> 24) & 0xFF) as u8,
            g: ((hex >> 16) & 0xFF) as u8,
            b: ((hex >> 8) & 0xFF) as u8,
            a: (hex & 0xFF) as u8,
        }
    }

    pub const fn colorref(self) -> COLORREF {
        COLORREF((self.r as u32) | ((self.g as u32) << 8) | ((self.b as u32) << 16))
    }

    pub fn alpha_f32(self) -> f32 {
        f32::from(self.a) / 255.0
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub mode: ThemeMode,
    pub bg: Color,
    pub bg_elev: Color,
    pub bg_hover: Color,
    pub bg_recess: Color,
    pub bg_pill: Color,
    pub border: Color,
    pub border_strong: Color,
    pub check_border: Color,
    pub divider: Color,
    pub text: Color,
    pub text_sec: Color,
    pub text_muted: Color,
    pub inv_bg: Color,
    pub inv_text: Color,
    pub inv_hover: Color,
    pub tactile_bg: Color,
    pub tactile_fill: Color,
    pub tactile_fill_muted: Color,
    pub tactile_edge: Color,
    pub tactile_edge_muted: Color,
    pub pin_soft_bg: Color,
    pub mute_hover_bg: Color,
    pub listen_popover_bg: Color,
    pub listen_default_bg: Color,
    pub listen_default_hover: Color,
    pub listen_default_active: Color,
    pub listen_default_border: Color,
    pub shadow_alpha: f32,
}

impl Theme {
    pub const fn from_mode(mode: ThemeMode) -> Self {
        match mode {
            ThemeMode::Dark => Self {
                mode: ThemeMode::Dark,
                bg: Color::rgb(0x121212),
                bg_elev: Color::rgb(0x191919),
                bg_hover: Color::rgb(0x212121),
                bg_recess: Color::rgb(0x0C0C0C),
                bg_pill: Color::rgb(0x262626),
                border: Color::rgb(0x262626),
                border_strong: Color::rgb(0x383838),
                check_border: Color::rgb(0x555555),
                divider: Color::rgb(0x202020),
                text: Color::rgb(0xF5F5F5),
                text_sec: Color::rgb(0xA3A3A3),
                text_muted: Color::rgb(0x737373),
                inv_bg: Color::rgb(0xF5F5F5),
                inv_text: Color::rgb(0x0A0A0A),
                inv_hover: Color::rgb(0xE5E5E5),
                tactile_bg: Color::rgb(0x151515),
                tactile_fill: Color::rgb(0x2E2E2E),
                tactile_fill_muted: Color::rgba(0x2E2E2E40),
                tactile_edge: Color::rgba(0xE5E5E5D9),
                tactile_edge_muted: Color::rgba(0xE5E5E559),
                pin_soft_bg: Color::rgba(0xFFFFFF17),
                mute_hover_bg: Color::rgba(0x80808033),
                listen_popover_bg: Color::rgb(0x121212),
                listen_default_bg: Color::rgb(0x272727),
                listen_default_hover: Color::rgb(0x323232),
                listen_default_active: Color::rgb(0x353535),
                listen_default_border: Color::rgb(0x383838),
                shadow_alpha: 0.45,
            },
            ThemeMode::Light => Self {
                mode: ThemeMode::Light,
                bg: Color::rgb(0xFFFFFF),
                bg_elev: Color::rgb(0xF7F7F7),
                bg_hover: Color::rgb(0xEFEFEF),
                bg_recess: Color::rgb(0xF0F0F0),
                bg_pill: Color::rgb(0xFFFFFF),
                border: Color::rgb(0xDEDEDE),
                border_strong: Color::rgb(0xC6C6C6),
                check_border: Color::rgb(0x9A9A9A),
                divider: Color::rgb(0xEAEAEA),
                text: Color::rgb(0x111111),
                text_sec: Color::rgb(0x525252),
                text_muted: Color::rgb(0x737373),
                inv_bg: Color::rgb(0x141414),
                inv_text: Color::rgb(0xFFFFFF),
                inv_hover: Color::rgb(0x2A2A2A),
                tactile_bg: Color::rgb(0xEEEEEE),
                tactile_fill: Color::rgb(0xD4D4D4),
                tactile_fill_muted: Color::rgba(0xD4D4D440),
                tactile_edge: Color::rgba(0x141414D9),
                tactile_edge_muted: Color::rgba(0x14141459),
                pin_soft_bg: Color::rgba(0x0000000F),
                mute_hover_bg: Color::rgba(0x80808033),
                listen_popover_bg: Color::rgb(0xF5F5F5),
                listen_default_bg: Color::rgb(0xFFFFFF),
                listen_default_hover: Color::rgb(0xEFEFEF),
                listen_default_active: Color::rgb(0xEAEAEA),
                listen_default_border: Color::rgb(0xDCDCDC),
                shadow_alpha: 0.16,
            },
        }
    }
}
