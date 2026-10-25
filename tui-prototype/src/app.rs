use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::{ListState, TableState};

use crate::data::Dataset;
use crate::mock_ai::{self, Entry};
use crate::search::{self, Hit, Strength};
use crate::semantic::{self, Semantic};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// Start page: logo and one tile per folder.
    Home,
    /// Articles of one folder, or of everything when `App::scope` is `None`.
    Inbox,
    Briefing,
    Feeds,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    List,
    Reader,
}

#[derive(Clone, PartialEq, Eq)]
pub enum Source {
    All,
    Starred,
    Folder(String),
    Feed(i64),
}

/// Briefing look-back windows that `w` cycles through.
pub const WINDOWS: &[(i64, &str)] = &[(6, "6 h"), (12, "12 h"), (24, "24 h"), (24 * 7, "7 d")];

pub struct Reader {
    pub item: usize,
    pub scroll: u16,
    /// Rendered paragraphs, cached with the width they were wrapped to.
    pub paragraphs: Vec<String>,
    pub width: u16,
}

pub struct App {
    pub ds: Dataset,
    pub view: View,
    /// Folder names in display order, one Home tile each.
    pub folders: Vec<String>,
    pub home_selected: usize,
    /// Tile columns in the last drawn Home grid, for up/down movement.
    pub home_columns: usize,
    /// The folder the inbox is showing, or `None` for all articles.
    pub scope: Option<String>,
    pub focus: Focus,
    pub quit: bool,
    pub help: bool,
    pub status: String,

    pub sources: Vec<Source>,
    pub source_state: ListState,
    /// Indices into `ds.items` shown in the article list.
    pub visible: Vec<usize>,
    pub list_state: ListState,
    pub unread_only: bool,
    pub query: String,
    pub searching: bool,
    /// Built the first time a search runs.
    pub index: Option<search::Index>,
    /// Why each shown article matched the current query, keyed by item index.
    pub hits: HashMap<usize, Hit>,
    /// `None` when the fixture has no embeddings file.
    pub semantic: Option<Semantic>,

    pub reader: Option<Reader>,
    pub show_ai: bool,
    pub show_internals: bool,
    pub zoom: bool,

    pub feeds_state: TableState,

    pub checkpoint: DateTime<Utc>,
    pub window: usize,
    pub brief: Vec<Entry>,
    pub brief_state: ListState,
    pub brief_sources: bool,
    pub brief_source_state: ListState,
}

impl App {
    pub fn new(ds: Dataset, semantic: Option<Semantic>) -> Self {
        let window = 1;
        let checkpoint = ds.now - Duration::hours(WINDOWS[window].0);
        let mut app = Self {
            status: format!("{} data · press ? for keys", ds.source),
            folders: ds.folders(),
            ds,
            view: View::Home,
            home_selected: 0,
            home_columns: 1,
            scope: None,
            focus: Focus::List,
            quit: false,
            help: false,
            sources: Vec::new(),
            source_state: ListState::default().with_selected(Some(0)),
            visible: Vec::new(),
            list_state: ListState::default(),
            unread_only: false,
            query: String::new(),
            searching: false,
            index: None,
            hits: HashMap::new(),
            semantic,
            reader: None,
            show_ai: true,
            show_internals: false,
            zoom: false,
            feeds_state: TableState::default().with_selected(Some(0)),
            checkpoint,
            window,
            brief: Vec::new(),
            brief_state: ListState::default().with_selected(Some(0)),
            brief_sources: false,
            brief_source_state: ListState::default().with_selected(Some(0)),
        };
        app.set_scope(None);
        app.rebuild_brief();
        app
    }

    /// Points the inbox at one folder (its feeds only) or at everything.
    pub fn set_scope(&mut self, scope: Option<String>) {
        self.sources = match &scope {
            Some(folder) => vec![Source::Folder(folder.clone())],
            None => vec![Source::All, Source::Starred],
        };
        for folder in &self.folders {
            if scope.as_ref().is_some_and(|s| s != folder) {
                continue;
            }
            if scope.is_none() {
                self.sources.push(Source::Folder(folder.clone()));
            }
            self.sources.extend(
                self.ds
                    .feeds
                    .iter()
                    .filter(|f| f.folder.as_ref() == Some(folder))
                    .map(|f| Source::Feed(f.id)),
            );
        }
        self.scope = scope;
        self.source_state.select(Some(0));
        self.list_state.select(None);
        self.reader = None;
        self.zoom = false;
        self.focus = Focus::List;
        self.refilter();
    }

