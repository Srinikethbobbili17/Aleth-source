//! Start page: the logo and reading stats on the left, one tile per folder on the
//! right. Opening a tile shows that folder's articles in the inbox.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};

use crate::app::{App, WINDOWS};
use crate::data::{Health, short_duration};
use crate::mock_ai::Entry;
use crate::ui::{ACCENT, AI, DIM, truncate};

/// "ALETH" in block letters, one glyph per letter so each can take its own colour.
const LOGO: [[&str; 5]; 5] = [
    [" █████ ", "██   ██", "███████", "██   ██", "██   ██"],
    ["██     ", "██     ", "██     ", "██     ", "███████"],
    ["███████", "██     ", "█████  ", "██     ", "███████"],
    ["████████", "   ██   ", "   ██   ", "   ██   ", "   ██   "],
    ["██   ██", "██   ██", "███████", "██   ██", "██   ██"],
];
const LOGO_COLORS: [Color; 5] = [
    Color::Cyan,
    Color::LightCyan,
    Color::LightBlue,
    Color::LightMagenta,
    Color::Magenta,
];

/// Folder accents, assigned in folder order.
const FOLDER_COLORS: [Color; 8] = [
    Color::Cyan,
    Color::Magenta,
    Color::Yellow,
    Color::Green,
    Color::LightBlue,
    Color::LightRed,
    Color::LightCyan,
    Color::LightMagenta,
];

const TILE_HEIGHT: u16 = 9;
const TILE_MIN_WIDTH: u16 = 38;
const HEADLINES: usize = 4;

pub fn folder_color(index: usize) -> Color {
    FOLDER_COLORS[index % FOLDER_COLORS.len()]
}

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    if area.width >= 110 {
        let [brand, grid] =
            Layout::horizontal([Constraint::Length(50), Constraint::Min(0)]).areas(area);
        draw_brand(frame, app, brand);
        draw_grid(frame, app, grid);
    } else {
        let [brand, grid] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
        draw_brand_compact(frame, app, brand);
        draw_grid(frame, app, grid);
    }
}

struct Stats {
    unread: usize,
    total: usize,
    feeds: usize,
    healthy: usize,
    failing: usize,
    dead: usize,
    newest: String,
    stories: usize,
}

fn stats(app: &App) -> Stats {
    let health = |h| app.ds.feeds.iter().filter(|f| f.health() == h).count();
    Stats {
        unread: app.unread_in(&crate::app::Source::All),
        total: app.ds.items.len(),
        feeds: app.ds.feeds.len(),
        healthy: health(Health::Ok),
        failing: health(Health::Failing),
        dead: health(Health::Dead),
        newest: app
            .ds
            .items
            .first()
            .map_or("–".into(), |i| short_duration(app.ds.now - i.when())),
        stories: app
            .brief
            .iter()
            .filter(|e| matches!(e, Entry::Story { .. }))
            .count(),
    }
}

fn draw_brand(frame: &mut Frame, app: &App, area: Rect) {
    let s = stats(app);
    let mut lines: Vec<Line> = (0..5)
        .map(|row| {
            let mut spans = vec![Span::raw("  ")];
            for (letter, color) in LOGO.iter().zip(LOGO_COLORS) {
                spans.push(Span::styled(letter[row], Style::new().fg(color).bold()));
                spans.push(Span::raw(" "));
            }
            Line::from(spans)
        })
        .collect();
    let key = |k: &'static str| Span::styled(format!("{k:<3}"), Style::new().fg(ACCENT).bold());
    lines.extend([
        Line::raw(""),
        Line::styled("  read what changed", DIM.italic()),
        Line::raw(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{}", s.unread), Style::new().fg(ACCENT).bold()),
            Span::raw(format!(" unread of {} articles", s.total)),
        ]),
        Line::raw(format!(
            "  {} feeds in {} folders · newest {} ago",
            s.feeds,
            app.folders.len(),
            s.newest
        )),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("● {} healthy", s.healthy),
                Style::new().fg(Color::Green),
            ),
            Span::styled(
                format!("  ! {} failing", s.failing),
                Style::new().fg(Color::Yellow),
            ),
            Span::styled(format!("  ✝ {} dead", s.dead), Style::new().fg(Color::Red)),
        ]),
        Line::from(vec![
            Span::styled(
                format!("  ◆ {} cross-feed stories", s.stories),
                Style::new().fg(AI),
            ),
            Span::styled(
                format!(" in the last {} (mock)", WINDOWS[app.window].1),
                DIM,
            ),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::raw("  "),
            key("⏎"),
            Span::raw("open folder     "),
            key("a"),
            Span::raw("all articles"),
        ]),
        Line::from(vec![
            Span::raw("  "),
            key("s"),
            Span::raw("starred         "),
            key("/"),
            Span::raw("search"),
        ]),
        Line::from(vec![
            Span::raw("  "),
            key("2"),
            Span::raw("briefing        "),
            key("3"),
            Span::raw("feed health"),
        ]),
        Line::from(vec![
            Span::raw("  "),
            key("?"),
            Span::raw("all keys        "),
            key("q"),
            Span::raw("quit"),
        ]),
    ]);
    // Top-aligned, level with the first row of folder tiles.
    let top = Rect {
        y: area.y + 2,
        height: area.height.saturating_sub(2),
        ..area
    };
    frame.render_widget(Paragraph::new(lines), top);
}

