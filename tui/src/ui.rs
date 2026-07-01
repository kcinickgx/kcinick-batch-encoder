//! Renders the whole "window" + registers hotspots (mouse hit-testing).
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Gauge, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{
    App, ElemId, ExKind, Focus, Mode, SlotState, BITRATES, CODECS, PRESETS, SLOTS,
};
use crate::encoder::{self, CropMode};
use crate::theme;
use crate::widgets::{self, Hotspots};

fn cw(label: &str) -> u16 {
    label.chars().count() as u16 + 4
}

fn label(f: &mut Frame, area: Rect, s: &str, fg: Color) {
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(s.to_string(), Style::default().fg(fg)))),
        area,
    );
}

pub fn draw(f: &mut Frame, app: &App, hs: &mut Hotspots) {
    hs.clear();
    let area = f.area();

    // outer "window" frame
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(theme::ACCENT))
        .title(Span::styled(
            " KciNicK Batch Encoder ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ))
        .title_alignment(Alignment::Center)
        .style(Style::default().bg(theme::PANEL));
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    // close button [x] top-right (on the border)
    if area.width >= 6 {
        let close = Rect::new(area.x + area.width - 5, area.y, 3, 1);
        let hover = app.hover == Some(ElemId::TitleClose);
        hs.push((close, ElemId::TitleClose));
        f.render_widget(
            Paragraph::new(Line::from("[x]")).style(
                Style::default()
                    .bg(if hover { theme::FAIL } else { theme::TITLEBAR })
                    .fg(Color::White),
            ),
            close,
        );
    }

    if inner.width < 60 || inner.height < 20 {
        f.render_widget(
            Paragraph::new("Enlarge the terminal (min. 62x22).")
                .style(Style::default().fg(theme::FAIL)),
            inner,
        );
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(12), // config
        Constraint::Length(3),  // tabs
        Constraint::Min(1),     // slot
    ])
    .split(inner);

    draw_config(f, app, hs, rows[0]);
    draw_tabs(f, app, hs, rows[1]);
    draw_slot(f, app, hs, rows[2]);

    if app.explorer.is_some() {
        draw_explorer(f, app, hs, inner);
    }
    if app.confirm_exit {
        draw_confirm(f, app, hs, inner);
    }
    if app.closing {
        draw_closing(f, inner, app.running_count());
    }
}

