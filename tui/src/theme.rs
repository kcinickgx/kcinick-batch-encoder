//! Palette mirrored from the GUI (egui) to keep the same look in the TUI.
use ratatui::style::Color;

pub const ACCENT: Color = Color::Rgb(90, 170, 255); // selection / active blue
pub const VIDEO: Color = Color::Rgb(120, 200, 255);
pub const AUDIO: Color = Color::Rgb(140, 220, 150);
pub const VAL: Color = Color::Rgb(225, 225, 235);
pub const AMBER: Color = Color::Rgb(240, 200, 120); // accent (bitrate/speed/dur)
pub const ETA: Color = Color::Rgb(120, 230, 160);
pub const FAIL: Color = Color::Rgb(230, 110, 110);
pub const OK: Color = Color::Rgb(120, 220, 140);
pub const LABEL: Color = Color::Rgb(150, 150, 160); // "key:" labels
pub const BORDER: Color = Color::Rgb(95, 95, 108);
pub const STRONG: Color = Color::Rgb(225, 225, 235);
pub const DIM: Color = Color::Rgb(165, 165, 170);

pub const PANEL: Color = Color::Rgb(24, 24, 28); // "window" background
pub const TITLEBAR: Color = Color::Rgb(38, 40, 52); // title bar
pub const HOVER_BG: Color = Color::Rgb(48, 48, 55);
pub const SEL_BG: Color = Color::Rgb(46, 90, 150); // selected chip background
pub const TAB_INACTIVE: Color = Color::Rgb(34, 34, 40);
pub const FIELD_BG: Color = Color::Rgb(30, 30, 36);
pub const FIELD_FOCUS_BG: Color = Color::Rgb(20, 40, 66);

pub const CROP_REMOVED: Color = Color::Rgb(50, 50, 60);
pub const CROP_KEPT: Color = Color::Rgb(150, 195, 245);
pub const CROP_KEPT_SEL: Color = Color::Rgb(240, 244, 255);