    fn open_folder(&mut self, index: usize) {
        if let Some(folder) = self.folders.get(index).cloned() {
            self.home_selected = index;
            self.query.clear();
            self.set_scope(Some(folder));
            self.view = View::Inbox;
        }
    }

    /// Opens the all-articles inbox with the given source selected.
    fn open_all(&mut self, source: Source) {
        self.query.clear();
        self.set_scope(None);
        if let Some(pos) = self.sources.iter().position(|s| *s == source) {
            self.source_state.select(Some(pos));
            self.refilter();
        }
        self.view = View::Inbox;
    }

    pub fn source(&self) -> &Source {
        &self.sources[self.source_state.selected().unwrap_or(0)]
    }

    pub fn matches(&self, source: &Source, item: usize) -> bool {
        let item = &self.ds.items[item];
        match source {
            Source::All => true,
            Source::Starred => self.ds.starred.contains(&item.id),
            Source::Folder(name) => self.ds.folder_of(item) == Some(name.as_str()),
            Source::Feed(id) => item.feed_id == *id,
        }
    }

    pub fn unread_in(&self, source: &Source) -> usize {
        (0..self.ds.items.len())
            .filter(|&i| !self.ds.read.contains(&self.ds.items[i].id) && self.matches(source, i))
            .count()
    }

    /// Recomputes the article list, keeping the selected article if it is still shown.
    pub fn refilter(&mut self) {
        let keep = self.selected_item();
        let source = self.source().clone();
        let query = search::Query::parse(&self.query);
        if !query.is_empty() && self.index.is_none() {
            self.index = Some(search::Index::build(&self.ds));
        }
        self.hits.clear();
        let scores = match self.semantic.as_mut().filter(|_| !query.is_empty()) {
            Some(sem) => sem.scores(&self.query).unwrap_or_else(|e| {
                self.status = format!("keyword search only: {e}");
                None
            }),
            None => None,
        };
        // Newest first, as in the unfiltered list; ranking by importance comes later.
        let mut visible = Vec::new();
        for i in 0..self.ds.items.len() {
            if !self.matches(&source, i)
                || (self.unread_only && self.ds.read.contains(&self.ds.items[i].id))
            {
                continue;
            }
            if let Some(index) = self.index.as_ref().filter(|_| !query.is_empty()) {
                let similar = scores
                    .as_ref()
                    .and_then(|s| s[i].as_ref())
                    .filter(|s| s.sigma >= semantic::MIN_SIGMA)
                    .map(|s| {
                        format!(
                            "meaning: close to “{}” (similarity {:.2}, {:.1}σ above average)",
                            self.query.trim(),
                            s.similarity,
                            s.sigma
                        )
                    });
                let hit = match (index.search(i, &query), similar) {
                    (Some(mut hit), Some(reason)) => {
                        hit.reasons.push(reason);
                        hit
                    }
                    (Some(hit), None) => hit,
                    (None, Some(reason)) => Hit {
                        strength: Strength::Similar,
                        reasons: vec![reason],
                    },
                    (None, None) => continue,
                };
                self.hits.insert(i, hit);
            }
            visible.push(i);
        }
        self.visible = visible;
        let pos = keep.and_then(|k| self.visible.iter().position(|&i| i == k));
        self.list_state.select(if self.visible.is_empty() {
            None
        } else {
            Some(pos.unwrap_or(0))
        });
    }

    /// Searches again when a semantic result for the current query has arrived.
    pub fn tick(&mut self) {
        if self.semantic.as_mut().is_some_and(Semantic::poll) {
            self.refilter();
        }
    }

    pub fn selected_item(&self) -> Option<usize> {
        self.list_state
            .selected()
            .and_then(|i| self.visible.get(i).copied())
    }