fn draw_config(f: &mut Frame, app: &App, hs: &mut Hotspots, area: Rect) {
    let is_running = app.is_running();
    let connected = app.connected();
    let hv = |id: ElemId| app.hover == Some(id);

    let r = Layout::vertical([
        Constraint::Length(5), // Play/Stop + MODE box + QUEUE
        Constraint::Length(1), // folder
        Constraint::Length(5), // group row (CROP·CODEC·ENCODERS·PRESET·BITRATE)
        Constraint::Min(0),    // status
    ])
    .split(area);

    // ---- MODE box (adapts on resize) + QUEUE (fixed width) on the right ----
    {
        let top = Layout::horizontal([
            Constraint::Length(10), // play
            Constraint::Length(10), // stop
            Constraint::Length(1),  // gap
            Constraint::Min(40),    // MODE — takes the leftover width
            Constraint::Length(1),  // gap
            Constraint::Length(15), // QUEUE — fixed width (up to 4 digits)
        ])
        .split(r[0]);

        // Play / Stop (left of MODE)
        let pa = Rect::new(top[0].x, r[0].y, 9, 5);
        widgets::box_button(f, hs, pa, ElemId::Play, "▶", theme::OK, hv(ElemId::Play), !is_running);
        let sa = Rect::new(top[1].x, r[0].y, 9, 5);
        widgets::box_button(f, hs, sa, ElemId::Stop, "■", theme::FAIL, hv(ElemId::Stop), is_running);

        // QUEUE (right, fixed width) — number in big digits (3 lines)
        let qi = widgets::group_box(f, top[5], "QUEUE");
        let big = widgets::big_digits(&app.queue_len().to_string());
        let lines: Vec<Line> = big.iter().map(|s| Line::from(s.clone())).collect();
        // right-aligned with 1 char of padding (excludes the last column)
        let na = Rect::new(qi.x, qi.y, qi.width.saturating_sub(1), qi.height);
        f.render_widget(
            Paragraph::new(lines)
                .alignment(Alignment::Right)
                .style(Style::default().fg(theme::AMBER).add_modifier(Modifier::BOLD)),
            na,
        );

        let mi = widgets::group_box(f, top[3], "MODE");
        let right = mi.x + mi.width; // inner right edge (exclusive)

        // line 0: Local / Remote tabs + connection indicator
        let ty = mi.y;
        let mut x = mi.x + 1;
        let w = cw("Local");
        widgets::chip(f, hs, Rect::new(x, ty, w, 1), ElemId::ModeLocal, "Local",
            app.mode == Mode::Local, hv(ElemId::ModeLocal), !is_running);
        x += w + 1;
        let w = cw("Remote");
        widgets::chip(f, hs, Rect::new(x, ty, w, 1), ElemId::ModeRemote, "Remote",
            app.mode == Mode::Daemon, hv(ElemId::ModeRemote), !is_running);
        x += w + 2;
        if app.mode == Mode::Daemon {
            if connected {
                label(f, Rect::new(x, ty, 14, 1), "● connected", theme::OK);
            } else {
                label(f, Rect::new(x, ty, 16, 1), "○ not connected", theme::DIM);
            }
        }

        // line 1: mode content
        let cy = mi.y + 1;
        let x0 = mi.x + 1;
        if app.mode == Mode::Local {
            label(f, Rect::new(x0, cy, 8, 1), "ffmpeg:", theme::LABEL);
            let fx = x0 + 8;
            let browse_w = 5u16;
            let fw = right.saturating_sub(fx + browse_w + 2);
            widgets::text_field(f, hs, Rect::new(fx, cy, fw, 1), ElemId::FieldFfmpeg,
                &app.ffmpeg, app.focus == Focus::Ffmpeg, app.edit_cursor,
                hv(ElemId::FieldFfmpeg), !is_running);
            widgets::flat_button(f, hs, Rect::new(fx + fw + 1, cy, browse_w, 1),
                ElemId::BrowseFfmpeg, "[…]", hv(ElemId::BrowseFfmpeg), !is_running);
        } else {
            label(f, Rect::new(x0, cy, 8, 1), "server:", theme::LABEL);
            let sx = x0 + 8;
            let btn_w = 14u16;
            let tok_lbl_w = 8u16;
            let avail = right.saturating_sub(sx + tok_lbl_w + btn_w + 4);
            let half = (avail / 2).max(8);
            widgets::text_field(f, hs, Rect::new(sx, cy, half, 1), ElemId::FieldServer,
                &app.daemon_addr, app.focus == Focus::Server, app.edit_cursor,
                hv(ElemId::FieldServer), !connected);
            let tx = sx + half + 1;
            label(f, Rect::new(tx, cy, tok_lbl_w, 1), "token:", theme::LABEL);
            let tfx = tx + tok_lbl_w;
            widgets::text_field(f, hs, Rect::new(tfx, cy, half, 1), ElemId::FieldToken,
                &app.token, app.focus == Focus::Token, app.edit_cursor,
                hv(ElemId::FieldToken), !connected);
            let bx = tfx + half + 1;
            let lbl = if connected { "Disconnect" } else { "Connect" };
            widgets::flat_button(f, hs, Rect::new(bx, cy, btn_w, 1), ElemId::ConnectBtn,
                lbl, hv(ElemId::ConnectBtn), true);
        }
    }

    // ---- Source folder row ----
    {
        let y = r[1].y;
        let x0 = area.x + 1;
        label(f, Rect::new(x0, y, 9, 1), "Folder:", theme::LABEL);
        let fx = x0 + 9;
        let browse_w = 5u16;
        let fw = area.right().saturating_sub(fx + browse_w + 2);
        widgets::text_field(f, hs, Rect::new(fx, y, fw, 1), ElemId::FieldSrc,
            &app.src_dir, app.focus == Focus::Src, app.edit_cursor,
            hv(ElemId::FieldSrc), !is_running);
        widgets::flat_button(f, hs, Rect::new(fx + fw + 1, y, browse_w, 1),
            ElemId::BrowseSrc, "[…]", hv(ElemId::BrowseSrc), !is_running);
    }

    // ---- group row: CROP · CODEC · ENCODERS · PRESET · BITRATE (all in one row) ----
    {
        let row = r[2];
        let cols = Layout::horizontal([
            Constraint::Min(24),    // crop — adapts on resize (fills the leftover)
            Constraint::Length(27), // codec
            Constraint::Length(25), // encoders
            Constraint::Length(44), // preset
            Constraint::Length(39), // bitrate
        ])
        .split(row);

        // CROP
        let inner = widgets::group_box(f, cols[0], "CROP");
        let cw3 = inner.width / 3;
        let modes = [CropMode::Centered, CropMode::OneThird, CropMode::None];
        for (i, m) in modes.iter().enumerate() {
            let cx = inner.x + i as u16 * cw3;
            let a = Rect::new(cx, inner.y, cw3, inner.height.min(3));
            widgets::crop_glyph(f, hs, a, *m, app.crop == *m,
                hv(ElemId::Crop(*m as u8)), !is_running);
        }

        // CODEC
        let inner = widgets::group_box(f, cols[1], "CODEC");
        let y = inner.y + inner.height / 2;
        let mut x = inner.x;
        for (i, c) in CODECS.iter().enumerate() {
            let w = cw(c.label());
            widgets::chip(f, hs, Rect::new(x, y, w, 1), ElemId::Codec(i), c.label(),
                app.codec_idx == i, hv(ElemId::Codec(i)), !is_running);
            x += w + 1;
        }

        // ENCODERS (always enabled)
        let inner = widgets::group_box(f, cols[2], "ENCODERS");
        let y = inner.y + inner.height / 2;
        let mut x = inner.x;
        for n in 1..=SLOTS {
            let s = n.to_string();
            let w = cw(&s);
            widgets::chip(f, hs, Rect::new(x, y, w, 1), ElemId::Encoders(n), &s,
                app.parallel == n, hv(ElemId::Encoders(n)), true);
            x += w + 1;
        }

        // PRESET
        let inner = widgets::group_box(f, cols[3], "PRESET");
        let y = inner.y + inner.height / 2;
        let mut x = inner.x;
        for (i, p) in PRESETS.iter().enumerate() {
            let w = cw(p);
            widgets::chip(f, hs, Rect::new(x, y, w, 1), ElemId::Preset(i), p,
                app.preset_idx == i, hv(ElemId::Preset(i)), !is_running);
            x += w;
        }

        // BITRATE
        let inner = widgets::group_box(f, cols[4], "BITRATE");
        let y = inner.y + inner.height / 2;
        let mut x = inner.x;
        for (i, (_, _, l)) in BITRATES.iter().enumerate() {
            let w = cw(l);
            widgets::chip(f, hs, Rect::new(x, y, w, 1), ElemId::Bitrate(i), l,
                app.bitrate_idx == i, hv(ElemId::Bitrate(i)), !is_running);
            x += w;
        }
    }

    // ---- status (errors, both modes) ----
    if !app.status_msg.is_empty() {
        label(f, Rect::new(area.x + 1, r[3].y, area.width.saturating_sub(2), 1),
            &app.status_msg, theme::FAIL);
    }
}

