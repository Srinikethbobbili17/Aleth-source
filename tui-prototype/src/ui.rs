use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, List, ListItem, Paragraph, Row, Table, Tabs,
};

use crate::app::{App, Focus, Reader, Source, View, WINDOWS};
use crate::data::{Health, html_to_text, short_duration};
use crate::home;
use crate::mock_ai::{self, Entry};

pub const ACCENT: Color = Color::Cyan;
pub const AI: Color = Color::Magenta;
const STAR: Color = Color::Yellow;
pub const DIM: Style = Style::new().fg(Color::DarkGray);

/// Readers rarely want lines longer than this, even on wide terminals.
const MAX_TEXT_WIDTH: u16 = 100;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    draw_header(frame, app, header);
    match app.view {
        View::Home => home::draw(frame, app, body),
        View::Inbox => draw_inbox(frame, app, body),
        View::Briefing => draw_briefing(frame, app, body),
        View::Feeds => draw_feeds(frame, app, body),
    }
    draw_footer(frame, app, footer);
    if app.help {
        draw_help(frame);
    }
}

pub fn panel(title: &str, focused: bool) -> Block<'_> {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(format!(" {title} "));
    if focused {
        block.border_style(Style::new().fg(ACCENT))
    } else {
        block.border_style(DIM)
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    // The inbox is a page below Home, shown as a breadcrumb on the Home tab.
    let home = match (app.view, &app.scope) {
        (View::Inbox, Some(folder)) => format!("1 Home › {folder}"),
        (View::Inbox, None) => "1 Home › All articles".to_string(),
        _ => "1 Home".to_string(),
    };
    let selected = match app.view {
        View::Home | View::Inbox => 0,
        View::Briefing => 1,
        View::Feeds => 2,
    };
    let tabs = Tabs::new([home, "2 Briefing".into(), "3 Feeds".into()])
        .select(selected)
        .highlight_style(
            Style::new()
                .fg(ACCENT)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        )
        .divider(" ");
    let [left, right] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(44)]).areas(area);
    frame.render_widget(tabs, left);
    let unread = app.ds.items.len() - app.ds.read.len().min(app.ds.items.len());
    let info = format!("aleth prototype · {} · {unread} unread ", app.ds.source);
    frame.render_widget(Line::from(info).style(DIM).right_aligned(), right);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let line = if app.searching {
        Line::from(vec![
            Span::from("/").fg(ACCENT),
            Span::from(app.query.as_str()),
            Span::from("▏").fg(ACCENT),
        ])
    } else {
        let hints = match app.view {
            View::Home => {
                "h/j/k/l move · ⏎ open folder · a all articles · s starred · / search · 2 briefing · 3 feeds · ? help"
            }
            View::Inbox if !app.query.is_empty() => {
                "match: T title · F feed/folder/author · B text · ~ related terms · ≈ similar meaning   / edit search · Esc in search clears · ? help"
            }
            View::Inbox => {
                "Esc home · j/k move · h/l pane · ⏎ open · n next unread · s star · m read · u unread-only · / search · a AI · z zoom · ? help"
            }
            View::Briefing => {
                "j/k move · l sources · ⏎ open article · w window · c caught up · ? help"
            }
            View::Feeds => "j/k move · ⏎ show articles · r refresh (mock) · ? help",
        };
        Line::from(vec![
            Span::from(format!(" {} ", app.status)).reversed(),
            Span::from(format!("  {hints}")).style(DIM),
        ])
    };
    frame.render_widget(line, area);
}

// ---------------------------------------------------------------- inbox

fn draw_inbox(frame: &mut Frame, app: &mut App, area: Rect) {
    if app.zoom && app.reader.is_some() {
        draw_reader(frame, app, area);
        return;
    }
    let wide = area.width >= 120;
    let [sidebar, rest] =
        Layout::horizontal([Constraint::Length(30), Constraint::Min(0)]).areas(area);
    draw_sources(frame, app, sidebar);
    if wide {
        let [list, reader] =
            Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
                .areas(rest);
        draw_list(frame, app, list);
        draw_reader(frame, app, reader);
    } else if app.focus == Focus::Reader {
        draw_reader(frame, app, rest);
    } else {
        draw_list(frame, app, rest);
    }
}

