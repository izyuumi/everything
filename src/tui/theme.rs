use ratatui::style::{Color, Modifier, Style};

pub const GLYPH: &str = "◈";
pub const AGENT: &str = "◈ Vorra";

pub const BORDER: Color = Color::Rgb(0xCD, 0x7F, 0x32);
pub const TITLE: Color = Color::Rgb(0xFF, 0xD7, 0x00);
pub const ACCENT: Color = Color::Rgb(0xFF, 0xBF, 0x00);
pub const DIM: Color = Color::Rgb(0xB8, 0x86, 0x0B);
pub const TEXT: Color = Color::Rgb(0xFF, 0xF8, 0xDC);
pub const LABEL: Color = Color::Rgb(0xDA, 0xA5, 0x20);
pub const OK: Color = Color::Rgb(0x4C, 0xAF, 0x50);
pub const ERROR: Color = Color::Rgb(0xEF, 0x53, 0x50);
pub const WARN: Color = Color::Rgb(0xFF, 0xA7, 0x26);
pub const MUTED: Color = Color::Rgb(0x8B, 0x86, 0x82);
pub const BAR_BG: Color = Color::Rgb(0x1A, 0x1A, 0x2E);
pub const SELECT_BG: Color = Color::Rgb(0x33, 0x33, 0x55);
pub const CODE_BG: Color = Color::Rgb(0x23, 0x23, 0x2E);
pub const CODE: Color = Color::Rgb(0xE6, 0xDB, 0x74);

pub const LOGO_GRADIENT: [Color; 6] = [
    Color::Rgb(0xFF, 0xD7, 0x00),
    Color::Rgb(0xFF, 0xCC, 0x00),
    Color::Rgb(0xFF, 0xBF, 0x00),
    Color::Rgb(0xF0, 0xA8, 0x30),
    Color::Rgb(0xDA, 0x8F, 0x35),
    Color::Rgb(0xCD, 0x7F, 0x32),
];

pub fn fg(color: Color) -> Style {
    Style::default().fg(color)
}

pub fn bold(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}
