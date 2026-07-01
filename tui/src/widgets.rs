//! Drawing + interaction (hotspots) helpers for the TUI.
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::app::ElemId;
use crate::encoder::CropMode;
use crate::theme;

pub type Hotspots = Vec<(Rect, ElemId)>;

pub fn hit(hs: &Hotspots, col: u16, row: u16) -> Option<ElemId> {
    // last one that contains the point (modals are pushed last → they win)
    hs.iter()
        .rev()
        .find(|(r, _)| col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height)
        .map(|(_, id)| *id)
}

/// Selectable chip (Mode/Codec/Preset/Bitrate/Encoders): background per state.
#[allow(clippy::too_many_arguments)]
pub fn chip(
    f: &mut Frame,
    hs: &mut Hotspots,
    area: Rect,
    id: ElemId,
    label: &str,
    selected: bool,
    hover: bool,
    enabled: bool,
) {
    if enabled {
        hs.push((area, id));
    }
    let (bg, fg, bold) = if !enabled {
        (theme::PANEL, Color::Rgb(90, 90, 98), false)
    } else if selected {
        (theme::SEL_BG, Color::White, true)
    } else if hover {
        (theme::HOVER_BG, theme::STRONG, false)
    } else {
        (theme::PANEL, theme::DIM, false)
    };
    let mut st = Style::default().bg(bg).fg(fg);
    if bold {
        st = st.add_modifier(Modifier::BOLD);
    }
    let p = Paragraph::new(Line::from(label))
        .alignment(Alignment::Center)
        .style(st);
    f.render_widget(p, area);
}

/// Flat one-line button: `[ label ]`.
pub fn flat_button(
    f: &mut Frame,
    hs: &mut Hotspots,
    area: Rect,
    id: ElemId,
    label: &str,
    hover: bool,
    enabled: bool,
) {
    if enabled {
        hs.push((area, id));
    }
    let (bg, fg) = if !enabled {
        (theme::PANEL, Color::Rgb(90, 90, 98))
    } else if hover {
        (theme::ACCENT, Color::Black)
    } else {
        (theme::TITLEBAR, theme::STRONG)
    };
    let p = Paragraph::new(Line::from(label))
        .alignment(Alignment::Center)
        .style(Style::default().bg(bg).fg(fg));
    f.render_widget(p, area);
}

/// Large bordered button (Play/Stop): fills the whole area.
pub fn box_button(
    f: &mut Frame,
    hs: &mut Hotspots,
    area: Rect,
    id: ElemId,
    label: &str,
    fg: Color,
    hover: bool,
    enabled: bool,
) {
    if enabled {
        hs.push((area, id));
    }
    let (border_col, text_col, bg) = if !enabled {
        (theme::BORDER, Color::Rgb(80, 80, 88), theme::PANEL)
    } else if hover {
        (theme::ACCENT, fg, theme::HOVER_BG)
    } else {
        (theme::BORDER, fg, theme::PANEL)
    };
    let blk = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_col))
        .style(Style::default().bg(bg));
    let inner = blk.inner(area);
    f.render_widget(blk, area);
    let p = Paragraph::new(Line::from(Span::styled(
        label,
        Style::default().fg(text_col).add_modifier(Modifier::BOLD),
    )))
    .alignment(Alignment::Center);
    // center vertically
    let y = inner.y + inner.height / 2;
    let line_area = Rect::new(inner.x, y, inner.width, 1);
    f.render_widget(p, line_area);
}

/// Editable text field (focus + cursor).
#[allow(clippy::too_many_arguments)]
pub fn text_field(
    f: &mut Frame,
    hs: &mut Hotspots,
    area: Rect,
    id: ElemId,
    value: &str,
    focused: bool,
    cursor: usize,
    hover: bool,
    enabled: bool,
) {
    if enabled {
        hs.push((area, id));
    }
    let (bg, fg) = if !enabled {
        (theme::PANEL, Color::Rgb(90, 90, 98))
    } else if focused {
        (theme::FIELD_FOCUS_BG, theme::STRONG)
    } else if hover {
        (theme::HOVER_BG, theme::STRONG)
    } else {
        (theme::FIELD_BG, theme::VAL)
    };

    let inner_w = area.width.saturating_sub(2) as usize;
    let chars: Vec<char> = value.chars().collect();
    let cur = cursor.min(chars.len());
    let start = if inner_w > 0 && cur > inner_w {
        cur - inner_w
    } else {
        0
    };
    let end = (start + inner_w).min(chars.len());
    let vis: String = chars[start..end].iter().collect();
    let line = format!(" {vis}");

    let p = Paragraph::new(Line::from(line)).style(Style::default().bg(bg).fg(fg));
    f.render_widget(p, area);

    if focused {
        let cx = area.x + 1 + (cur - start) as u16;
        let cx = cx.min(area.x + area.width.saturating_sub(1));
        f.set_cursor_position((cx, area.y));
    }
}

