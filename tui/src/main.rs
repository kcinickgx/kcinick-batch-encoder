// fast6t — fast6's TUI client (Linux): same layout as the egui GUI, full screen,
// with mouse (hover + click), editable fields and a file explorer.
// Reuses proto.rs and encoder.rs; ports the daemon-client and scheduler logic.

mod app;
mod client;
mod config;
mod controller;
mod theme;
mod ui;
mod widgets;

#[allow(dead_code)] // proto is shared with the daemon; the client doesn't use all of it
#[path = "../../src/proto.rs"]
mod proto;
#[allow(dead_code)] // encoder is shared; the TUI doesn't use all of its functions
#[path = "../../src/encoder.rs"]
mod encoder;

use std::io::{self, Stdout};
use std::sync::atomic::Ordering;
use std::time::Duration;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::{App, ElemId, ExKind, Explorer, Focus, Mode};
use encoder::CropMode;
use widgets::hit;

type Term = Terminal<CrosstermBackend<Stdout>>;

fn main() -> io::Result<()> {
    install_panic_hook();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run(&mut terminal);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    res
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        original(info);
    }));
}

fn run(terminal: &mut Term) -> io::Result<()> {
    let mut app = App::default();

    loop {
        if app.needs_scan && !app.is_running() {
            app.scan();
            app.needs_scan = false;
        }
        // daemon: if no jobs remain, free the Play button (same as the GUI)
        if app.mode == Mode::Daemon
            && app.is_running()
            && app.daemon_files.lock().unwrap().is_empty()
        {
            app.running.store(false, Ordering::SeqCst);
        }

        let mut hs = Vec::new();
        terminal.draw(|f| ui::draw(f, &app, &mut hs))?;
        app.hotspots = hs;

        if event::poll(Duration::from_millis(120))? {
            match event::read()? {
                Event::Key(k) => handle_key(&mut app, k),
                Event::Mouse(m) => handle_mouse(&mut app, m),
                _ => {}
            }
        }

        if app.closing && app.running_count() == 0 {
            persist_all(&app);
            app.should_quit = true;
        }
        if app.should_quit {
            break;
        }
    }
    Ok(())
}

// ===================== persistence on close =====================
fn persist_all(app: &App) {
    config::save("ffmpeg", &app.ffmpeg);
    config::save("codec", app::CODECS[app.codec_idx].label());
    config::save("mode", if app.mode == Mode::Daemon { "daemon" } else { "local" });
    config::save("daemon_addr", &app.daemon_addr);
    config::save("token", &app.token);
}

fn request_quit(app: &mut App) {
    if app.mode == Mode::Local && app.running_count() > 0 {
        app.confirm_exit = true;
    } else {
        persist_all(app);
        app.should_quit = true;
    }
}

