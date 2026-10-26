//! Article HTML to styled, wrapped paragraphs for the reader. Summaries cite
//! paragraphs by number, so the split into paragraphs must not depend on the
//! width the text is wrapped to: html2text renders at a width nothing reaches,
//! the paragraphs are split there, and only then is each line wrapped here.

use html2text::render::{RichAnnotation, TaggedLine};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::ui::ACCENT;

const LINK: Color = Color::LightBlue;
const CODE: Color = Color::LightYellow;
/// Wide enough that html2text never wraps a line.
const UNWRAPPED: usize = 100_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Heading,
    Quote,
    List,
    Code,
}

pub struct Para {
    pub kind: Kind,
    /// Wrapped and styled for display.
    pub lines: Vec<Line<'static>>,
    /// Unwrapped plain text, for the summarizer and citation checks.
    pub text: String,
}

/// Paragraphs of `html` wrapped to `width` columns.
pub fn render(html: &str, width: usize) -> Vec<Para> {
    let width = width.max(20);
    let mut paras = unwrapped(html);
    for p in &mut paras {
        p.lines = p
            .lines
            .iter()
            .flat_map(|l| wrap_line(l, width, p.kind))
            .collect();
    }
    paras
}

/// The plain text of each paragraph, as `render` numbers them.
pub fn plain(html: &str) -> Vec<String> {
    unwrapped(html).into_iter().map(|p| p.text).collect()
}

/// Paragraphs with one line per source line: a paragraph, list item, or line of code.
fn unwrapped(html: &str) -> Vec<Para> {
    let lines = match html2text::config::rich()
        .link_footnotes(false)
        .lines_from_read(html.as_bytes(), UNWRAPPED)
    {
        Ok(lines) => lines,
        // Malformed or too deeply nested to fit: fall back to the markup as text.
        Err(_) => return fallback(html),
    };
    let mut paras = Vec::new();
    let mut group: Vec<TaggedLine<Vec<RichAnnotation>>> = Vec::new();
    for line in lines.into_iter().chain([TaggedLine::new()]) {
        let blank = line_text(&line).trim().is_empty();
        match (blank, preformat(&line)) {
            // A blank line inside a code block belongs to the block.
            (true, Some(false)) if !group.is_empty() => group.push(line),
            (true, _) => {
                if !group.is_empty() {
                    paras.push(para(std::mem::take(&mut group)));
                }
            }
            (false, _) => {
                // html2text puts no blank line between a heading or quote and
                // what follows, so a change of role starts a paragraph too.
                if group.last().is_some_and(|g| role(g) != role(&line)) {
                    paras.push(para(std::mem::take(&mut group)));
                }
                group.push(line);
            }
        }
    }
    paras
}

fn fallback(html: &str) -> Vec<Para> {
    html.split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| Para {
            kind: Kind::Text,
            lines: p.lines().map(|l| Line::raw(l.to_string())).collect(),
            text: p.split_whitespace().collect::<Vec<_>>().join(" "),
        })
        .collect()
}

fn line_text(line: &TaggedLine<Vec<RichAnnotation>>) -> String {
    line.tagged_strings().map(|s| s.s.as_str()).collect()
}

/// What a line is part of, as far as splitting paragraphs goes.
fn role(line: &TaggedLine<Vec<RichAnnotation>>) -> Kind {
    let text = line_text(line);
    // A list item can hold a heading: "* ### Q".
    let lead = without_list_marker(text.trim_start());
    if preformat(line).is_some() {
        Kind::Code
    } else if lead.starts_with('#') && lead.trim_start_matches('#').starts_with(' ') {
        Kind::Heading
    } else if lead.starts_with("> ") {
        Kind::Quote
    } else {
        Kind::Text
    }
}

/// `Some(continuation)` when the line is part of a code block.
fn preformat(line: &TaggedLine<Vec<RichAnnotation>>) -> Option<bool> {
    line.tagged_strings()
        .flat_map(|s| &s.tag)
        .find_map(|a| match a {
            RichAnnotation::Preformat(cont) => Some(*cont),
            _ => None,
        })
}