fn draw_sources(frame: &mut Frame, app: &mut App, area: Rect) {
    let items: Vec<ListItem> = app
        .sources
        .iter()
        .map(|source| {
            let unread = app.unread_in(source);
            let count = if unread > 0 {
                format!("{unread}")
            } else {
                String::new()
            };
            let (label, style, badge) = match source {
                Source::All => (
                    "All articles".to_string(),
                    Style::new().bold(),
                    Span::raw(""),
                ),
                Source::Starred => ("Starred".to_string(), Style::new().fg(STAR), Span::raw("")),
                Source::Folder(_) if app.scope.is_some() => (
                    "All in folder".to_string(),
                    Style::new().bold(),
                    Span::raw(""),
                ),
                Source::Folder(name) => (format!("▾ {name}"), Style::new().bold(), Span::raw("")),
                Source::Feed(id) => {
                    let feed = app.ds.feed(*id).unwrap();
                    let badge = match feed.health() {
                        Health::Ok => Span::raw(""),
                        Health::Failing => Span::from(" !").fg(Color::Yellow),
                        Health::Dead => Span::from(" ✝").fg(Color::Red),
                    };
                    (format!("  {}", feed.name()), Style::new(), badge)
                }
            };
            let count = if matches!(source, Source::Starred) {
                format!("{}", app.ds.starred.len())
            } else {
                count
            };
            let width = area.width.saturating_sub(4) as usize;
            let room = width.saturating_sub(count.chars().count() + badge.width() + 1);
            let label = truncate(&label, room);
            let pad =
                width.saturating_sub(label.chars().count() + badge.width() + count.chars().count());
            ListItem::new(Line::from(vec![
                Span::styled(label, style),
                badge,
                Span::raw(" ".repeat(pad)),
                Span::styled(count, DIM),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(panel("Sources", app.focus == Focus::Sidebar))
        .highlight_style(Style::new().bg(Color::DarkGray));
    frame.render_stateful_widget(list, area, &mut app.source_state);
}

fn draw_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let show_feed = !matches!(app.source(), Source::Feed(_));
    let items: Vec<ListItem> = app
        .visible
        .iter()
        .map(|&i| {
            let item = &app.ds.items[i];
            let unread = !app.ds.read.contains(&item.id);
            let mut spans = vec![
                if unread {
                    Span::from("● ").fg(ACCENT)
                } else {
                    Span::raw("  ")
                },
                if app.ds.starred.contains(&item.id) {
                    Span::from("★ ").fg(STAR)
                } else {
                    Span::raw("  ")
                },
                Span::styled(
                    format!("{:>3} ", short_duration(app.ds.now - item.when())),
                    DIM,
                ),
            ];
            if let Some(hit) = app.hits.get(&i) {
                spans.push(Span::styled(
                    format!("{} ", hit.strength.marker()),
                    Style::new().fg(AI),
                ));
            }
            if show_feed {
                spans.push(Span::styled(
                    format!("{:<14} ", truncate(app.ds.feed_name(item.feed_id), 14)),
                    DIM,
                ));
            }
            let title = Span::raw(item.title().to_string());
            spans.push(if unread { title.bold() } else { title });
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut title = match app.source() {
        Source::All => "All articles".to_string(),
        Source::Starred => "Starred".to_string(),
        Source::Folder(f) => f.clone(),
        Source::Feed(id) => app.ds.feed_name(*id).to_string(),
    };
    title.push_str(&format!(" · {}", app.visible.len()));
    if app.unread_only {
        title.push_str(" · unread");
    }
    if !app.query.is_empty() {
        title.push_str(&format!(" · search \"{}\"", app.query));
        if app.semantic.as_ref().is_some_and(|s| s.pending()) {
            title.push_str(" · matching meaning…");
        }
    }
    let list = List::new(items)
        .block(panel(&title, app.focus == Focus::List))
        .highlight_style(Style::new().bg(Color::DarkGray));
    frame.render_stateful_widget(list, area, &mut app.list_state);
}

fn draw_reader(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Reader;
    // Preview the selected article until one is opened.
    let item = match (&app.reader, app.selected_item()) {
        (Some(r), _) if focused || app.zoom => Some(r.item),
        (_, Some(sel)) => Some(sel),
        (Some(r), None) => Some(r.item),
        (None, None) => None,
    };
    let block = panel(if focused { "Article" } else { "Preview" }, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(item) = item else {
        frame.render_widget(Paragraph::new("No article selected").style(DIM), inner);
        return;
    };

    let width = inner.width.saturating_sub(2).min(MAX_TEXT_WIDTH);
    if app.reader.as_ref().is_none_or(|r| r.item != item) {
        app.reader = Some(Reader {
            item,
            scroll: 0,
            paragraphs: Vec::new(),
            width: 0,
        });
    }
    let reader = app.reader.as_mut().unwrap();
    if reader.width != width {
        reader.paragraphs = render_paragraphs(app.ds.items[item].body_html(), width);
        reader.width = width;
    }

    let it = &app.ds.items[item];
    let mut lines: Vec<Line> = Vec::new();
    for l in wrap(it.title(), width as usize) {
        lines.push(Line::from(l).bold());
    }
    let mut meta = vec![Span::from(app.ds.feed_name(it.feed_id).to_string()).fg(ACCENT)];
    if !it.authors.is_empty() {
        meta.push(Span::raw(format!(" · {}", it.authors.join(", "))));
    }
    if let Some(p) = it.published_at {
        meta.push(Span::raw(format!(" · {}", p.format("%a %d %b %Y %H:%M"))));
    }
    if app.ds.starred.contains(&it.id) {
        meta.push(Span::from(" ★").fg(STAR));
    }
    lines.push(Line::from(meta));
    if let Some(url) = &it.url {
        lines.push(Line::styled(truncate(url, width as usize), DIM));
    }
    lines.push(Line::raw(""));

    if let Some(hit) = app.hits.get(&item) {
        lines.push(Line::styled(
            format!("┌ search “{}”", app.query),
            Style::new().fg(AI),
        ));
        for reason in &hit.reasons {
            for l in wrap(reason, (width as usize).saturating_sub(2)) {
                lines.push(Line::from(vec![
                    Span::styled("│ ", Style::new().fg(AI)),
                    Span::raw(l),
                ]));
            }
        }
        lines.push(Line::styled("└ Esc in the search bar clears it", DIM));
        lines.push(Line::raw(""));
    }

    if app.show_internals {
        let rows = [
            ("item / version", format!("{} / {}", it.id, it.version_id)),
            ("feed", it.feed_id.to_string()),
            ("first seen", it.first_seen_at.to_rfc3339()),
            ("observed", it.observed_at.to_rfc3339()),
            (
                "external id",
                it.external_id.clone().unwrap_or_else(|| "–".into()),
            ),
            (
                "content type",
                it.content_type.clone().unwrap_or_else(|| "–".into()),
            ),
        ];
        lines.push(Line::styled("┌ internals", DIM));
        for (k, v) in rows {
            lines.push(Line::from(vec![
                Span::styled(format!("│ {k:<15}"), DIM),
                Span::raw(v),
            ]));
        }
        if it.observed_at > it.first_seen_at {
            lines.push(Line::styled(
                "│ content changed since first seen (newer version stored)",
                Style::new().fg(Color::Yellow),
            ));
        }
        lines.push(Line::styled("└ i: hide", DIM));
        lines.push(Line::raw(""));
    }

    let paragraphs = &app.reader.as_ref().unwrap().paragraphs;
    if app.show_ai {
        let summary = mock_ai::summarize(it.content.is_some(), paragraphs);
        let ai = Style::new().fg(AI);
        lines.push(Line::from(vec![
            Span::styled("┌ AI summary ", ai.bold()),
            Span::styled("MOCK", Style::new().fg(Color::Black).bg(AI)),
            Span::styled(
                format!(" input: {}, {} words", summary.input.label(), summary.words),
                DIM,
            ),
        ]));
        if summary.claims.is_empty() {
            lines.push(Line::styled("│ Unavailable: not enough text. A real model would need full-text extraction first.", ai));
        }
        for claim in &summary.claims {
            let cite = format!(" [¶{}]", claim.paragraph);
            let wrapped = wrap(&claim.text, (width as usize).saturating_sub(4 + cite.len()));
            let last = wrapped.len() - 1;
            for (n, l) in wrapped.into_iter().enumerate() {
                let mut spans = vec![
                    Span::styled(if n == 0 { "│ • " } else { "│   " }, ai),
                    Span::raw(l),
                ];
                if n == last {
                    spans.push(Span::styled(cite.clone(), ai));
                }
                lines.push(Line::from(spans));
            }
        }
        lines.push(Line::styled(
            "└ a: hide · ¶ numbers cite paragraphs below",
            DIM,
        ));
        lines.push(Line::raw(""));
    }

    if paragraphs.is_empty() {
        lines.push(Line::styled(
            "The feed provides no text for this article. Press o to open it in a browser.",
            DIM,
        ));
    }
    for (n, p) in paragraphs.iter().enumerate() {
        for (k, l) in p.lines().enumerate() {
            let gutter = if app.show_ai && k == 0 {
                format!("¶{:<3}", n + 1)
            } else {
                "    ".into()
            };
            lines.push(Line::from(vec![
                Span::styled(gutter, DIM),
                Span::raw(l.to_string()),
            ]));
        }
        lines.push(Line::raw(""));
    }

    let reader = app.reader.as_mut().unwrap();
    let max = (lines.len() as u16).saturating_sub(inner.height);
    reader.scroll = reader.scroll.min(max);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).scroll((reader.scroll, 0)),
        indent(inner),
    );
}

/// Renders article HTML to wrapped plain-text paragraphs.
fn render_paragraphs(html: Option<&str>, width: u16) -> Vec<String> {
    let Some(html) = html else { return Vec::new() };
    let width = (width as usize).saturating_sub(4).max(20);
    let text = html_to_text(html, width);
    text.split("\n\n")
        .map(|p| p.trim_matches('\n').to_string())
        .filter(|p| !p.trim().is_empty())
        .collect()
}

// ---------------------------------------------------------------- briefing

fn draw_briefing(frame: &mut Frame, app: &mut App, area: Rect) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).areas(area);
    let since = short_duration(app.ds.now - app.checkpoint);
    let window = if app.checkpoint == app.ds.now {
        "now".to_string()
    } else {
        format!("last {}", WINDOWS[app.window].1)
    };

    let rows: Vec<ListItem> = app
        .brief
        .iter()
        .map(|e| match e {
            Entry::Story { label, items } => ListItem::new(Line::from(vec![
                Span::styled(format!("◆ {} sources  ", items.len()), Style::new().fg(AI)),
                Span::raw(label.clone()).bold(),
            ])),
            Entry::Folder { name, items } => ListItem::new(Line::from(vec![
                Span::styled(format!("▤ {:>3} new    ", items.len()), DIM),
                Span::raw(name.clone()),
            ])),
        })
        .collect();
    let title = format!("Briefing · new since {window} ({since} ago)");
    if rows.is_empty() {
        let block = panel(&title, !app.brief_sources);
        let msg = Paragraph::new("You're caught up. Press w to look back further.")
            .style(DIM)
            .block(block);
        frame.render_widget(msg, left);
    } else {
        let list = List::new(rows)
            .block(panel(&title, !app.brief_sources))
            .highlight_style(Style::new().bg(Color::DarkGray));
        frame.render_stateful_widget(list, left, &mut app.brief_state);
    }

    let Some(entry) = app.brief_state.selected().and_then(|i| app.brief.get(i)) else {
        frame.render_widget(panel("Details", false), right);
        return;
    };
    let block = panel("What's new", app.brief_sources);
    let inner = block.inner(right);
    frame.render_widget(block, right);
    let width = inner.width.saturating_sub(2) as usize;
    let ai = Style::new().fg(AI);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("MOCK", Style::new().fg(Color::Black).bg(AI)),
            Span::styled(
                match entry {
                    Entry::Story { .. } => " grouped because titles share words across feeds",
                    Entry::Folder { .. } => " newest items in this folder, not grouped",
                },
                DIM,
            ),
        ]),
        Line::raw(""),
    ];
    let shown = &entry.items()[..entry.items().len().min(8)];
    for (n, &i) in shown.iter().enumerate() {
        let it = &app.ds.items[i];
        let html = it.body_html().unwrap_or("");
        let text = html_to_text(html, 10_000);
        let sentence = mock_ai::lead_sentence(&text, 220);
        // Link-only blurbs (Hacker News, Lobsters) say less than the title.
        let useful = sentence.split_whitespace().count() >= 6 && !sentence.contains("://");
        let claim = if useful {
            sentence
        } else {
            it.title().to_string()
        };
        let cite = format!(" [{}]", n + 1);
        let wrapped = wrap(&claim, width.saturating_sub(2 + cite.len()));
        let last = wrapped.len() - 1;
        for (k, l) in wrapped.into_iter().enumerate() {
            let mut spans = vec![Span::raw(if k == 0 { "• " } else { "  " }), Span::raw(l)];
            if k == last {
                spans.push(Span::styled(cite.clone(), ai));
            }
            lines.push(Line::from(spans));
        }
    }
    if entry.items().len() > shown.len() {
        lines.push(Line::styled(
            format!("  … {} more", entry.items().len() - shown.len()),
            DIM,
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("Sources", Style::new().bold()));
    let [text_area, sources_area] =
        Layout::vertical([Constraint::Length(lines.len() as u16), Constraint::Min(3)]).areas(inner);
    frame.render_widget(Paragraph::new(lines), indent(text_area));

    let sources: Vec<ListItem> = entry
        .items()
        .iter()
        .enumerate()
        .map(|(n, &i)| {
            let it = &app.ds.items[i];
            ListItem::new(Line::from(vec![
                Span::styled(format!("[{}] ", n + 1), ai),
                Span::styled(
                    format!("{:>3} ", short_duration(app.ds.now - it.when())),
                    DIM,
                ),
                Span::styled(
                    format!("{:<14} ", truncate(app.ds.feed_name(it.feed_id), 14)),
                    DIM,
                ),
                Span::raw(it.title().to_string()),
            ]))
        })
        .collect();
    let mut list = List::new(sources);
    if app.brief_sources {
        list = list.highlight_style(Style::new().bg(Color::DarkGray));
    }
    frame.render_stateful_widget(list, indent(sources_area), &mut app.brief_source_state);
}

// ---------------------------------------------------------------- feeds

fn draw_feeds(frame: &mut Frame, app: &mut App, area: Rect) {
    let [table_area, detail_area] =
        Layout::vertical([Constraint::Min(5), Constraint::Length(10)]).areas(area);
    let now = app.ds.now;
    let rows: Vec<Row> = app
        .ds
        .feeds
        .iter()
        .map(|f| {
            let (dot, color) = match f.health() {
                Health::Ok => ("●", Color::Green),
                Health::Failing => ("●", Color::Yellow),
                Health::Dead => ("✝", Color::Red),
            };
            let count = app.ds.items.iter().filter(|i| i.feed_id == f.id).count();
            let unread = app.unread_in(&Source::Feed(f.id));
            let last_ok = match f.last_success_at.map(|t| short_duration(now - t)) {
                None => "never".to_string(),
                Some(d) if d == "now" => "just now".to_string(),
                Some(d) => format!("{d} ago"),
            };
            let next = match (f.dead_at, f.next_poll_at) {
                (Some(_), _) => "stopped".to_string(),
                (None, Some(t)) if t > now => format!("in {}", short_duration(t - now)),
                (None, Some(t)) => format!("late {}", short_duration(now - t)),
                (None, None) => "–".into(),
            };
            Row::new(vec![
                Cell::from(dot).fg(color),
                Cell::from(f.name().to_string()),
                Cell::from(f.folder.clone().unwrap_or_default()).style(DIM),
                Cell::from(format!("{count:>4}")),
                Cell::from(format!("{unread:>4}")),
                Cell::from(last_ok),
                Cell::from(next),
                Cell::from(if f.failure_count > 0 {
                    f.failure_count.to_string()
                } else {
                    String::new()
                }),
                Cell::from(f.last_error_code.clone().unwrap_or_default()).fg(Color::Red),
            ])
        })
        .collect();
    let header = Row::new([
        "",
        "Feed",
        "Folder",
        "Items",
        "Unrd",
        "Last OK",
        "Next poll",
        "Fails",
        "Error",
    ])
    .style(Style::new().bold())
    .bottom_margin(1);
    let widths = [
        Constraint::Length(1),
        Constraint::Fill(3),
        Constraint::Fill(2),
        Constraint::Length(5),
        Constraint::Length(5),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(5),
        Constraint::Length(14),
    ];
    let healthy = app
        .ds
        .feeds
        .iter()
        .filter(|f| f.health() == Health::Ok)
        .count();
    let title = format!("Feeds · {healthy}/{} healthy", app.ds.feeds.len());
    let table = Table::new(rows, widths)
        .header(header)
        .block(panel(&title, true))
        .row_highlight_style(Style::new().bg(Color::DarkGray));
    frame.render_stateful_widget(table, table_area, &mut app.feeds_state);

    let Some(f) = app.feeds_state.selected().map(|i| &app.ds.feeds[i]) else {
        return;
    };
    let ts = |t: Option<chrono::DateTime<chrono::Utc>>| {
        t.map_or("–".into(), |t| t.format("%d %b %H:%M:%S").to_string())
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled("url            ", DIM),
            Span::raw(f.url.clone()),
        ]),
        Line::from(vec![
            Span::styled("last fetched   ", DIM),
            Span::raw(ts(f.last_fetched_at)),
        ]),
        Line::from(vec![
            Span::styled("last success   ", DIM),
            Span::raw(ts(f.last_success_at)),
        ]),
        Line::from(vec![
            Span::styled("next poll      ", DIM),
            Span::raw(ts(f.next_poll_at)),
        ]),
        Line::from(vec![
            Span::styled("retry not bef. ", DIM),
            Span::raw(ts(f.retry_not_before)),
        ]),
    ];
    if let Some(err) = &f.last_error {
        lines.push(Line::from(vec![
            Span::styled("last error     ", DIM),
            Span::from(format!("{} ", f.last_error_code.as_deref().unwrap_or("?"))).fg(Color::Red),
            Span::raw(err.clone()),
        ]));
    }
    if f.dead_at.is_some() {
        lines.push(Line::styled(
            "dead after 10 consecutive failures; polling skips it until a manual refresh succeeds",
            Style::new().fg(Color::Red),
        ));
    }
    frame.render_widget(
        Paragraph::new(lines).block(panel(f.name(), false)),
        detail_area,
    );
}