// ===================== keyboard =====================
fn handle_key(app: &mut App, k: KeyEvent) {
    if k.kind == KeyEventKind::Release {
        return;
    }
    // global quit
    if k.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(k.code, KeyCode::Char('q') | KeyCode::Char('c'))
    {
        request_quit(app);
        return;
    }

    if app.explorer.is_some() {
        explorer_key(app, k);
        return;
    }
    if app.confirm_exit {
        match k.code {
            KeyCode::Char('s') | KeyCode::Char('y') | KeyCode::Enter => {
                app.send_quit_all();
                app.confirm_exit = false;
                app.closing = true;
            }
            KeyCode::Char('n') | KeyCode::Esc => app.confirm_exit = false,
            _ => {}
        }
        return;
    }
    if app.closing {
        return;
    }

    // slot key zone: forward q/+/- to ffmpeg/daemon
    if app.focus == Focus::KeyZone {
        match k.code {
            KeyCode::Char(c @ ('q' | '+' | '-')) => {
                app.daemon_key(app.selected_tab, &c.to_string());
                return;
            }
            KeyCode::Esc => {
                app.focus = Focus::None;
                return;
            }
            _ => {}
        }
    }

    // text field editing
    if matches!(app.focus, Focus::Ffmpeg | Focus::Server | Focus::Token | Focus::Src) {
        match k.code {
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => field_insert(app, c),
            KeyCode::Backspace => field_backspace(app),
            KeyCode::Delete => field_delete(app),
            KeyCode::Left => app.edit_cursor = app.edit_cursor.saturating_sub(1),
            KeyCode::Right => {
                let len = focused_len(app);
                app.edit_cursor = (app.edit_cursor + 1).min(len);
            }
            KeyCode::Home => app.edit_cursor = 0,
            KeyCode::End => app.edit_cursor = focused_len(app),
            KeyCode::Enter => app.blur(),
            KeyCode::Esc => app.blur(),
            KeyCode::Tab => cycle_focus(app),
            _ => {}
        }
        return;
    }

    // no field focus: global shortcuts
    match k.code {
        KeyCode::Esc => request_quit(app),
        KeyCode::Tab => cycle_focus(app),
        KeyCode::Char('1') => app.selected_tab = 0,
        KeyCode::Char('2') => app.selected_tab = 1,
        KeyCode::Char('3') => app.selected_tab = 2,
        KeyCode::Char('4') => app.selected_tab = 3,
        _ => {}
    }
}

fn focused_len(app: &App) -> usize {
    match app.focus {
        Focus::Ffmpeg => app.ffmpeg.chars().count(),
        Focus::Server => app.daemon_addr.chars().count(),
        Focus::Token => app.token.chars().count(),
        Focus::Src => app.src_dir.chars().count(),
        _ => 0,
    }
}

fn field_insert(app: &mut App, c: char) {
    let cur = app.edit_cursor;
    if let Some(s) = app.focused_field_mut() {
        let mut v: Vec<char> = s.chars().collect();
        let cur = cur.min(v.len());
        v.insert(cur, c);
        *s = v.into_iter().collect();
    }
    app.edit_cursor += 1;
}

fn field_backspace(app: &mut App) {
    let cur = app.edit_cursor;
    if cur == 0 {
        return;
    }
    if let Some(s) = app.focused_field_mut() {
        let mut v: Vec<char> = s.chars().collect();
        if cur <= v.len() {
            v.remove(cur - 1);
            *s = v.into_iter().collect();
        }
    }
    app.edit_cursor = cur - 1;
}

fn field_delete(app: &mut App) {
    let cur = app.edit_cursor;
    if let Some(s) = app.focused_field_mut() {
        let mut v: Vec<char> = s.chars().collect();
        if cur < v.len() {
            v.remove(cur);
            *s = v.into_iter().collect();
        }
    }
}

fn cycle_focus(app: &mut App) {
    let order: Vec<Focus> = if app.mode == Mode::Local {
        vec![Focus::Ffmpeg, Focus::Src]
    } else if app.connected() {
        vec![Focus::Src]
    } else {
        vec![Focus::Server, Focus::Token, Focus::Src]
    };
    let cur = order.iter().position(|f| *f == app.focus);
    let next = match cur {
        Some(i) => order[(i + 1) % order.len()],
        None => order[0],
    };
    app.focus_field(next);
}

fn explorer_key(app: &mut App, k: KeyEvent) {
    // Space: check/uncheck the selected entry (Dir mode: video or folder)
    if k.code == KeyCode::Char(' ') {
        let sel = app.explorer.as_ref().map(|ex| ex.selected);
        if let Some((is_dir, path, dir_mode)) = sel.and_then(|i| explorer_entry(app, i)) {
            if dir_mode {
                if is_dir {
                    toggle_excluded_dir(app, path);
                } else {
                    toggle_excluded_file(app, path);
                }
            }
        }
        return;
    }
    let Some(ex) = app.explorer.as_mut() else { return };
    match k.code {
        KeyCode::Up => {
            if ex.selected > 0 {
                ex.selected -= 1;
                if ex.selected < ex.scroll {
                    ex.scroll = ex.selected;
                }
            }
        }
        KeyCode::Down => {
            if ex.selected + 1 < ex.entries.len() {
                ex.selected += 1;
            }
        }
        KeyCode::Enter => {
            if let Some((_, is_dir)) = ex.entries.get(ex.selected).cloned() {
                if is_dir {
                    ex.enter(ex.selected);
                    return;
                }
            }
            explorer_confirm(app);
        }
        KeyCode::Backspace | KeyCode::Left => ex.go_up(),
        KeyCode::Esc => app.explorer = None,
        _ => {}
    }
}