    pub fn rebuild_brief(&mut self) {
        self.brief = mock_ai::briefing(&self.ds, self.checkpoint);
        self.brief_state
            .select(if self.brief.is_empty() { None } else { Some(0) });
        self.brief_source_state.select(Some(0));
        self.brief_sources = false;
    }

    fn open(&mut self, item: usize) {
        self.ds.read.insert(self.ds.items[item].id);
        self.reader = Some(Reader {
            item,
            scroll: 0,
            paragraphs: Vec::new(),
            width: 0,
        });
    }

    /// Shows an article in the inbox, switching to "All" if the current filter hides it.
    fn jump_to(&mut self, item: usize) {
        self.view = View::Inbox;
        if !self.visible.contains(&item) {
            self.unread_only = false;
            self.query.clear();
            self.set_scope(None);
        }
        let pos = self.visible.iter().position(|&i| i == item);
        self.list_state.select(pos);
        self.open(item);
        self.focus = Focus::Reader;
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if self.searching {
            match key.code {
                KeyCode::Esc => {
                    self.searching = false;
                    self.query.clear();
                }
                KeyCode::Enter => self.searching = false,
                KeyCode::Backspace => {
                    self.query.pop();
                }
                KeyCode::Char(c) => self.query.push(c),
                _ => return,
            }
            self.refilter();
            return;
        }
        if self.help {
            self.help = false;
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }

        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('1') => self.view = View::Home,
            KeyCode::Char('2') => self.view = View::Briefing,
            KeyCode::Char('3') => self.view = View::Feeds,
            KeyCode::Tab => {
                self.view = match self.view {
                    View::Home | View::Inbox => View::Briefing,
                    View::Briefing => View::Feeds,
                    View::Feeds => View::Home,
                }
            }
            // Backspace always steps back to Home; Esc does too once nothing else is open.
            KeyCode::Backspace => self.view = View::Home,
            KeyCode::Esc
                if self.view == View::Feeds
                    || (self.view == View::Inbox && self.focus != Focus::Reader)
                    || (self.view == View::Briefing && !self.brief_sources) =>
            {
                self.view = View::Home
            }
            _ => match self.view {
                View::Home => self.home_key(key.code),
                View::Inbox => self.inbox_key(key.code),
                View::Briefing => self.briefing_key(key.code),
                View::Feeds => self.feeds_key(key.code),
            },
        }
    }

    fn home_key(&mut self, code: KeyCode) {
        let (n, cols, sel) = (
            self.folders.len(),
            self.home_columns.max(1),
            self.home_selected,
        );
        if n == 0 {
            return;
        }
        self.home_selected = match code {
            KeyCode::Enter => return self.open_folder(sel),
            KeyCode::Char('a') => return self.open_all(Source::All),
            KeyCode::Char('s') => return self.open_all(Source::Starred),
            KeyCode::Char('/') => {
                self.open_all(Source::All);
                self.searching = true;
                return;
            }
            KeyCode::Char('h') | KeyCode::Left if sel % cols > 0 => sel - 1,
            KeyCode::Char('l') | KeyCode::Right if sel % cols + 1 < cols => (sel + 1).min(n - 1),
            KeyCode::Char('j') | KeyCode::Down if sel / cols < (n - 1) / cols => {
                (sel + cols).min(n - 1)
            }
            KeyCode::Char('k') | KeyCode::Up if sel >= cols => sel - cols,
            KeyCode::Char('g') | KeyCode::Home => 0,
            KeyCode::Char('G') | KeyCode::End => n - 1,
            _ => sel,
        };
    }

    fn inbox_key(&mut self, code: KeyCode) {
        match (self.focus, code) {
            (_, KeyCode::Char('/')) => {
                self.searching = true;
                self.focus = Focus::List;
            }
            (_, KeyCode::Char('u')) => {
                self.unread_only = !self.unread_only;
                self.refilter();
            }
            (_, KeyCode::Char('z')) => self.zoom = !self.zoom,
            (_, KeyCode::Char('a')) => self.show_ai = !self.show_ai,
            (_, KeyCode::Char('i')) => self.show_internals = !self.show_internals,
            (_, KeyCode::Char('n')) => self.next_unread(),
            (_, KeyCode::Char('s')) => self.toggle_star(),
            (_, KeyCode::Char('m')) => self.toggle_read(),
            (_, KeyCode::Char('M')) => self.mark_source_read(),
            (_, KeyCode::Char('o')) => self.open_in_browser(),
            (_, KeyCode::Char('r')) => self.mock_refresh(),
            (Focus::Sidebar, KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter) => {
                self.focus = Focus::List
            }
            (Focus::Sidebar, c) => {
                if move_selection(&mut self.source_state, self.sources.len(), c) {
                    self.refilter();
                }
            }
            (Focus::List, KeyCode::Char('h') | KeyCode::Left) => self.focus = Focus::Sidebar,
            (Focus::List, KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter) => {
                if let Some(item) = self.selected_item() {
                    self.open(item);
                    self.focus = Focus::Reader;
                }
            }
            (Focus::List, c) => {
                move_selection(&mut self.list_state, self.visible.len(), c);
            }
            (Focus::Reader, KeyCode::Char('h') | KeyCode::Left | KeyCode::Esc) => {
                self.focus = Focus::List;
                self.zoom = false;
            }
            (Focus::Reader, c) => {
                if let Some(reader) = &mut self.reader {
                    reader.scroll = match c {
                        KeyCode::Char('j') | KeyCode::Down => reader.scroll.saturating_add(1),
                        KeyCode::Char('k') | KeyCode::Up => reader.scroll.saturating_sub(1),
                        KeyCode::Char(' ') | KeyCode::PageDown => reader.scroll.saturating_add(20),
                        KeyCode::PageUp => reader.scroll.saturating_sub(20),
                        KeyCode::Char('g') | KeyCode::Home => 0,
                        KeyCode::Char('G') | KeyCode::End => u16::MAX,
                        _ => reader.scroll,
                    };
                }
            }
        }
    }

    fn briefing_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('w') => {
                self.window = (self.window + 1) % WINDOWS.len();
                self.checkpoint = self.ds.now - Duration::hours(WINDOWS[self.window].0);
                self.rebuild_brief();
            }
            KeyCode::Char('c') => {
                self.checkpoint = self.ds.now;
                self.rebuild_brief();
                self.status =
                    "caught up: briefing checkpoint moved to now (w resets the window)".into();
            }
            KeyCode::Char('l') | KeyCode::Right if !self.brief_sources => {
                self.brief_sources = true;
                self.brief_source_state.select(Some(0));
            }
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Esc => self.brief_sources = false,
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                let Some(entry) = self.brief_state.selected().and_then(|i| self.brief.get(i))
                else {
                    return;
                };
                let pick = if self.brief_sources {
                    self.brief_source_state.selected().unwrap_or(0)
                } else {
                    0
                };
                if let Some(&item) = entry.items().get(pick) {
                    self.jump_to(item);
                }
            }
            c if self.brief_sources => {
                let n = self
                    .brief_state
                    .selected()
                    .and_then(|i| self.brief.get(i))
                    .map_or(0, |e| e.items().len());
                move_selection(&mut self.brief_source_state, n, c);
            }
            c => {
                if move_selection(&mut self.brief_state, self.brief.len(), c) {
                    self.brief_source_state.select(Some(0));
                }
            }
        }
    }

    fn feeds_key(&mut self, code: KeyCode) {
        let Some(sel) = self.feeds_state.selected() else {
            return;
        };
        match code {
            KeyCode::Enter => {
                let feed = &self.ds.feeds[sel];
                let (id, folder) = (feed.id, feed.folder.clone());
                self.set_scope(folder);
                if let Some(pos) = self.sources.iter().position(|s| *s == Source::Feed(id)) {
                    self.source_state.select(Some(pos));
                    self.refilter();
                }
                self.view = View::Inbox;
            }
            KeyCode::Char('r') => self.mock_refresh(),
            c => {
                let mut state = ListState::default().with_selected(Some(sel));
                move_selection(&mut state, self.ds.feeds.len(), c);
                self.feeds_state.select(state.selected());
            }
        }
    }

    fn next_unread(&mut self) {
        let start = self.list_state.selected().map_or(0, |s| s + 1);
        let found = (start..self.visible.len())
            .find(|&p| !self.ds.read.contains(&self.ds.items[self.visible[p]].id));
        match found {
            Some(pos) => {
                self.list_state.select(Some(pos));
                self.open(self.visible[pos]);
            }
            None => self.status = "no more unread articles below".into(),
        }
    }

    /// The article actions apply to: the open one in the reader, else the selected one.
    fn target(&self) -> Option<usize> {
        match (self.focus, &self.reader) {
            (Focus::Reader, Some(r)) => Some(r.item),
            _ => self.selected_item(),
        }
    }

    fn toggle_star(&mut self) {
        if let Some(i) = self.target() {
            let id = self.ds.items[i].id;
            if !self.ds.starred.remove(&id) {
                self.ds.starred.insert(id);
            }
        }
    }

    fn toggle_read(&mut self) {
        if let Some(i) = self.target() {
            let id = self.ds.items[i].id;
            if !self.ds.read.remove(&id) {
                self.ds.read.insert(id);
            }
        }
    }

    fn mark_source_read(&mut self) {
        let ids: Vec<i64> = self.visible.iter().map(|&i| self.ds.items[i].id).collect();
        let n = ids.iter().filter(|id| self.ds.read.insert(**id)).count();
        self.status = format!("marked {n} articles read");
    }

    fn open_in_browser(&mut self) {
        let Some(url) = self.target().and_then(|i| self.ds.items[i].url.clone()) else {
            return;
        };
        let spawned = std::process::Command::new("xdg-open")
            .arg(&url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        self.status = match spawned {
            Ok(_) => format!("opened {url}"),
            Err(e) => format!("xdg-open failed: {e}"),
        };
    }

    fn mock_refresh(&mut self) {
        let feed = match self.view {
            View::Feeds => self.feeds_state.selected().map(|i| &self.ds.feeds[i]),
            _ => match self.source() {
                Source::Feed(id) => self.ds.feed(*id),
                _ => self
                    .target()
                    .and_then(|i| self.ds.feed(self.ds.items[i].feed_id)),
            },
        };
        self.status = match feed {
            Some(f) => format!("mock: would POST /v1/feeds/{}/refresh ({})", f.id, f.name()),
            None => "mock: nothing to refresh".into(),
        };
    }
}

