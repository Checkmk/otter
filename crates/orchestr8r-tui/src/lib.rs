mod app;
mod help_modal;
mod input;
mod input_field;
mod right_panel;
mod runs_panel;
mod scroll;
mod status_bar;
mod styles;
mod text;
mod ui;

use std::io::stdout;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use anyhow::Context;
use crossterm::{event, execute, terminal};
use orchestr8r_core::types::{DaemonCommand, DaemonEvent};
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::mpsc;

pub fn run(
    mut event_rx: mpsc::Receiver<DaemonEvent>,
    cmd_tx: mpsc::Sender<DaemonCommand>,
    shutdown: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    terminal::enable_raw_mode().context("enable raw mode")?;
    let mut out = stdout();
    execute!(out, terminal::EnterAlternateScreen).context("enter alternate screen")?;

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    let mut app = app::App::new(cmd_tx);

    loop {
        // Drain pending daemon events
        while let Ok(ev) = event_rx.try_recv() {
            app.handle_daemon_event(ev);
        }

        terminal.draw(|f| ui::render(f, &mut app))?;
        app.tick = app.tick.wrapping_add(1);

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
