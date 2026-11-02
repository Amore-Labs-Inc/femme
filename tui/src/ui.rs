//! Drawing. Wide: a 34-column sidebar on the left, the agent's screen on the right, a 3-row
//! composer and a 1-row status bar underneath. Narrow (a phone): the list, or the agent with its
//! composer, and Tab between them. Dialogs float over the middle.
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::state::{elapsed, short_dir, App, Mode, Row, KINDS};

const SIDEBAR: u16 = 38;

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    app.width = area.width;
    let [main, comp, status] = Layout::vertical([Constraint::Min(3), Constraint::Length(3), Constraint::Length(1)]).areas(area);
    if app.narrow() {
        if app.narrow_agent {
            screen(f, app, main);
        } else {
            sidebar(f, app, main);
        }
    } else if app.sidebar {
        let [side, right] = Layout::horizontal([Constraint::Length(SIDEBAR), Constraint::Min(10)]).areas(main);
        sidebar(f, app, side);
        screen(f, app, right);
    } else {
        screen(f, app, main);
    }
    composer(f, app, comp);
    status_bar(f, app, status);
    match app.mode {
        Mode::Help => help(f, app, area),
        Mode::New => new_dialog(f, app, area),
        Mode::Close => close_dialog(f, app, area),
        _ => {}
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max <= 1 {
        "…".into()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}

fn pad(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width { s.to_string() } else { format!("{s}{}", " ".repeat(width - n)) }
}

fn sidebar(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme;
    let agents = app.rows.iter().filter(|r| matches!(r, Row::Agent(_))).count();
    let headers = app.rows.len() - agents;
    let block = Block::new().style(t.sidebar()).title(Line::from(vec![
        Span::styled(" FEMME", t.accent()),
        Span::styled(format!("  {agents} agents"), Style::new().fg(t.muted)),
    ]));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
    let w = inner.width as usize;
    // A second line per agent (its folder) when the rows fit twice over.
    let two_lines = agents * 2 + headers + 1 <= inner.height as usize;
    let mut items = Vec::with_capacity(app.rows.len());
    for (i, row) in app.rows.iter().enumerate() {
        match row {
            Row::Header { machine, note } => {
                let head = if note.is_empty() { format!(" {machine} ") } else { format!(" {machine} · {note} ") };
                let fill = w.saturating_sub(head.chars().count() + 2);
                let style = if note.is_empty() { t.header() } else { t.header().fg(t.muted) };
                items.push(ListItem::new(Line::styled(format!("──{head}{}", "─".repeat(fill)), style)));
            }
            Row::Agent(e) => {
                let selected = i == app.selected;
                let (dot, color) = t.dot(e.status);
                let age = elapsed(e.since);
                let kind = format!("{:<6}", truncate(&e.kind, 6));
                let fixed = 2 + 7 + age.chars().count() + 1;
                let label = truncate(&e.label, w.saturating_sub(fixed));
                let gap = w.saturating_sub(fixed - 1 + label.chars().count());
                let (base, dim, dotc) = if selected {
                    (t.selected(), t.selected(), t.selected())
                } else {
                    (Style::new().fg(t.paper), Style::new().fg(t.muted), Style::new().fg(color))
                };
                let mut lines = vec![Line::from(vec![
                    Span::styled(format!("{dot} "), dotc),
                    Span::styled(kind, dim),
                    Span::styled(" ", base),
                    Span::styled(label, base),
                    Span::styled(" ".repeat(gap), base),
                    Span::styled(age, dim),
                ])];
                if two_lines {
                    let dir = short_dir(&e.dir, &app.home, w.saturating_sub(9));
                    lines.push(Line::from(vec![Span::styled(pad(&format!("         {dir}"), w), dim)]));
                }
                items.push(ListItem::new(Text::from(lines)));
            }
        }
    }
    if app.rows.is_empty() {
        items.push(ListItem::new(Line::styled("  looking for herdr…", Style::new().fg(t.muted))));
    }
    let mut state = ListState::default().with_selected(Some(app.selected));
    f.render_stateful_widget(List::new(items), inner, &mut state);
}

fn screen(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme;
    let Some(e) = app.current() else {
        let b = Block::bordered().border_style(Style::new().fg(t.muted));
        let p = Paragraph::new(Line::styled("no agent selected", Style::new().fg(t.muted))).block(b).centered();
        f.render_widget(p, area);
        return;
    };
    let title = Line::from(vec![
        Span::raw(" "),
        Span::styled(e.label.clone(), Style::new().add_modifier(Modifier::BOLD)),
        Span::styled(format!(" · {} · ", e.kind), Style::new().fg(t.muted)),
        Span::styled(format!("{} {}", e.status.word(), elapsed(e.since)), Style::new().fg(t.status_color(e.status))),
        Span::styled(format!(" · {} ", e.target.machine), Style::new().fg(t.muted)),
    ]);
    let block = Block::bordered().border_style(Style::new().fg(t.muted)).title(title);
    let body = match (&app.screen, &app.screen_err) {
        (Some((_, text)), _) => text.clone(),
        (None, Some(err)) => Text::from(Line::styled(format!("cannot read the screen: {err}"), Style::new().fg(t.pause))),
        (None, None) => Text::from(Line::styled("reading…", Style::new().fg(t.muted))),
    };
    f.render_widget(Paragraph::new(body).block(block), area);
}

fn composer(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme;
    let active = app.mode == Mode::Insert;
    let target = app.current().map(|e| e.label.clone());
    let title = match (&target, active) {
        (Some(l), true) => format!(" to {l} · Enter sends · Shift+Enter newline · Esc back "),
        (Some(l), false) => format!(" to {l} · i to write "),
        (None, _) => " nothing selected ".to_string(),
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if active { t.pr } else { t.panel }))
        .title(Line::styled(title, Style::new().fg(if active { t.pr } else { t.muted })));
    let inner = block.inner(area);
    let width = inner.width.max(1) as usize;
    // Where the cursor lands once the text wraps at the box width.
    let (mut row, mut col) = (0usize, 0usize);
    let mut cursor = (0usize, 0usize);
    for (i, ch) in app.composer.text.chars().enumerate() {
        if i == app.composer.cursor {
            cursor = (row, col);
        }
        if ch == '\n' || col + 1 >= width {
            row += 1;
            col = 0;
            if ch == '\n' {
                continue;
            }
        }
        col += 1;
    }
    if app.composer.cursor >= app.composer.len() {
        cursor = (row, col);
    }
    let scroll = (cursor.0 + 1).saturating_sub(inner.height.max(1) as usize) as u16;
    let text = if app.composer.text.is_empty() && !active {
        Text::from(Line::styled("Describe a task or ask a question…", Style::new().fg(t.muted)))
    } else {
        Text::raw(app.composer.text.clone())
    };
    f.render_widget(Paragraph::new(text).block(block).wrap(Wrap { trim: false }).scroll((scroll, 0)), area);
    if active {
        f.set_cursor_position(Position::new(inner.x + cursor.1 as u16, inner.y + (cursor.0 as u16).saturating_sub(scroll)));
    }
}