fn draw_tabs(f: &mut Frame, app: &App, hs: &mut Hotspots, area: Rect) {
    // 4 tabs at 25% each, filling the whole terminal width
    let cols = Layout::horizontal([
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
    ])
    .split(area);
    for i in 0..SLOTS {
        let a = cols[i];
        hs.push((a, ElemId::Tab(i)));
        let selected = app.selected_tab == i;
        let hover = app.hover == Some(ElemId::Tab(i));
        let st = app.slots[i].state.lock().unwrap().clone();
        let (border_col, bg) = if selected {
            (theme::ACCENT, theme::SEL_BG)
        } else if hover {
            (theme::BORDER, theme::HOVER_BG)
        } else {
            (theme::BORDER, theme::TAB_INACTIVE)
        };
        let blk = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_col))
            .style(Style::default().bg(bg));
        let bi = blk.inner(a);
        f.render_widget(blk, a);
        // 1 char of padding each side (name on the left, state on the right)
        let ti = Rect::new(bi.x + 1, bi.y, bi.width.saturating_sub(2), bi.height);
        let name_col = if selected { theme::STRONG } else { theme::DIM };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("encode{}", i + 1),
                Style::default().fg(name_col).add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ))),
            ti,
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(st.label(), Style::default().fg(st.color()))))
                .alignment(Alignment::Right),
            ti,
        );
    }
}