fn explorer_confirm(app: &mut App) {
    let Some(ex) = app.explorer.as_ref() else { return };
    match ex.kind {
        ExKind::Dir => {
            // doesn't rescan here: the QUEUE count updates on Play
            app.src_dir = ex.cwd.to_string_lossy().into_owned();
        }
        ExKind::File => {
            if let Some((nm, is_dir)) = ex.entries.get(ex.selected) {
                if !is_dir {
                    let p = ex.cwd.join(nm);
                    app.ffmpeg = p.to_string_lossy().into_owned();
                    config::save("ffmpeg", &app.ffmpeg);
                }
            }
        }
    }
    app.explorer = None;
}

// ===================== mouse =====================
fn handle_mouse(app: &mut App, m: MouseEvent) {
    match m.kind {
        MouseEventKind::Moved | MouseEventKind::Drag(_) => {
            app.hover = hit(&app.hotspots, m.column, m.row);
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let id = hit(&app.hotspots, m.column, m.row);
            match id {
                Some(id) => dispatch(app, id),
                None => {
                    // click outside everything: close editing (commit) if a field was focused
                    if matches!(
                        app.focus,
                        Focus::Ffmpeg | Focus::Server | Focus::Token | Focus::Src
                    ) {
                        app.blur();
                    } else {
                        app.focus = Focus::None;
                    }
                }
            }
        }
        MouseEventKind::ScrollUp => {
            if let Some(ex) = app.explorer.as_mut() {
                ex.scroll = ex.scroll.saturating_sub(1);
            } else {
                app.cmd_scroll = app.cmd_scroll.saturating_sub(1);
            }
        }
        MouseEventKind::ScrollDown => {
            if let Some(ex) = app.explorer.as_mut() {
                if ex.scroll + 1 < ex.entries.len() {
                    ex.scroll += 1;
                }
            } else {
                app.cmd_scroll = app.cmd_scroll.saturating_add(1);
            }
        }
        _ => {}
    }
}

fn is_explorer_id(id: ElemId) -> bool {
    matches!(
        id,
        ElemId::ExRow(_) | ElemId::ExCheck(_) | ElemId::ExUp | ElemId::ExOpen | ElemId::ExCancel
    )
}

/// Returns (is_dir, joined path, Dir_mode) of explorer entry i.
fn explorer_entry(app: &App, i: usize) -> Option<(bool, std::path::PathBuf, bool)> {
    app.explorer.as_ref().and_then(|ex| {
        ex.entries
            .get(i)
            .map(|(nm, d)| (*d, ex.cwd.join(nm), matches!(ex.kind, ExKind::Dir)))
    })
}

/// Checks/unchecks a video and rescans (live QUEUE count).
fn toggle_excluded_file(app: &mut App, path: std::path::PathBuf) {
    let path = path.canonicalize().unwrap_or(path);
    if !app.excluded.remove(&path) {
        app.excluded.insert(path);
    }
    app.scan();
}

/// Checks/unchecks a folder (recursive) and rescans.
fn toggle_excluded_dir(app: &mut App, path: std::path::PathBuf) {
    let path = path.canonicalize().unwrap_or(path);
    if !app.excluded_dirs.remove(&path) {
        app.excluded_dirs.insert(path);
    }
    app.scan();
}

