use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, HighlightSpacing, List, ListItem, Paragraph, Row, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Table, Tabs,
};

use crate::app::{App, Focus, Reader, Source, View, WINDOWS};
use crate::article::{self, Kind, Para};
use crate::data::{Health, Item, html_to_text, short_duration};
use crate::home;
use crate::mock_ai::{self, Entry};
use crate::summary::{self, Input, MIN_WORDS, Point, State};

pub const ACCENT: Color = Color::Cyan;
pub const AI: Color = Color::Magenta;
const STAR: Color = Color::Yellow;
const WARN: Color = Color::Yellow;
pub const DIM: Style = Style::new().fg(Color::DarkGray);

/// About 75 characters of text per line plus the paragraph numbers: past
/// that, eyes lose their place when jumping back to the next line.
const MAX_TEXT_WIDTH: u16 = 80;
/// Paragraph numbers left of the text, which summaries cite.
const GUTTER: usize = 5;

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
                "hjkl move · ⏎ open folder · a all articles · s starred · / search · 2 briefing · 3 feeds · ? help"
            }
            View::Inbox if !app.query.is_empty() => {
                "T title · F feed/folder/author · B text · ~ related terms · ≈ meaning · / edit search · ? help"
            }
            View::Inbox if app.focus == Focus::Reader => {
                "j/k scroll · Space page · G end (summary) · ]/[ related · ⏎ open related · A summarize again · a AI · z zoom · o browser · Esc back · ? help"
            }
            View::Inbox => {
                "Esc home · j/k move · h/l pane · ⏎ open · n next unread · s star · m read · u unread · / search · a AI · z zoom · ? help"
            }
            View::Briefing => {
                "j/k move · l sources · ⏎ open article · w window · c caught up · ? help"
            }
            View::Feeds => "j/k move · ⏎ show articles · r refresh (mock) · ? help",
        };
        let status = format!(" {} ", truncate(&app.status, area.width as usize / 2));
        let room = (area.width as usize).saturating_sub(status.chars().count() + 2);
        let mut spans = vec![Span::from(status).reversed(), Span::raw("  ")];
        spans.extend(hint_spans(hints, room));
        Line::from(spans)
    };
    frame.render_widget(line, area);
}

/// "j/k move · ⏎ open" → the keys in the accent colour, labels dimmed, cut at
/// a whole hint so nothing ends mid-word.
fn hint_spans(hints: &str, room: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0;
    for (n, hint) in hints.split(" · ").enumerate() {
        let sep = if n == 0 { "" } else { "  " };
        let len = sep.len() + hint.chars().count();
        if used + len > room {
            break;
        }
        used += len;
        let (key, label) = hint.split_once(' ').unwrap_or((hint, ""));
        out.push(Span::raw(sep));
        out.push(Span::styled(key.to_string(), Style::new().fg(ACCENT)));
        out.push(Span::styled(format!(" {label}"), DIM));
    }
    out
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
            let width = area.width.saturating_sub(5) as usize;
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
        .highlight_style(Style::new().bg(Color::DarkGray))
        .highlight_symbol(Span::styled("▌", Style::new().fg(ACCENT)))
        .highlight_spacing(HighlightSpacing::Always);
    frame.render_stateful_widget(list, area, &mut app.source_state);
}