fn draw_slot(f: &mut Frame, app: &App, hs: &mut Hotspots, area: Rect) {
    let idx = app.selected_tab.min(SLOTS - 1);
    let slot = &app.slots[idx];
    let st = slot.state.lock().unwrap().clone();
    let name = slot.name.lock().unwrap().clone();
    let info = slot.info.lock().unwrap().clone();
    let cmd = slot.cmdline.lock().unwrap().clone();
    let streams = slot.streams.lock().unwrap().clone();
    let target = slot.target.lock().unwrap().clone();
    let progress = slot.progress.lock().unwrap().clone();

    let running = st == SlotState::Running;

    let r = Layout::vertical([
        Constraint::Length(1), // file
        Constraint::Length(1), // output
        Constraint::Length(4), // command
        Constraint::Length(8), // streams
        Constraint::Length(8), // progress
        Constraint::Min(1),    // keys / error
    ])
    .split(area);

    // ---- File + state ----
    {
        let mut spans = vec![Span::styled("File: ", Style::default().fg(theme::STRONG).add_modifier(Modifier::BOLD))];
        if name.is_empty() {
            spans.push(Span::styled("idle", Style::default().fg(SlotState::Idle.color())));
        } else {
            spans.push(Span::styled(name.clone(), Style::default().fg(theme::VAL)));
            if st != SlotState::Idle {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(st.label(), Style::default().fg(st.color())));
            }
        }
        f.render_widget(Paragraph::new(Line::from(spans)), Rect::new(area.x + 1, r[0].y, area.width - 2, 1));
    }
    // ---- Output ----
    if !name.is_empty() && !info.is_empty() {
        label(f, Rect::new(area.x + 1, r[1].y, area.width - 2, 1), &format!("Output: {info}"), theme::DIM);
    }

    // ---- Command ----
    {
        let inner = widgets::group_box(f, r[2], "Command");
        if !cmd.is_empty() {
            f.render_widget(
                Paragraph::new(cmd)
                    .style(Style::default().fg(Color::Rgb(170, 170, 175)))
                    .wrap(Wrap { trim: false })
                    .scroll((app.cmd_scroll, 0)),
                inner,
            );
        }
    }

    // ---- Streams: 4 boxes ----
    {
        let cols = Layout::horizontal([
            Constraint::Ratio(1, 4),
            Constraint::Ratio(1, 4),
            Constraint::Ratio(1, 4),
            Constraint::Ratio(1, 4),
        ])
        .split(r[3]);
        let lw = 7;
        let vs = draw_stream_box(f, cols[0], "VIDEO · SOURCE", &[
            ("Codec", &streams.v_codec, theme::VIDEO),
            ("Res", &streams.v_res, theme::VIDEO),
            ("FPS", &streams.v_fps, theme::VIDEO),
            ("Bits", &streams.v_bits, theme::VIDEO),
            ("Bitrate", &streams.v_bitrate, theme::VIDEO),
            ("Dur", &streams.dur_str, theme::AMBER),
        ], lw);
        let _ = vs;
        draw_stream_box(f, cols[1], "VIDEO · TARGET", &[
            ("Codec", &target.v_codec, theme::VIDEO),
            ("Res", &target.v_res, theme::VIDEO),
            ("FPS", &target.v_fps, theme::VIDEO),
            ("Bits", &target.v_bits, theme::VIDEO),
            ("Bitrate", &target.v_bitrate, theme::AMBER),
            ("Dur", &target.dur_str, theme::AMBER),
        ], lw);
        draw_stream_box(f, cols[2], "AUDIO · SOURCE", &[
            ("Codec", &streams.a_codec, theme::AUDIO),
            ("Channels", &streams.a_layout, theme::AUDIO),
            ("Sample", &streams.a_rate, theme::AUDIO),
            ("Bitrate", &streams.a_bitrate, theme::AUDIO),
        ], lw);
        draw_stream_box(f, cols[3], "AUDIO · TARGET", &[
            ("Codec", &target.a_codec, theme::AUDIO),
            ("Channels", &target.a_layout, theme::AUDIO),
            ("Sample", &target.a_rate, theme::AUDIO),
            ("Bitrate", &target.a_bitrate, theme::AMBER),
        ], lw);
    }

    // ---- PROGRESS ----
    {
        let inner = widgets::group_box(f, r[4], "PROGRESS");
        let p = progress.clone().unwrap_or_default();
        let eta = if p.speed_val > 0.0 && streams.duration > 0.0 {
            let rem = (streams.duration - p.time_sec).max(0.0);
            encoder::fmt_hms(rem / p.speed_val)
        } else {
            "—".to_string()
        };
        let elapsed = if p.time.is_empty() {
            format!("— / {}", streams.dur_str)
        } else {
            format!("{} / {}", p.time, streams.dur_str)
        };
        // 2 columns of kv (to save height)
        let pc = Layout::horizontal([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
            .split(Rect::new(inner.x, inner.y, inner.width, inner.height.saturating_sub(2)));
        let lw = 9;
        let left = vec![
            widgets::kv_line("Frame", &p.frame, lw, theme::VAL),
            widgets::kv_line("FPS", &p.fps, lw, theme::VAL),
            widgets::kv_line("Q", &p.q, lw, theme::VAL),
            widgets::kv_line("Size", &p.size, lw, theme::VAL),
        ];
        let right = vec![
            widgets::kv_line("Time", &elapsed, lw, theme::VAL),
            widgets::kv_line("Bitrate", &p.bitrate, lw, theme::VAL),
            widgets::kv_line("Speed", &p.speed, lw, theme::AMBER),
            widgets::kv_line("ETA", &eta, lw, theme::ETA),
        ];
        f.render_widget(Paragraph::new(left), pc[0]);
        f.render_widget(Paragraph::new(right), pc[1]);

        let frac = if streams.duration > 0.0 {
            (p.time_sec / streams.duration).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let gy = inner.y + inner.height.saturating_sub(1);
        let ga = Rect::new(inner.x, gy, inner.width, 1);
        f.render_widget(
            Gauge::default()
                .gauge_style(Style::default().fg(theme::ACCENT).bg(theme::FIELD_BG))
                .ratio(frac)
                .label(format!("{:.0}%", frac * 100.0)),
            ga,
        );
    }

    // ---- keys zone / error ----
    if running {
        draw_keys(f, app, hs, r[5]);
    } else if let SlotState::Fail(_) = st {
        let out = slot.output.lock().unwrap().clone();
        if !out.is_empty() {
            let tail: String = out.chars().rev().take(1500).collect::<Vec<_>>().into_iter().rev().collect();
            let blk = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme::FAIL))
                .title(Span::styled(" Error (ffmpeg) ", Style::default().fg(theme::FAIL)))
                .style(Style::default().bg(Color::Rgb(0, 0, 30)));
            let bi = blk.inner(r[5]);
            f.render_widget(blk, r[5]);
            f.render_widget(
                Paragraph::new(tail)
                    .style(Style::default().fg(Color::Rgb(235, 180, 180)))
                    .wrap(Wrap { trim: false }),
                bi,
            );
        }
    }
}

