//! Interactive terminal UI.

mod app;
pub mod canvas;
pub mod chart;
pub mod form;
pub mod logview;
pub mod module;
pub mod picker;
pub mod theme;
pub mod widgets;

use std::io::stdout;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use anyhow::{Result, anyhow};
use ratatui::crossterm::event::{self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture};
use ratatui::crossterm::execute;

use crate::config::Loaded;
use module::AppEvent;

/// Opens the UI in `start_env`, or in the current environment (see `use`).
pub fn run(loaded: Loaded, start_env: Option<&str>) -> Result<()> {
    let env_name = match start_env {
        Some(name) => {
            loaded.config.env(name)?;
            name.to_string()
        }
        None => loaded
            .config
            .default_env()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("no environments: create ~/.deployment-cli/config.yaml or an AWS profile"))?,
    };

    let (tx, rx) = mpsc::channel::<AppEvent>();
    let input_tx = tx.clone();
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if input_tx.send(AppEvent::Input(ev)).is_err() {
                break;
            }
        }
    });

    let mut terminal = ratatui::init();
    let _ = execute!(stdout(), EnableMouseCapture, EnableBracketedPaste);
    let mut mouse_on = true;
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
        previous_hook(info);
    }));

    let mut app = app::App::new(loaded, &env_name, tx, crate::modules::all());
    let result = (|| -> Result<()> {
        loop {
            terminal.draw(|f| app.render(f))?;
            let wait = if app.animating() { Duration::from_millis(100) } else { Duration::from_secs(1) };
            match rx.recv_timeout(wait) {
                Ok(ev) => {
                    app.handle(ev);
                    while let Ok(ev) = rx.try_recv() {
                        app.handle(ev);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            app.tick();
            if app.quit {
                break;
            }
            // Full-screen logs leave the mouse to the terminal, so text can be selected.
            let wanted = app.mouse_wanted();
            if wanted != mouse_on {
                let _ = if wanted { execute!(stdout(), EnableMouseCapture) } else { execute!(stdout(), DisableMouseCapture) };
                mouse_on = wanted;
            }
        }
        Ok(())
    })();

    let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    result
}