fn draw_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let show_feed = !matches!(app.source(), Source::Feed(_));
    let folder_color = |item: &Item| {
        let folder = app.ds.folder_of(item);
        app.folders
            .iter()
            .position(|f| Some(f.as_str()) == folder)
            .map_or(Color::Gray, home::folder_color)
    };
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
                    Style::new().fg(folder_color(item)),
                ));
            }
            let title = Span::raw(item.title().to_string());
            spans.push(if unread {
                title.bold()
            } else {
                title.fg(Color::Gray)
            });
            if app.summarizer.done(item).is_some() {
                spans.push(Span::styled(" ✦", Style::new().fg(AI)));
            }
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
        .highlight_style(Style::new().bg(Color::DarkGray))
        .highlight_symbol(Span::styled("▌", Style::new().fg(ACCENT)))
        .highlight_spacing(HighlightSpacing::Always);
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
    let Some(item) = item else {
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new("No article selected").style(DIM),
            indent(inner),
        );
        return;
    };

    // One column of margin and one for the scrollbar; centre the text when the
    // pane is wider than a comfortable line.
    let room = inner.width.saturating_sub(3);
    let width = room.min(MAX_TEXT_WIDTH);
    let text_area = Rect {
        x: inner.x + 1 + (room - width) / 2,
        width,
        ..inner
    };
    if app.reader.as_ref().is_none_or(|r| r.item != item) {
        app.reader = Some(Reader::new(item, app.semantic.as_ref()));
    }
    let reader = app.reader.as_mut().unwrap();
    if reader.width != width {
        let html = app.ds.items[item].body_html().unwrap_or("");
        reader.paragraphs = article::render(html, (width as usize).saturating_sub(GUTTER));
        reader.width = width;
    }

    let reader = app.reader.as_ref().unwrap();
    let it = &app.ds.items[item];
    let w = width as usize;
    let words: usize = reader
        .paragraphs
        .iter()
        .map(|p| p.text.split_whitespace().count())
        .sum();
    let minutes = words.div_ceil(230).max(1);
    let state = current_summary(app, it);
    let cited: Vec<usize> = match state {
        Some(State::Done(s)) => s.points.iter().flat_map(|p| p.cites.clone()).collect(),
        _ => Vec::new(),
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    for l in wrap(it.title(), w) {
        lines.push(Line::from(l).bold());
    }
    let mut source = vec![Span::from(app.ds.feed_name(it.feed_id).to_string()).fg(ACCENT)];
    if !it.authors.is_empty() {
        source.push(Span::raw(format!(" · {}", it.authors.join(", "))));
    }
    if app.ds.starred.contains(&it.id) {
        source.push(Span::from(" ★").fg(STAR));
    }
    let mut when = Vec::new();
    if let Some(p) = it.published_at {
        when.push(p.format("%a %d %b %Y %H:%M").to_string());
    }
    if words > 0 {
        when.push(format!("{minutes} min read"));
    }
    let when = when.join(" · ");
    // Date and reading time move to their own line rather than being cut off.
    let source_width: usize = source.iter().map(Span::width).sum();
    if when.is_empty() {
        lines.push(Line::from(source));
    } else if source_width + 3 + when.chars().count() <= w {
        source.push(Span::styled(format!(" · {when}"), DIM));
        lines.push(Line::from(source));
    } else {
        lines.push(Line::from(source));
        lines.push(Line::styled(when, DIM));
    }
    if let Some(url) = &it.url {
        lines.push(Line::styled(truncate(url, w), DIM));
    }
    if app.show_ai {
        let hint = match state {
            Some(State::Done(_)) => Some("✦ AI summary at the end · G jumps there"),
            Some(State::Writing { .. }) => Some("✦ writing an AI summary · it appears at the end"),
            _ => None,
        };
        if let Some(hint) = hint {
            lines.push(Line::styled(hint, Style::new().fg(AI)));
        }
    }
    lines.push(Line::styled("─".repeat(w), DIM));
    lines.push(Line::raw(""));

    if let Some(hit) = app.hits.get(&item) {
        lines.push(Line::styled(
            format!("┌ search “{}”", app.query),
            Style::new().fg(AI),
        ));
        for reason in &hit.reasons {
            for l in wrap(reason, w.saturating_sub(2)) {
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
                Style::new().fg(WARN),
            ));
        }
        lines.push(Line::styled("└ i: hide", DIM));
        lines.push(Line::raw(""));
    }

    if reader.paragraphs.is_empty() {
        lines.push(Line::styled(
            "The feed provides no text for this article. Press o to open it in a browser.",
            DIM,
        ));
        lines.push(Line::raw(""));
    }
    for (n, p) in reader.paragraphs.iter().enumerate() {
        // Headings get room above, so sections stand apart.
        if p.kind == Kind::Heading && n > 0 {
            lines.push(Line::raw(""));
        }
        for (k, l) in p.lines.iter().enumerate() {
            let gutter = if app.show_ai && k == 0 {
                let style = if cited.contains(&(n + 1)) {
                    Style::new().fg(AI)
                } else {
                    DIM
                };
                Span::styled(format!("¶{:<w$}", n + 1, w = GUTTER - 1), style)
            } else {
                Span::raw(" ".repeat(GUTTER))
            };
            let mut spans = vec![gutter];
            spans.extend(l.spans.iter().cloned());
            lines.push(Line::from(spans));
        }
        lines.push(Line::raw(""));
    }

    // One rule closes the article; the summary and related articles follow.
    let related = related_lines(app, reader, w);
    if app.show_ai || !related.is_empty() {
        lines.push(Line::styled("─".repeat(w), DIM));
        lines.push(Line::raw(""));
    }
    if app.show_ai {
        lines.extend(summary_lines(app, it, &reader.paragraphs, words, w));
    }
    lines.extend(related);

    let reader = app.reader.as_mut().unwrap();
    let max = (lines.len() as u16).saturating_sub(inner.height);
    reader.max = max;
    let scroll = reader.scroll.min(max);
    let percent = if max == 0 {
        100
    } else {
        scroll as usize * 100 / max as usize
    };
    let block =
        block.title(Line::styled(format!(" {percent}% · {minutes} min "), DIM).right_aligned());
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).scroll((scroll, 0)),
        text_area,
    );
    if max > 0 {
        let mut state = ScrollbarState::new(max as usize).position(scroll as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some(" "))
                .thumb_style(DIM),
            inner,
            &mut state,
        );
    }
}