fn status_bar(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme;
    let mode = match app.mode {
        Mode::Normal => "NORMAL",
        Mode::Insert => "WRITE",
        Mode::Filter => "FILTER",
        Mode::Help => "HELP",
        Mode::New => "NEW",
        Mode::Close => "CLOSE",
    };
    let (working, needs) = app.counts();
    let mut spans = vec![
        Span::styled(format!(" {mode} "), Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(format!(" {working} working · {needs} need you  ")),
    ];
    if !app.filter.is_empty() {
        spans.push(Span::styled(format!("filter: {}  ", app.filter), Style::new().add_modifier(Modifier::ITALIC)));
    }
    if let Some((toast, _)) = app.toasts.last() {
        spans.push(Span::styled(toast.clone(), Style::new().add_modifier(Modifier::BOLD)));
    } else {
        let hints = match app.mode {
            Mode::Insert => "Enter send  Shift+Enter newline  Esc back",
            Mode::Filter => "type to filter  Enter keep  Esc clear",
            _ if app.narrow() => "j/k move  Enter open  Tab list/agent  n new  ? help  q quit",
            _ => "j/k move  Enter open  i write  n new  x close  / filter  ? help  q quit",
        };
        spans.push(Span::raw(hints));
    }
    f.render_widget(Paragraph::new(Line::from(spans)).style(t.status_bar()), area);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn dialog_block(app: &App, title: &str) -> Block<'static> {
    let t = app.theme;
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(t.neon))
        .title(Line::styled(format!(" {title} "), t.accent()))
        .style(Style::new().bg(t.ink).fg(t.paper))
}

fn help(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme;
    let keys = [
        ("j/k ↑/↓", "move between agents"),
        ("1-9", "jump to the nth agent"),
        ("Enter", "inside herdr: open the agent's pane · else: write to it"),
        ("i, Tab", "write a message (Enter sends, Shift+Enter newline)"),
        ("a", "open the agent's pane in herdr (inside herdr only)"),
        ("n", "new agent: machine, folder, claude/codex/pi/shell, label"),
        ("x", "close the selected workspace"),
        ("/", "filter by label, kind, folder, machine"),
        ("s", "show plain shells too"),
        ("b", "hide/show the list"),
        ("r", "refresh now"),
        ("q", "quit"),
    ];
    let mut lines: Vec<Line> = keys
        .iter()
        .map(|(k, d)| Line::from(vec![Span::styled(format!("  {k:<9}"), Style::new().fg(t.neon)), Span::raw(d.to_string())]))
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::styled("  Times count from when this screen first saw the state; herdr keeps no start time.", Style::new().fg(t.muted)));
    lines.push(Line::styled("  The screen is the pane's own width: a narrower window clips it on the right.", Style::new().fg(t.muted)));
    let r = centered(area, 76, lines.len() as u16 + 2);
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(lines).block(dialog_block(app, "keys")), r);
}

