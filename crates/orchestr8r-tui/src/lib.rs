mod app;
mod input;
mod ui;

use std::io::stdout;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use anyhow::Context;
use crossterm::{event, execute, terminal};
use orchestr8r_core::types::EngineEvent;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::mpsc;

pub fn run(
    mut ui_rx: mpsc::Receiver<EngineEvent>,
    shutdown: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    terminal::enable_raw_mode().context("enable raw mode")?;
    let mut out = stdout();
    execute!(out, terminal::EnterAlternateScreen).context("enter alternate screen")?;

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    let mut app = app::App::new();

    loop {
        // Drain pending engine events
        while let Ok(ev) = ui_rx.try_recv() {
            app.handle_engine_event(ev);
        }

        terminal.draw(|f| ui::render(f, &app))?;

        if event::poll(Duration::from_millis(16))? {
            if let event::Event::Key(key) = event::read()? {
                input::handle_key(&mut app, key);
            }
        }

        if app.should_quit || shutdown.load(Ordering::Relaxed) {
            shutdown.store(true, Ordering::Relaxed);
            break;
        }
    }

    terminal::disable_raw_mode().context("disable raw mode")?;
    execute!(terminal.backend_mut(), terminal::LeaveAlternateScreen)
        .context("leave alternate screen")?;
    terminal.show_cursor().context("show cursor")?;

    Ok(())
}
