//! Design prototype for the Aleth TUI. Runs entirely on fixture data and never
//! talks to the backend; see README.md.

mod app;
mod data;
mod home;
mod mock_ai;
mod search;
mod semantic;
mod ui;

use std::path::PathBuf;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyEventKind};

use app::App;
use data::Dataset;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data");
    let path = match std::env::args().nth(1).as_deref() {
        Some("--fake") => data_dir.join("fake.json"),
        Some(path) => PathBuf::from(path),
        // Prefer the trial snapshot when one has been taken.
        None if data_dir.join("trial.json").exists() => data_dir.join("trial.json"),
        None => data_dir.join("fake.json"),
    };
    let ds = Dataset::load(&path)?;
    let semantic = semantic::Semantic::load(&ds, &path)?;
    let note = match &semantic {
        Some(sem) => format!(
            "semantic search: {} ({}/{} articles embedded)",
            sem.model,
            sem.embedded,
            ds.items.len()
        ),
        None => "keyword search only (no embeddings; see README)".to_string(),
    };
    let mut app = App::new(ds, semantic);
    app.status = format!("{} · {note}", app.status);

    let mut terminal = ratatui::init();
    let result = (|| -> std::io::Result<()> {
        while !app.quit {
            app.tick();
            terminal.draw(|frame| ui::draw(frame, &mut app))?;
            if event::poll(Duration::from_millis(50))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                app.on_key(key);
            }
        }
        Ok(())
    })();
    ratatui::restore();
    Ok(result?)
}