/// The summary state for this version of the article; an older version's
/// summary counts as none.
fn current_summary<'a>(app: &'a App, it: &Item) -> Option<&'a State> {
    match app.summarizer.state(it.id) {
        Some(State::Done(s)) if s.version_id != it.version_id => None,
        state => state,
    }
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The AI summary block shown after the article text.
fn summary_lines(
    app: &App,
    it: &Item,
    paragraphs: &[Para],
    words: usize,
    w: usize,
) -> Vec<Line<'static>> {
    let ai = Style::new().fg(AI);
    let mut out = Vec::new();
    let head = |detail: String| {
        Line::from(vec![
            Span::styled("✦ AI summary", ai.bold()),
            Span::styled(detail, DIM),
        ])
    };
    let input = Input::of(it.content.is_some(), words);
    match current_summary(app, it) {
        _ if input == Input::TitleOnly => out.push(head(
            " · the feed sends only a title, so there is nothing to summarize".into(),
        )),
        _ if words < MIN_WORDS => out.push(head(format!(
            " · not needed: the article is only {words} words"
        ))),
        None => out.push(head(" · open the article (⏎) to write one".into())),
        Some(State::Writing { started, text }) => {
            let secs = started.elapsed().as_secs();
            let frame = SPINNER[(started.elapsed().as_millis() / 100) as usize % SPINNER.len()];
            out.push(head(format!(" · {frame} writing · {secs} s")));
            if text.is_empty() && secs >= 5 {
                out.push(Line::styled(
                    "  the model reads the whole article first; on a CPU that takes a while",
                    DIM,
                ));
            }
            let texts: Vec<String> = paragraphs.iter().map(|p| p.text.clone()).collect();
            for point in summary::parse(text, &texts) {
                out.extend(point_lines(&point, w, false));
            }
        }
        Some(State::Failed(e)) => {
            out.push(head(" · unavailable".into()));
            for l in wrap(e, w.saturating_sub(2)) {
                out.push(Line::styled(format!("  {l}"), Style::new().fg(WARN)));
            }
            out.push(Line::styled("  A tries again", DIM));
        }
        Some(State::Done(s)) => {
            out.push(head(format!(
                " · {:.0} s · from {}, {words} words",
                s.seconds,
                input.label()
            )));
            out.push(Line::raw(""));
            for point in &s.points {
                out.extend(point_lines(point, w, true));
            }
            out.push(Line::raw(""));
            if s.points.iter().any(|p| !p.supported()) {
                out.push(Line::styled(
                    "  ⚠ few of its words appear in the paragraphs it cites: check it",
                    Style::new().fg(WARN),
                ));
            }
            if s.truncated {
                out.push(Line::styled(
                    format!(
                        "  only the first {} of {} paragraphs were read",
                        s.paragraphs,
                        paragraphs.len()
                    ),
                    DIM,
                ));
            }
            out.push(Line::styled(
                "  ¶ numbers point at paragraphs above · A write again · a hide",
                DIM,
            ));
        }
    }
    out.push(Line::raw(""));
    out
}

fn point_lines(point: &Point, w: usize, check: bool) -> Vec<Line<'static>> {
    let cite = if point.cites.is_empty() {
        " [no source]".to_string()
    } else {
        let refs: Vec<String> = point.cites.iter().map(|n| format!("¶{n}")).collect();
        format!(" [{}]", refs.join(", "))
    };
    let flag = if check && !point.supported() {
        " ⚠"
    } else {
        ""
    };
    let room = w.saturating_sub(4 + cite.chars().count() + flag.chars().count());
    let wrapped = wrap(&point.text, room);
    let last = wrapped.len() - 1;
    wrapped
        .into_iter()
        .enumerate()
        .map(|(n, l)| {
            let mut spans = vec![
                Span::styled(if n == 0 { "  • " } else { "    " }, Style::new().fg(AI)),
                Span::raw(l),
            ];
            if n == last {
                spans.push(Span::styled(cite.clone(), Style::new().fg(AI)));
                spans.push(Span::styled(flag, Style::new().fg(WARN)));
            }
            Line::from(spans)
        })
        .collect()
}