fn new_dialog(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme;
    let d = &app.new_agent;
    let machines = app.order();
    let pick = |items: Vec<String>, sel: usize| -> Vec<Line<'static>> {
        items
            .into_iter()
            .enumerate()
            .map(|(i, s)| {
                if i == sel {
                    Line::styled(format!(" ❯ {s} "), t.selected())
                } else {
                    Line::styled(format!("   {s} "), Style::new().fg(t.paper))
                }
            })
            .collect()
    };
    let (question, mut body): (&str, Vec<Line>) = match d.step {
        0 => ("Where?", pick(machines.clone(), d.machine)),
        1 => {
            let mut items: Vec<String> = d.folders.iter().map(|(s, _)| s.clone()).collect();
            items.push("type a folder…".into());
            ("Which folder?", pick(items, d.folder))
        }
        2 => ("Folder (on that machine):", vec![Line::styled(format!(" {}▏", d.custom), Style::new().fg(t.paper))]),
        3 => ("Which agent?", pick(KINDS.iter().map(|k| k.to_string()).collect(), d.kind)),
        _ => ("Label (optional, Enter to start):", vec![Line::styled(format!(" {}▏", d.label), Style::new().fg(t.paper))]),
    };
    let mut lines = vec![Line::styled(format!(" {question}"), Style::new().add_modifier(Modifier::BOLD)), Line::raw("")];
    lines.append(&mut body);
    lines.push(Line::raw(""));
    lines.push(Line::styled(" ↑/↓ pick · Enter next · Esc back", Style::new().fg(t.muted)));
    let r = centered(area, 56, lines.len() as u16 + 2);
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(lines).block(dialog_block(app, &format!("new agent {}/4", d.step.min(4).max(1)))), r);
}

fn close_dialog(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme;
    let label = app.current().map(|e| e.label.clone()).unwrap_or_default();
    let lines = vec![
        Line::raw(format!(" Close {label}?")),
        Line::styled(" The workspace and everything in it ends.", Style::new().fg(t.muted)),
        Line::raw(""),
        Line::styled(" Enter/y close · anything else keeps it", Style::new().fg(t.muted)),
    ];
    let r = centered(area, 56, 6);
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(lines).block(dialog_block(app, "close")), r);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::machines::Machine;
    use crate::herdr::types::{parse_snapshot, FIXTURE};
    use crate::theme::Theme;
    use crate::worker::Msg;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::path::PathBuf;

    fn app() -> App {
        let mut app = App::new(Theme::build(false), "agentbox".into(), None, PathBuf::from("/home/dora"));
        app.apply(Msg::Machines(vec![
            Machine { name: "agentbox".into(), sock: PathBuf::from("/tmp/a.sock"), local: true },
            Machine { name: "laptop".into(), sock: PathBuf::from("/tmp/l.sock"), local: false },
        ]));
        let v: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        app.apply(Msg::Snapshot("agentbox".into(), parse_snapshot(&v["result"]).unwrap()));
        app.apply(Msg::Down("laptop".into(), "connection refused".into()));
        app
    }

    fn render(app: &mut App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, app)).unwrap();
        let buf = t.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..h {
            for x in 0..w {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn wide_layout_has_list_screen_composer_and_bar() {
        let mut app = app();
        let s = render(&mut app, 120, 30);
        println!("{s}");
        assert!(s.contains("FEMME"));
        assert!(s.contains("#14 add csv export"));
        assert!(s.contains("laptop · down"));
        assert!(s.contains("to #14 add csv export"));
        assert!(s.contains("NORMAL"));
        assert!(s.contains("reading…"), "the screen area waits for the first pane read");
    }

    #[test]
    fn narrow_layout_switches_between_list_and_agent() {
        let mut app = app();
        let s = render(&mut app, 50, 24);
        println!("{s}");
        assert!(s.contains("#14 add csv export"));
        assert!(!s.contains("reading…"), "the list fills a narrow screen");
        app.narrow_agent = true;
        app.mode = Mode::Insert;
        app.composer.insert_str("hello");
        let s = render(&mut app, 50, 24);
        println!("{s}");
        assert!(s.contains("reading…"));
        assert!(s.contains("hello"));
        assert!(s.contains("WRITE"));
    }

    #[test]
    fn dialogs_render() {
        let mut app = app();
        app.mode = Mode::Help;
        assert!(render(&mut app, 100, 30).contains("jump to the nth agent"));
        app.mode = Mode::Normal;
        app.handle(crate::keys::Action::New, &std::sync::mpsc::channel().0);
        let s = render(&mut app, 100, 30);
        println!("{s}");
        assert!(s.contains("Where?"));
        assert!(s.contains("❯ agentbox"));
        app.mode = Mode::Close;
        assert!(render(&mut app, 100, 30).contains("Close #14 add csv export?"));
    }
}