fn draw_stream_box(f: &mut Frame, area: Rect, title: &str, rows: &[(&str, &str, Color)], lw: usize) {
    let inner = widgets::group_box(f, area, title);
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, v, c)| widgets::kv_line(k, v, lw, *c))
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_keys(f: &mut Frame, app: &App, hs: &mut Hotspots, area: Rect) {
    let y = area.y;
    let focused = app.focus == Focus::KeyZone;
    let zone_w = area.width.saturating_sub(20).min(46);
    let zone = Rect::new(area.x + 1, y, zone_w, 1);
    hs.push((zone, ElemId::KeyZone));
    let (bg, fg) = if focused {
        (Color::Rgb(18, 55, 30), Color::Rgb(175, 240, 185))
    } else if app.hover == Some(ElemId::KeyZone) {
        (theme::HOVER_BG, theme::STRONG)
    } else {
        (theme::FIELD_BG, theme::DIM)
    };
    let msg = if focused {
        " ● keyboard active — press  q  +  -"
    } else {
        " click here and type  (q + -)"
    };
    f.render_widget(
        Paragraph::new(Line::from(msg)).style(Style::default().bg(bg).fg(fg)),
        zone,
    );
    // q / + / - buttons
    let mut x = zone.x + zone.width + 1;
    for (k, id) in [
        ("q", ElemId::KeyBtn(b'q')),
        ("+", ElemId::KeyBtn(b'+')),
        ("-", ElemId::KeyBtn(b'-')),
    ] {
        let a = Rect::new(x, y, 3, 1);
        widgets::flat_button(f, hs, a, id, k, app.hover == Some(id), true);
        x += 4;
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

fn draw_explorer(f: &mut Frame, app: &App, hs: &mut Hotspots, area: Rect) {
    let Some(ex) = &app.explorer else { return };
    let m = centered(area, (area.width * 3 / 4).max(50), (area.height * 3 / 4).max(14));
    f.render_widget(Clear, m);
    let title = match ex.kind {
        ExKind::File => " Pick ffmpeg ",
        ExKind::Dir => " Pick source folder ",
    };
    let blk = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(theme::ACCENT))
        .title(Span::styled(title, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)))
        .style(Style::default().bg(theme::TITLEBAR));
    let bi = blk.inner(m);
    f.render_widget(blk, m);

    let parts = Layout::vertical([
        Constraint::Length(1), // path
        Constraint::Min(3),    // list
        Constraint::Length(1), // buttons
    ])
    .split(bi);

    label(f, parts[0], &format!(" {}", ex.cwd.display()), theme::AMBER);

    // list: ".." + entries (with scroll)
    let list = parts[1];
    let up = Rect::new(list.x, list.y, list.width, 1);
    hs.push((up, ElemId::ExUp));
    let up_hover = app.hover == Some(ElemId::ExUp);
    f.render_widget(
        Paragraph::new(Line::from("  📁 ..")).style(
            Style::default()
                .bg(if up_hover { theme::HOVER_BG } else { theme::TITLEBAR })
                .fg(theme::DIM),
        ),
        up,
    );
    let visible = list.height.saturating_sub(1) as usize;
    let start = ex.scroll.min(ex.entries.len());
    for (row, i) in (start..ex.entries.len()).take(visible).enumerate() {
        let (nm, is_dir) = &ex.entries[i];
        let is_dir = *is_dir;
        let a = Rect::new(list.x, list.y + 1 + row as u16, list.width, 1);
        hs.push((a, ElemId::ExRow(i)));
        // in Dir mode, folders and videos have a checkbox (checked = included)
        let dir_mode = matches!(ex.kind, ExKind::Dir);
        let canon = ex.cwd.join(nm);
        let checked = if !dir_mode {
            true
        } else if is_dir {
            !app.dir_excluded(&canon)
        } else {
            !app.file_excluded(&canon)
        };
        // the checkbox is its own hotspot: on folders it toggles without navigating
        if dir_mode {
            hs.push((Rect::new(a.x + 2, a.y, 4, 1), ElemId::ExCheck(i)));
        }
        let selected = ex.selected == i;
        let hover = app.hover == Some(ElemId::ExRow(i)) || app.hover == Some(ElemId::ExCheck(i));
        let base_fg = if !checked {
            theme::DIM
        } else if is_dir {
            theme::VIDEO
        } else {
            theme::VAL
        };
        let (bg, fg) = if selected {
            (theme::SEL_BG, Color::White)
        } else if hover {
            (theme::HOVER_BG, theme::STRONG)
        } else {
            (theme::TITLEBAR, base_fg)
        };
        let mark = if checked { "x" } else { " " };
        let text = if dir_mode {
            if is_dir {
                format!("  [{mark}] 📁 {nm}")
            } else {
                format!("  [{mark}] {nm}")
            }
        } else if is_dir {
            format!("  📁 {nm}")
        } else {
            format!("     {nm}")
        };
        f.render_widget(
            Paragraph::new(Line::from(text)).style(Style::default().bg(bg).fg(fg)),
            a,
        );
    }

    // buttons
    let by = parts[2].y;
    let open_lbl = match ex.kind {
        ExKind::File => "[ Choose file ]",
        ExKind::Dir => "[ Use this folder ]",
    };
    let ow = open_lbl.chars().count() as u16;
    widgets::flat_button(f, hs, Rect::new(bi.x + 1, by, ow, 1), ElemId::ExOpen, open_lbl,
        app.hover == Some(ElemId::ExOpen), true);
    let cw2 = 14u16;
    widgets::flat_button(f, hs, Rect::new(bi.right().saturating_sub(cw2 + 1), by, cw2, 1),
        ElemId::ExCancel, "[ Cancel ]", app.hover == Some(ElemId::ExCancel), true);
}