fn dispatch(app: &mut App, id: ElemId) {
    // modal guards: only accept their own elements
    if app.explorer.is_some() && !is_explorer_id(id) {
        return;
    }
    if app.confirm_exit && !matches!(id, ElemId::ConfirmYes | ElemId::ConfirmNo) {
        return;
    }

    match id {
        ElemId::TitleClose => request_quit(app),
        ElemId::ModeLocal => {
            app.mode = Mode::Local;
            config::save("mode", "local");
        }
        ElemId::ModeRemote => {
            app.mode = Mode::Daemon;
            config::save("mode", "daemon");
        }
        ElemId::FieldFfmpeg => app.focus_field(Focus::Ffmpeg),
        ElemId::FieldServer => app.focus_field(Focus::Server),
        ElemId::FieldToken => app.focus_field(Focus::Token),
        ElemId::FieldSrc => app.focus_field(Focus::Src),
        ElemId::BrowseFfmpeg => {
            app.blur();
            app.explorer = Some(Explorer::open(ExKind::File, &app.ffmpeg));
        }
        ElemId::BrowseSrc => {
            app.blur();
            app.explorer = Some(Explorer::open(ExKind::Dir, &app.src_dir));
        }
        ElemId::ConnectBtn => {
            app.blur();
            if app.connected() {
                app.disconnect_daemon();
            } else {
                config::save("daemon_addr", &app.daemon_addr);
                config::save("token", &app.token);
                app.connect_daemon();
            }
        }
        ElemId::Play => {
            app.blur();
            app.start();
        }
        ElemId::Stop => {
            if app.mode == Mode::Daemon {
                app.daemon_cancel_all();
            } else {
                app.cancel.store(true, Ordering::SeqCst);
            }
        }
        ElemId::Crop(m) => {
            app.crop = match m {
                1 => CropMode::Centered,
                2 => CropMode::OneThird,
                _ => CropMode::None,
            };
        }
        ElemId::Codec(i) => {
            app.codec_idx = i;
            config::save("codec", app::CODECS[i].label());
        }
        ElemId::Encoders(n) => app.set_parallel(n),
        ElemId::Preset(i) => app.preset_idx = i,
        ElemId::Bitrate(i) => app.bitrate_idx = i,
        ElemId::Tab(i) => app.selected_tab = i,
        ElemId::KeyZone => {
            app.blur();
            app.focus = Focus::KeyZone;
        }
        ElemId::KeyBtn(b) => app.daemon_key(app.selected_tab, &(b as char).to_string()),
        ElemId::ExRow(i) => {
            if let Some((is_dir, path, dir_mode)) = explorer_entry(app, i) {
                if is_dir {
                    if let Some(ex) = app.explorer.as_mut() {
                        ex.enter(i);
                    }
                } else if dir_mode {
                    toggle_excluded_file(app, path); // video: clicking the row toggles it
                    if let Some(ex) = app.explorer.as_mut() {
                        ex.selected = i;
                    }
                } else if let Some(ex) = app.explorer.as_mut() {
                    ex.selected = i;
                }
            }
        }
        ElemId::ExCheck(i) => {
            if let Some((is_dir, path, _)) = explorer_entry(app, i) {
                if is_dir {
                    toggle_excluded_dir(app, path); // folder: unchecks recursively
                } else {
                    toggle_excluded_file(app, path);
                }
                if let Some(ex) = app.explorer.as_mut() {
                    ex.selected = i;
                }
            }
        }
        ElemId::ExUp => {
            if let Some(ex) = app.explorer.as_mut() {
                ex.go_up();
            }
        }
        ElemId::ExOpen => explorer_confirm(app),
        ElemId::ExCancel => app.explorer = None,
        ElemId::ConfirmYes => {
            app.send_quit_all();
            app.confirm_exit = false;
            app.closing = true;
        }
        ElemId::ConfirmNo => app.confirm_exit = false,
    }
}