fn draw_brand_compact(frame: &mut Frame, app: &App, area: Rect) {
    let s = stats(app);
    let mut title = vec![Span::raw(" ")];
    for (letter, color) in "ALETH".chars().zip(LOGO_COLORS) {
        title.push(Span::styled(
            format!("{letter} "),
            Style::new().fg(color).bold(),
        ));
    }
    title.push(Span::styled(" read what changed", DIM.italic()));
    let info = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            format!("{} unread", s.unread),
            Style::new().fg(ACCENT).bold(),
        ),
        Span::raw(format!(" · {} feeds · ", s.feeds)),
        Span::styled(
            format!("{} failing", s.failing),
            Style::new().fg(Color::Yellow),
        ),
        Span::raw(" · "),
        Span::styled(format!("{} dead", s.dead), Style::new().fg(Color::Red)),
    ]);
    frame.render_widget(Paragraph::new(vec![Line::from(title), info]), area);
}

fn draw_grid(frame: &mut Frame, app: &mut App, area: Rect) {
    let [heading, area] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    let n = app.folders.len();
    frame.render_widget(
        Line::from(vec![
            Span::styled(" Folders", Style::new().bold()),
            Span::styled(format!(" · {n}"), DIM),
        ]),
        heading,
    );
    if n == 0 {
        frame.render_widget(Paragraph::new(" No folders yet.").style(DIM), area);
        return;
    }

    let cols = (area.width / TILE_MIN_WIDTH).clamp(1, 3) as usize;
    app.home_columns = cols;
    app.home_selected = app.home_selected.min(n - 1);
    let rows = n.div_ceil(cols);
    let visible_rows = ((area.height / TILE_HEIGHT) as usize).max(1);
    // Scroll so the selected tile's row is always on screen.
    let first_row = (app.home_selected / cols).saturating_sub(visible_rows - 1);

    for r in 0..visible_rows.min(rows - first_row) {
        let row_area = Rect {
            y: area.y + r as u16 * TILE_HEIGHT,
            height: TILE_HEIGHT.min(area.height.saturating_sub(r as u16 * TILE_HEIGHT)),
            ..area
        };
        let cells =
            Layout::horizontal(vec![Constraint::Ratio(1, cols as u32); cols]).split(row_area);
        for c in 0..cols {
            let index = (first_row + r) * cols + c;
            if index < n {
                draw_tile(frame, app, index, cells[c]);
            }
        }
    }
    if first_row + visible_rows < rows {
        let more = Line::styled(
            format!("↓ {} more ", n - (first_row + visible_rows) * cols),
            DIM,
        )
        .right_aligned();
        frame.render_widget(more, heading);
    }
}

fn draw_tile(frame: &mut Frame, app: &App, index: usize, area: Rect) {
    let folder = &app.folders[index];
    let color = folder_color(index);
    let selected = index == app.home_selected;

    let items: Vec<usize> = (0..app.ds.items.len())
        .filter(|&i| app.ds.folder_of(&app.ds.items[i]) == Some(folder.as_str()))
        .collect();
    let unread = items
        .iter()
        .filter(|&&i| !app.ds.read.contains(&app.ds.items[i].id))
        .count();
    let feeds: Vec<_> = app
        .ds
        .feeds
        .iter()
        .filter(|f| f.folder.as_ref() == Some(folder))
        .collect();
    let failing = feeds
        .iter()
        .filter(|f| f.health() == Health::Failing)
        .count();
    let dead = feeds.iter().filter(|f| f.health() == Health::Dead).count();

    let mut block = Block::bordered()
        .title(Line::styled(
            format!(" {folder} "),
            Style::new().fg(color).bold(),
        ))
        .title(
            Line::styled(
                if unread > 0 {
                    format!(" {unread} ")
                } else {
                    " ✓ ".into()
                },
                Style::new().fg(color),
            )
            .right_aligned(),
        );
    block = if selected {
        block
            .border_type(BorderType::Thick)
            .border_style(Style::new().fg(color))
    } else {
        block.border_type(BorderType::Rounded).border_style(DIM)
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width.saturating_sub(2) as usize;
    let health = if dead > 0 {
        Span::styled(format!("✝ {dead} dead"), Style::new().fg(Color::Red))
    } else if failing > 0 {
        Span::styled(
            format!("! {failing} failing"),
            Style::new().fg(Color::Yellow),
        )
    } else {
        Span::styled("● healthy", Style::new().fg(Color::Green))
    };
    let mut lines = vec![
        Line::from(vec![
            Span::raw(format!("{unread} unread")),
            Span::styled(format!(" · {} articles", items.len()), DIM),
        ]),
        Line::from(vec![
            Span::styled(
                format!(
                    "{} feed{} · ",
                    feeds.len(),
                    if feeds.len() == 1 { "" } else { "s" }
                ),
                DIM,
            ),
            health,
        ]),
        Line::raw(""),
    ];
    for &i in items.iter().take(HEADLINES) {
        let item = &app.ds.items[i];
        let is_unread = !app.ds.read.contains(&item.id);
        let age = format!("{:>3} ", short_duration(app.ds.now - item.when()));
        let title = truncate(item.title(), width.saturating_sub(age.len() + 2));
        lines.push(Line::from(vec![
            if is_unread {
                Span::styled("● ", Style::new().fg(color))
            } else {
                Span::raw("  ")
            },
            Span::styled(age, DIM),
            if is_unread {
                Span::raw(title).bold()
            } else {
                Span::styled(title, DIM)
            },
        ]));
    }
    if items.is_empty() {
        lines.push(Line::styled("no articles yet", DIM));
    }
    let text_area = Rect {
        x: inner.x + 1,
        width: inner.width.saturating_sub(1),
        ..inner
    };
    frame.render_widget(Paragraph::new(lines), text_area);
}