/// GroupBox: border + title (Windows box style). Returns the inner area.
pub fn group_box(f: &mut Frame, area: Rect, title: &str) -> Rect {
    let blk = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::BORDER))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(theme::STRONG)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme::PANEL));
    let inner = blk.inner(area);
    f.render_widget(blk, area);
    inner
}

/// "Big" 3-row digits (ASCII 7-segment) for the QUEUE counter.
/// Up to 3 digits fit in 14 columns (3 per digit + 1 of spacing).
pub fn big_digits(s: &str) -> [String; 3] {
    const SEG: [[&str; 3]; 10] = [
        [" _ ", "| |", "|_|"], // 0
        ["   ", "  |", "  |"], // 1
        [" _ ", " _|", "|_ "], // 2
        [" _ ", " _|", " _|"], // 3
        ["   ", "|_|", "  |"], // 4
        [" _ ", "|_ ", " _|"], // 5
        [" _ ", "|_ ", "|_|"], // 6
        [" _ ", "  |", "  |"], // 7
        [" _ ", "|_|", "|_|"], // 8
        [" _ ", "|_|", " _|"], // 9
    ];
    let mut rows = [String::new(), String::new(), String::new()];
    for ch in s.chars() {
        let d = ch.to_digit(10).unwrap_or(0) as usize;
        for (r, row) in rows.iter_mut().enumerate() {
            row.push_str(SEG[d][r]);
        }
    }
    rows
}

/// "key: value" row with the key right-aligned (fixed width).
pub fn kv_line<'a>(key: &str, val: &'a str, label_w: usize, val_color: Color) -> Line<'a> {
    let v = if val.is_empty() { "—" } else { val };
    Line::from(vec![
        Span::styled(
            format!("{key:>label_w$}: "),
            Style::default().fg(theme::LABEL),
        ),
        Span::styled(v.to_string(), Style::default().fg(val_color)),
    ])
}

/// Graphic crop button: a mini 16:9 frame with the kept 21:9 band in light.
pub fn crop_glyph(
    f: &mut Frame,
    hs: &mut Hotspots,
    area: Rect,
    mode: CropMode,
    selected: bool,
    hover: bool,
    enabled: bool,
) {
    if enabled {
        hs.push((area, ElemId::Crop(mode as u8)));
    }
    let bg = if selected {
        theme::SEL_BG
    } else if hover {
        theme::HOVER_BG
    } else {
        theme::PANEL
    };
    let removed = if enabled {
        theme::CROP_REMOVED
    } else {
        Color::Rgb(40, 40, 46)
    };
    let kept = if !enabled {
        Color::Rgb(80, 90, 110)
    } else if selected {
        theme::CROP_KEPT_SEL
    } else {
        theme::CROP_KEPT
    };

    // 3-row pattern (approx): which bands are kept vs cropped
    let rows: [Color; 3] = match mode {
        CropMode::None => [kept, kept, kept],
        CropMode::Centered => [removed, kept, removed],
        CropMode::OneThird => [removed, kept, kept],
    };
    let bar_w = area.width.saturating_sub(2) as usize; // 1 margin each side (shows bg)
    for (i, col) in rows.iter().enumerate() {
        if i as u16 >= area.height {
            break;
        }
        let bars: String = "█".repeat(bar_w);
        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled(bars, Style::default().fg(*col)),
            Span::raw(" "),
        ]);
        let ra = Rect::new(area.x, area.y + i as u16, area.width, 1);
        f.render_widget(
            Paragraph::new(line).style(Style::default().bg(bg)),
            ra,
        );
    }
}