/// Moves a list selection for j/k/g/G/arrows/page keys. Returns true if it changed.
fn move_selection(state: &mut ListState, len: usize, code: KeyCode) -> bool {
    if len == 0 {
        return false;
    }
    let cur = state.selected().unwrap_or(0).min(len - 1);
    let next = match code {
        KeyCode::Char('j') | KeyCode::Down => (cur + 1).min(len - 1),
        KeyCode::Char('k') | KeyCode::Up => cur.saturating_sub(1),
        KeyCode::PageDown | KeyCode::Char(' ') => (cur + 15).min(len - 1),
        KeyCode::PageUp => cur.saturating_sub(15),
        KeyCode::Char('g') | KeyCode::Home => 0,
        KeyCode::Char('G') | KeyCode::End => len - 1,
        _ => return false,
    };
    state.select(Some(next));
    next != cur
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ratatui::crossterm::event::KeyEvent;

    use super::*;
    use crate::semantic::{UNREACHABLE, fake_ollama};

    const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/data/fake.json");
    /// "Deep-sea survey finds dozens of new species"
    const SPECIES: i64 = 109;
    /// "Major cloud provider outage disrupts services across Europe"
    const OUTAGE: i64 = 102;

    fn dataset() -> Dataset {
        Dataset::load(Path::new(FAKE)).unwrap()
    }

    /// Semantic search where only `target` points the same way as the query
    /// vector the fake Ollama returns.
    fn app_with_semantic(target: i64, api: &str) -> App {
        let ds = dataset();
        let vectors = ds
            .items
            .iter()
            .map(|i| {
                Some(if i.id == target {
                    vec![1.0, 0.0]
                } else {
                    vec![0.0, 1.0]
                })
            })
            .collect();
        let sem = Semantic::new("bge-m3", "", vectors, api).unwrap();
        App::new(ds, Some(sem))
    }

    /// Waits for a pending semantic result and applies it, as the main loop would.
    fn settle(app: &mut App) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while app.semantic.as_ref().is_some_and(|s| s.pending()) {
            assert!(
                std::time::Instant::now() < deadline,
                "no answer from Ollama"
            );
            app.tick();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    fn search(app: &mut App, query: &str) -> Vec<i64> {
        app.query = query.into();
        app.refilter();
        settle(app);
        app.visible.iter().map(|&i| app.ds.items[i].id).collect()
    }

    fn hit(app: &App, id: i64) -> &Hit {
        let i = app.ds.items.iter().position(|i| i.id == id).unwrap();
        &app.hits[&i]
    }

    #[test]
    fn keyword_search_without_semantic() {
        let mut app = App::new(dataset(), None);
        let found = search(&mut app, "outage");
        assert!(found.contains(&OUTAGE) && found.contains(&103) && found.contains(&111));
        assert!(app.hits.values().all(|h| h.strength != Strength::Similar));
        assert!(search(&mut app, "").len() == app.ds.items.len());
        assert!(app.hits.is_empty());
    }

    #[test]
    fn meaning_adds_articles_without_shared_words() {
        let (api, server) = fake_ollama(200, r#"{"embeddings":[[1.0,0.0]]}"#);
        let mut app = app_with_semantic(SPECIES, &api);
        assert_eq!(search(&mut app, "qqqq"), vec![SPECIES]);
        server.join().unwrap();
        let hit = hit(&app, SPECIES);
        assert_eq!(hit.strength, Strength::Similar);
        assert!(
            hit.reasons[0].starts_with("meaning: close to “qqqq”"),
            "{:?}",
            hit.reasons
        );
    }

    #[test]
    fn keyword_and_meaning_matches_combine() {
        let (api, server) = fake_ollama(200, r#"{"embeddings":[[1.0,0.0]]}"#);
        let mut app = app_with_semantic(OUTAGE, &api);
        let keyword_only = search(&mut App::new(dataset(), None), "outage");
        assert_eq!(
            search(&mut app, "outage"),
            keyword_only,
            "newest-first order kept"
        );
        server.join().unwrap();
        let hit = hit(&app, OUTAGE);
        assert_eq!(hit.strength, Strength::Title);
        assert_eq!(hit.reasons.len(), 2, "{:?}", hit.reasons);
        assert!(hit.reasons[1].starts_with("meaning:"));
    }

    #[test]
    fn ollama_down_falls_back_to_keywords() {
        let mut app = app_with_semantic(SPECIES, UNREACHABLE);
        let keyword_only = search(&mut App::new(dataset(), None), "outage");
        assert_eq!(search(&mut app, "outage"), keyword_only);
        assert!(
            app.status.starts_with("keyword search only: Ollama:"),
            "{}",
            app.status
        );
    }

    #[test]
    fn typing_a_query_key_by_key() {
        let (api, server) = fake_ollama(200, r#"{"embeddings":[[1.0,0.0]]}"#);
        let mut app = app_with_semantic(SPECIES, &api);
        let press = |app: &mut App, code| app.on_key(KeyEvent::from(code));
        press(&mut app, KeyCode::Char('/'));
        assert!(app.searching && app.view == View::Inbox);
        // "qq" is too short for semantic search, "qqq" asks Ollama once.
        for _ in 0..3 {
            press(&mut app, KeyCode::Char('q'));
        }
        assert!(app.visible.is_empty(), "keyword results show at once");
        server.join().unwrap();
        settle(&mut app);
        assert_eq!(app.visible.len(), 1, "meaning match joins when it arrives");
        press(&mut app, KeyCode::Backspace);
        assert!(app.visible.is_empty(), "two letters: keyword matching only");
        press(&mut app, KeyCode::Char('q'));
        assert_eq!(
            app.visible.len(),
            1,
            "back to \"qqq\": served from the cache"
        );
        press(&mut app, KeyCode::Esc);
        assert!(!app.searching && app.query.is_empty());
        assert_eq!(app.visible.len(), app.ds.items.len());
    }
}