/// Articles close in meaning, listed after the summary.
fn related_lines(app: &App, reader: &Reader, w: usize) -> Vec<Line<'static>> {
    if reader.related.is_empty() {
        return Vec::new();
    }
    let mut out = vec![Line::from(vec![
        Span::styled("Related", Style::new().bold()),
        Span::styled(" · close in meaning", DIM),
    ])];
    for (k, &(i, sim)) in reader.related.iter().enumerate() {
        let it = &app.ds.items[i];
        let selected = reader.related_selected == Some(k);
        let lead = format!(
            "{}{sim:.2} {:>3} {:<14} ",
            if selected { "› " } else { "  " },
            short_duration(app.ds.now - it.when()),
            truncate(app.ds.feed_name(it.feed_id), 14)
        );
        let title = truncate(it.title(), w.saturating_sub(lead.chars().count()));
        let line = Line::from(vec![
            Span::styled(
                lead,
                if selected {
                    Style::new().fg(ACCENT)
                } else {
                    DIM
                },
            ),
            Span::raw(title),
        ]);
        out.push(if selected {
            line.style(Style::new().bg(Color::DarkGray))
        } else {
            line
        });
    }
    out.push(Line::styled("  ] [ choose · ⏎ open", DIM));
    out
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
            Entry::Story {
                label,
                items,
                by_meaning,
            } => ListItem::new(Line::from(vec![
                Span::styled(
                    format!(
                        "◆ {} sources {} ",
                        items.len(),
                        if *by_meaning { "≈" } else { " " }
                    ),
                    Style::new().fg(AI),
                ),
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
    let grouping = match entry {
        Entry::Story {
            by_meaning: true, ..
        } => "same story: shared links or title words, or close in meaning",
        Entry::Story { .. } => "same story: shared links or title words across feeds",
        Entry::Folder { .. } => "newest items in this folder, not grouped",
    };
    let mut lines = vec![Line::styled(grouping, DIM)];
    let shown = &entry.items()[..entry.items().len().min(8)];
    // An article's AI summary speaks for it when there is one; otherwise its
    // lead sentence stands in, which is a mock.
    let claims: Vec<(String, bool)> = shown
        .iter()
        .map(|&i| {
            let it = &app.ds.items[i];
            if let Some(point) = app.summarizer.done(it).and_then(|s| s.points.first()) {
                return (point.text.clone(), true);
            }
            let text = html_to_text(it.body_html().unwrap_or(""), 10_000);
            let sentence = mock_ai::lead_sentence(&text, 220);
            // Link-only blurbs (Hacker News, Lobsters) say less than the title.
            let useful = sentence.split_whitespace().count() >= 6 && !sentence.contains("://");
            (
                if useful {
                    sentence
                } else {
                    it.title().to_string()
                },
                false,
            )
        })
        .collect();
    if claims.iter().any(|(_, ai)| !ai) {
        lines.push(Line::from(vec![
            Span::styled("MOCK", Style::new().fg(Color::Black).bg(AI)),
            Span::styled(
                " lead sentences; ✦ marks AI summaries (open an article to write one)",
                DIM,
            ),
        ]));
    }
    lines.push(Line::raw(""));
    for (n, (claim, from_ai)) in claims.iter().enumerate() {
        let cite = format!(" [{}]", n + 1);
        let wrapped = wrap(claim, width.saturating_sub(2 + cite.len()));
        let last = wrapped.len() - 1;
        for (k, l) in wrapped.into_iter().enumerate() {
            let bullet = match (k, from_ai) {
                (0, true) => Span::styled("✦ ", ai),
                (0, false) => Span::raw("• "),
                _ => Span::raw("  "),
            };
            let mut spans = vec![bullet, Span::raw(l)];
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
        ("a", "show / hide AI summary (at the end)"),
        ("A", "write the summary again"),
        ("] [ then ⏎", "choose and open a related article"),
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};

    use super::*;
    use crate::data::Dataset;
    use crate::summary::{Summarizer, Summary};

    const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/data/fake.json");
    const LONG: i64 = 135;

    /// The screen as text, one string per row.
    fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    fn row(rows: &[String], needle: &str) -> usize {
        rows.iter()
            .position(|r| r.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} not on screen:\n{}", rows.join("\n")))
    }

    /// The long article opened full screen, with a finished summary.
    fn reading_long_article() -> App {
        let ds = Dataset::load(Path::new(FAKE)).unwrap();
        let mut summarizer = Summarizer::new("test-model", "http://127.0.0.1:1/api/chat", None);
        let item = ds.items.iter().find(|i| i.id == LONG).unwrap();
        summarizer.insert(
            LONG,
            Summary {
                model: "test-model".into(),
                version_id: item.version_id,
                points: summary::parse(
                    "- A one-line change rebuilds in 3.2 seconds instead of 41 [3]\n- Unrelated claim about bananas [2]",
                    &article::plain(item.body_html().unwrap()),
                ),
                seconds: 12.0,
                paragraphs: 12,
                truncated: false,
            },
        );
        let mut app = App::new(ds, None, summarizer);
        app.on_key(KeyEvent::from(KeyCode::Char('a')));
        app.on_key(KeyEvent::from(KeyCode::Char('/')));
        for c in "incremental".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        app.on_key(KeyEvent::from(KeyCode::Char('z')));
        app
    }

    #[test]
    fn summary_comes_after_the_article_text() {
        let mut app = reading_long_article();
        let rows = screen(&mut app, 110, 120);
        let title = row(&rows, "Example Lang 2.5 brings incremental compilation");
        let hint = row(&rows, "AI summary at the end");
        let last_paragraph = row(&rows, "better error messages for generic code");
        let summary = row(&rows, "✦ AI summary · 12 s");
        assert!(
            !rows.iter().any(|r| r.contains("test-model")),
            "no model name on screen"
        );
        assert!(title < hint && hint < last_paragraph && last_paragraph < summary);
        assert!(rows[summary + 2].contains("A one-line change rebuilds in 3.2 seconds"));
        assert!(rows[summary + 2].contains("[¶3]") && !rows[summary + 2].contains("⚠"));
        let unsupported = row(&rows, "Unrelated claim about bananas");
        assert!(
            rows[unsupported].contains("[¶2] ⚠"),
            "{}",
            rows[unsupported]
        );
        row(&rows, "few of its words appear in the paragraphs it cites");
    }

    #[test]
    fn article_markup_is_rendered_readably() {
        let mut app = reading_long_article();
        let rows = screen(&mut app, 110, 120);
        // Headings lose their "##", list items get bullets, quotes a bar.
        let heading = row(&rows, "Faster rebuilds");
        assert!(!rows[heading].contains('#'));
        row(&rows, "• Pattern matching on slices is now stable.");
        row(&rows, "│ We would rather break a few builds loudly");
        // Zoomed text is centred and kept to a readable width.
        let line = &rows[row(&rows, "Example Lang 2.5 is out today")];
        let start = line.find("Example").unwrap();
        assert!(start > 10, "centred: {line}");
        let text_rows = rows.iter().filter(|r| r.contains("rebuild")).count();
        assert!(text_rows >= 2);
        for r in &rows {
            assert!(r.trim().chars().count() <= 110);
        }
    }

    #[test]
    fn end_of_article_stays_in_view_as_text_arrives_below() {
        let mut app = reading_long_article();
        app.show_ai = false;
        screen(&mut app, 110, 30);
        app.on_key(KeyEvent::from(KeyCode::Char('G')));
        let rows = screen(&mut app, 110, 30);
        row(&rows, "better error messages for generic code");
        // The summary appears below, as when it finishes writing.
        app.show_ai = true;
        let rows = screen(&mut app, 110, 30);
        row(&rows, "¶ numbers point at paragraphs above");
        // Scrolling up leaves the end.
        app.on_key(KeyEvent::from(KeyCode::Char('k')));
        let rows = screen(&mut app, 110, 30);
        assert!(
            !rows.iter().any(|r| r.contains("100%")),
            "no longer at the end"
        );
    }

    #[test]
    fn footer_hints_never_end_mid_word() {
        let spans = hint_spans("j/k move · ⏎ open · n next unread", 18);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "j/k move  ⏎ open");
        assert!(hint_spans("j/k move", 3).is_empty());
    }
}