fn draw_confirm(f: &mut Frame, app: &App, hs: &mut Hotspots, area: Rect) {
    let m = centered(area, 56, 8);
    f.render_widget(Clear, m);
    let blk = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(theme::AMBER))
        .title(Span::styled(" Quit? ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)))
        .style(Style::default().bg(theme::TITLEBAR));
    let bi = blk.inner(m);
    f.render_widget(blk, m);
    f.render_widget(
        Paragraph::new(format!(
            "There are {} ffmpeg process(es) running. If you quit they'll be closed cleanly (q) — it may take a few seconds.",
            app.running_count()
        ))
        .wrap(Wrap { trim: true })
        .style(Style::default().fg(theme::VAL)),
        Rect::new(bi.x, bi.y, bi.width, bi.height.saturating_sub(2)),
    );
    let by = bi.y + bi.height - 1;
    widgets::flat_button(f, hs, Rect::new(bi.x + 1, by, 14, 1), ElemId::ConfirmYes, "[ Yes, quit ]",
        app.hover == Some(ElemId::ConfirmYes), true);
    widgets::flat_button(f, hs, Rect::new(bi.x + 18, by, 14, 1), ElemId::ConfirmNo, "[ No, stay ]",
        app.hover == Some(ElemId::ConfirmNo), true);
}

fn draw_closing(f: &mut Frame, area: Rect, n: usize) {
    let m = centered(area, 50, 5);
    f.render_widget(Clear, m);
    let blk = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(theme::ACCENT))
        .title(" Closing… ")
        .style(Style::default().bg(theme::TITLEBAR));
    let bi = blk.inner(m);
    f.render_widget(blk, m);
    f.render_widget(
        Paragraph::new(format!("Waiting for {n} process(es) to close (q, clean shutdown)…"))
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(theme::VAL)),
        bi,
    );
}