fn para(group: Vec<TaggedLine<Vec<RichAnnotation>>>) -> Para {
    let first = line_text(&group[0]);
    let lead = first.trim_start();
    let kind = match role(&group[0]) {
        Kind::Text if lead.starts_with("* ") || ordered_prefix(lead).is_some() => Kind::List,
        role => role,
    };

    let mut text = Vec::new();
    let mut image_marked = false;
    let lines = group
        .iter()
        .map(|line| {
            let mut spans = Vec::new();
            let mut at_start = true;
            for ts in line.tagged_strings() {
                let mut s = ts.s.clone();
                if at_start {
                    s = decorate_prefix(kind, &s);
                    at_start = s.trim().is_empty();
                }
                let mut style = style_for(&ts.tag);
                if kind == Kind::Heading {
                    style = style.fg(ACCENT).add_modifier(Modifier::BOLD);
                } else if kind == Kind::Quote {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                if let Some(RichAnnotation::Image(_)) = ts.tag.last()
                    && !image_marked
                {
                    s = format!("▣ {}", if s.trim().is_empty() { "image" } else { &s });
                    image_marked = true;
                }
                spans.push(Span::styled(s, style));
            }
            let plain: String = spans.iter().map(|s| s.content.as_ref()).collect();
            text.push(plain.trim().trim_start_matches('│').trim().to_string());
            Line::from(spans)
        })
        .collect();
    Para {
        kind,
        lines,
        text: text
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// Swaps html2text's markdown-like prefixes for typographic ones.
fn decorate_prefix(kind: Kind, s: &str) -> String {
    let indent = s.len() - s.trim_start().len();
    let (pad, rest) = s.split_at(indent);
    match kind {
        Kind::Heading => {
            let bullet = if rest.starts_with("* ") { "• " } else { "" };
            let title = without_list_marker(rest)
                .trim_start_matches('#')
                .trim_start();
            format!("{pad}{bullet}{title}")
        }
        Kind::Quote => format!("{pad}{}", rest.replace("> ", "│ ")),
        Kind::List => match rest.strip_prefix("* ") {
            Some(item) => format!("{pad}• {item}"),
            None => s.to_string(),
        },
        _ => s.to_string(),
    }
}

fn without_list_marker(s: &str) -> &str {
    match (s.strip_prefix("* "), ordered_prefix(s)) {
        (Some(rest), _) => rest,
        (None, Some(n)) => &s[n..],
        (None, None) => s,
    }
}

/// "12. item" → Some(4), the prefix length.
fn ordered_prefix(s: &str) -> Option<usize> {
    let digits = s.chars().take_while(char::is_ascii_digit).count();
    (digits > 0 && s[digits..].starts_with(". ")).then_some(digits + 2)
}

/// Wraps one source line to `width` columns. Continuation lines hang under the
/// text, past any indent, bullet or quote bar. Code is cut, not wrapped at words.
fn wrap_line(line: &Line<'static>, width: usize, kind: Kind) -> Vec<Line<'static>> {
    if line.width() <= width {
        return vec![line.clone()];
    }
    // Characters with their styles, so the line can be cut anywhere.
    let chars: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
        .collect();
    let text: String = chars.iter().map(|(c, _)| c).collect();
    let prefix_len = hanging_prefix(&text);
    let (prefix, body) = chars.split_at(prefix_len);
    // The bar of a quote repeats; bullets and numbers turn into spaces.
    let continuation: Vec<(char, Style)> = prefix
        .iter()
        .map(|&(c, st)| (if c == '│' || c == ' ' { c } else { ' ' }, st))
        .collect();
    let room = width.saturating_sub(prefix.len()).max(10);

    let mut rows: Vec<Vec<(char, Style)>> = Vec::new();
    let mut row: Vec<(char, Style)> = Vec::new();
    if kind == Kind::Code {
        for chunk in body.chunks(room) {
            rows.push(chunk.to_vec());
        }
    } else {
        // Words with the spaces before them; a word longer than a row is cut.
        let mut i = 0;
        while i < body.len() {
            let start = i;
            while i < body.len() && body[i].0.is_whitespace() {
                i += 1;
            }
            let word_start = i;
            while i < body.len() && !body[i].0.is_whitespace() {
                i += 1;
            }
            let (space, word) = (&body[start..word_start], &body[word_start..i]);
            if !row.is_empty() && row.len() + space.len() + word.len() > room {
                rows.push(std::mem::take(&mut row));
            } else if !row.is_empty() {
                row.extend_from_slice(space);
            }
            for &ch in word {
                if row.len() == room {
                    rows.push(std::mem::take(&mut row));
                }
                row.push(ch);
            }
        }
        if !row.is_empty() {
            rows.push(row);
        }
    }
    rows.into_iter()
        .enumerate()
        .map(|(n, row)| {
            let lead = if n == 0 { prefix } else { &continuation[..] };
            spans_of(lead.iter().chain(&row))
        })
        .collect()
}

/// Length in characters of the indent, bullets, numbers and quote bars that start `text`.
fn hanging_prefix(text: &str) -> usize {
    let mut rest = text;
    loop {
        let trimmed = rest.trim_start_matches(' ');
        let next = ["• ", "│ "]
            .iter()
            .find_map(|m| trimmed.strip_prefix(m))
            .or_else(|| ordered_prefix(trimmed).map(|n| &trimmed[n..]));
        match next {
            Some(after) => rest = after,
            None => {
                rest = trimmed;
                break;
            }
        }
    }
    text[..text.len() - rest.len()].chars().count()
}

/// Joins runs of characters with the same style back into spans.
fn spans_of<'a>(chars: impl Iterator<Item = &'a (char, Style)>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut current: Option<(String, Style)> = None;
    for &(c, style) in chars {
        match &mut current {
            Some((s, st)) if *st == style => s.push(c),
            _ => {
                if let Some((s, st)) = current.take() {
                    spans.push(Span::styled(s, st));
                }
                current = Some((c.to_string(), style));
            }
        }
    }
    if let Some((s, st)) = current {
        spans.push(Span::styled(s, st));
    }
    Line::from(spans)
}

fn style_for(tags: &[RichAnnotation]) -> Style {
    let mut style = Style::new();
    for tag in tags {
        style = match tag {
            RichAnnotation::Link(_) => style.fg(LINK).add_modifier(Modifier::UNDERLINED),
            RichAnnotation::Image(_) => style.fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
            RichAnnotation::Emphasis => style.add_modifier(Modifier::ITALIC),
            RichAnnotation::Strong => style.add_modifier(Modifier::BOLD),
            RichAnnotation::Strikeout => style.add_modifier(Modifier::CROSSED_OUT),
            RichAnnotation::Code | RichAnnotation::Preformat(_) => style.fg(CODE),
            // Default, and publisher colours, which rarely suit a terminal theme.
            _ => style,
        };
    }
    style
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::data::Dataset;

    fn texts(html: &str) -> Vec<(Kind, String)> {
        render(html, 60)
            .into_iter()
            .map(|p| (p.kind, p.text))
            .collect()
    }

    #[test]
    fn splits_paragraphs_and_detects_their_kind() {
        let html = "<h2>Big news</h2><p>First <b>bold</b> para.</p>\
            <blockquote>A quote.</blockquote><ul><li>one</li><li>two</li></ul>\
            <pre>let x = 1;</pre><p>Last.</p>\
            <h3>A heading long enough to wrap onto a second line</h3><ul><li>three</li></ul>";
        assert_eq!(
            texts(html),
            vec![
                (Kind::Heading, "Big news".into()),
                (Kind::Text, "First bold para.".into()),
                (Kind::Quote, "A quote.".into()),
                (Kind::List, "• one • two".into()),
                (Kind::Code, "let x = 1;".into()),
                (Kind::Text, "Last.".into()),
                (
                    Kind::Heading,
                    "A heading long enough to wrap onto a second line".into()
                ),
                (Kind::List, "• three".into()),
            ]
        );
        let wrapped = &render(html, 30)[6];
        assert!(wrapped.lines.len() >= 2);
        assert!(wrapped.lines.iter().all(|l| !l.to_string().contains('#')));
    }

    #[test]
    fn styles_inline_markup() {
        let paras = render(
            r#"<p>See <a href="https://x.test">the docs</a>, <em>now</em>, with <code>cargo</code>.</p>"#,
            60,
        );
        let spans = &paras[0].lines[0].spans;
        let style = |text: &str| spans.iter().find(|s| s.content == text).unwrap().style;
        assert!(
            style("the docs")
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        assert!(style("now").add_modifier.contains(Modifier::ITALIC));
        assert_eq!(style("cargo").fg, Some(CODE));
        assert_eq!(paras[0].text, "See the docs, now, with cargo.");
    }

    #[test]
    fn wraps_to_the_width() {
        let long = format!("<p>{}</p>", "word ".repeat(100));
        let paras = render(&long, 40);
        assert!(paras[0].lines.len() > 10);
        assert!(paras[0].lines.iter().all(|l| l.width() <= 40));
    }

    #[test]
    fn continuation_lines_hang_under_the_text() {
        let item = format!("<ul><li>{}</li></ul>", "word ".repeat(20));
        let lines = &render(&item, 30)[0].lines;
        assert!(lines[0].to_string().starts_with("• word"));
        assert!(
            lines[1].to_string().starts_with("  word"),
            "{:?}",
            lines[1].to_string()
        );
        let quote = format!("<blockquote>{}</blockquote>", "word ".repeat(20));
        let lines = &render(&quote, 30)[0].lines;
        assert!(lines.iter().all(|l| l.to_string().starts_with("│ ")));
        let code = format!("<pre>{}</pre>", "x".repeat(70));
        let lines = &render(&code, 30)[0].lines;
        assert_eq!(lines.len(), 3, "code is cut at the width, not at words");
        let long_word = format!("<p>{}</p>", "y".repeat(70));
        assert!(
            render(&long_word, 30)[0]
                .lines
                .iter()
                .all(|l| l.width() <= 30)
        );
    }

    #[test]
    fn wrapping_keeps_styles() {
        let html = format!("<p>{} <b>bold words here</b> end</p>", "plain ".repeat(5));
        let lines = &render(&html, 34)[0].lines;
        let bold: String = lines
            .iter()
            .flat_map(|l| &l.spans)
            .filter(|s| s.style.add_modifier.contains(Modifier::BOLD))
            .map(|s| s.content.to_string())
            .collect::<Vec<_>>()
            .join("|");
        // The bold run may be split across rows; joined again it is all there.
        assert_eq!(
            bold.replace('|', " ")
                .split_whitespace()
                .collect::<Vec<_>>(),
            ["bold", "words", "here"],
            "{bold}"
        );
        assert_eq!(
            lines
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>()
                .join(" "),
            format!("{}bold words here end", "plain ".repeat(5))
        );
    }

    #[test]
    fn empty_html_has_no_paragraphs() {
        assert!(render("", 60).is_empty());
        assert!(render("<p>  </p><div></div>", 60).is_empty());
    }

    /// Citations would point at the wrong paragraph if wrapping changed the split.
    fn assert_numbering_ignores_width(path: &Path) {
        let ds = Dataset::load(path).unwrap();
        for item in &ds.items {
            let Some(html) = item.body_html() else {
                continue;
            };
            let wide = plain(html);
            for width in [20, 30, 56, 76, 96] {
                let narrow: Vec<String> = render(html, width).into_iter().map(|p| p.text).collect();
                assert_eq!(
                    narrow.len(),
                    wide.len(),
                    "item {} at width {width}",
                    item.id
                );
            }
        }
    }

    #[test]
    fn numbering_ignores_width_on_fake_data() {
        assert_numbering_ignores_width(Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/data/fake.json"
        )));
    }

    #[test]
    fn numbering_ignores_width_on_trial_data() {
        let path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/data/trial.json"));
        if !path.exists() {
            eprintln!("SKIP: no data/trial.json");
            return;
        }
        assert_numbering_ignores_width(path);
    }
}