// ---------------------------------------------------------------- help

fn draw_help(frame: &mut Frame) {
    let keys: &[(&str, &str)] = &[
        ("Global", ""),
        ("1 2 3 / Tab", "Home, Briefing, Feeds"),
        ("Esc / ⌫", "back to Home"),
        ("q / Ctrl-c", "quit"),
        ("Home", ""),
        ("h j k l ← ↓ ↑ →", "move between folders"),
        ("⏎", "open folder"),
        ("a / s", "all articles / starred"),
        ("Inbox", ""),
        ("h l ← →", "move between sources, list, article"),
        ("j k g G PgUp PgDn", "move / scroll"),
        ("⏎", "open article"),
        ("n", "open next unread"),
        ("m / M", "toggle read / mark all shown as read"),
        ("s", "toggle star"),
        ("u", "show unread only"),
        (
            "/",
            "topic search: words, related terms, meaning (Esc clears)",
        ),
        ("a", "toggle mock AI summary"),
        ("i", "toggle internals (ids, versions)"),
        ("z", "zoom article to full screen"),
        ("o", "open in browser"),
        ("r", "refresh feed (mock)"),
        ("Briefing", ""),
        ("l / h", "into sources / back"),
        ("⏎", "open the selected source in the inbox"),
        ("w", "change look-back window"),
        ("c", "mark caught up"),
    ];
    let lines: Vec<Line> = keys
        .iter()
        .map(|(k, v)| {
            if v.is_empty() {
                Line::styled(*k, Style::new().fg(ACCENT).bold())
            } else {
                Line::from(vec![Span::raw(format!("  {k:<20}")), Span::styled(*v, DIM)])
            }
        })
        .collect();
    let area = frame.area();
    let [area] = Layout::vertical([Constraint::Length(lines.len() as u16 + 2)])
        .flex(Flex::Center)
        .areas(area);
    let [area] = Layout::horizontal([Constraint::Length(62)])
        .flex(Flex::Center)
        .areas(area);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(panel("Keys · any key closes", true)),
        area,
    );
}

// ---------------------------------------------------------------- text helpers

/// Leaves a one-column margin inside a panel's border.
fn indent(area: Rect) -> Rect {
    Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(1),
        ..area
    }
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max == 0 {
        String::new()
    } else {
        let cut: String = s.chars().take(max - 1).collect();
        format!("{cut}…")
    }
}

fn wrap(s: &str, width: usize) -> Vec<String> {
    let width = width.max(10);
    let mut lines = vec![String::new()];
    for word in s.split_whitespace() {
        let cur = lines.last_mut().unwrap();
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > width {
            lines.push(word.to_string());
        } else {
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
        }
    }
    lines
}
