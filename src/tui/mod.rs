//! Interactive terminal UI.

mod app;
pub mod canvas;
pub mod chart;
pub mod form;
pub mod logview;
pub mod module;
pub mod picker;
pub mod theme;
mod usage;
pub mod widgets;

use std::io::stdout;
use std::process::{Command, ExitStatus};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::Duration;

use anyhow::{Result, anyhow};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};

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
    let input = Input::start(tx.clone());

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
            if let Some(job) = app.take_interactive() {
                let program = job.command.get_program().to_string_lossy().into_owned();
                let result = hand_over(&mut terminal, &input, job.command);
                mouse_on = true;
                app.interactive_done(&program, job.missing, result);
                continue;
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

/// Reads terminal input on its own thread, and stops reading while another program owns the
/// terminal (it would otherwise take that program's keys).
struct Input {
    control: Sender<bool>,
    ack: Receiver<()>,
}

impl Input {
    fn start(tx: Sender<AppEvent>) -> Input {
        let (control, control_rx) = mpsc::channel::<bool>();
        let (ack_tx, ack) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            loop {
                match control_rx.try_recv() {
                    Ok(true) => {
                        // Paused until resumed.
                        let _ = ack_tx.send(());
                        loop {
                            match control_rx.recv() {
                                Ok(false) => break,
                                Ok(true) => {
                                    let _ = ack_tx.send(());
                                }
                                Err(_) => return,
                            }
                        }
                    }
                    Ok(false) | Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => return,
                }
                match event::poll(Duration::from_millis(50)) {
                    Ok(true) => match event::read() {
                        Ok(ev) => {
                            if tx.send(AppEvent::Input(ev)).is_err() {
                                return;
                            }
                        }
                        Err(_) => return,
                    },
                    Ok(false) => {}
                    Err(_) => return,
                }
            }
        });
        Input { control, ack }
    }

    /// Returns once the reader has stopped.
    fn pause(&self) {
        if self.control.send(true).is_ok() {
            let _ = self.ack.recv_timeout(Duration::from_secs(1));
        }
    }

    fn resume(&self) {
        let _ = self.control.send(false);
    }
}

/// Runs `command` on the plain terminal (no alternate screen, no raw mode, no mouse) and
/// takes the terminal back afterwards, redrawing everything.
fn hand_over(terminal: &mut DefaultTerminal, input: &Input, mut command: Command) -> std::io::Result<ExitStatus> {
    input.pause();
    let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    let result = command.status();
    let _ = enable_raw_mode();
    let _ = execute!(stdout(), EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste);
    let _ = terminal.clear();
    input.resume();
    result
}
