//! Terminal management UI (Go: internal/tui). A client of the management API: six tabs
//! (Dashboard, Config, Auth Files, API Keys, OAuth, Logs) plus a password gate for remote use.
//!
//! The UI is an Elm-style loop like bubbletea: key/resize events and the results of background
//! HTTP calls arrive as [`Msg`]s, [`App::update`] applies them and the screen is redrawn once per
//! batch of messages. Nothing repaints on a timer.

pub mod app;
pub mod auth_tab;
pub mod browser;
pub mod client;
pub mod clipboard;
pub mod config_tab;
pub mod dashboard;
pub mod i18n;
pub mod jsonutil;
pub mod keys;
pub mod keys_tab;
pub mod loghook;
pub mod logs_tab;
pub mod msg;
pub mod oauth_tab;
pub mod styles;
pub mod text;
pub mod widgets;

use std::io::{self, Stdout};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode, size,
};
use tokio::sync::mpsc;

pub use app::App;
pub use client::Client;
pub use loghook::LogHook;
pub use msg::Msg;

/// `Run`: the TUI against the management API on localhost at `port`.
pub async fn run(port: u16, secret_key: &str, hook: Option<LogHook>) -> io::Result<()> {
    run_with_base_url(&format!("http://127.0.0.1:{port}"), secret_key, hook).await
}

/// `RunWithBaseURL`: takes over the terminal (alternate screen) until the user quits. A `hook`
/// selects standalone mode: no password gate, logs come from the in-process hook.
pub async fn run_with_base_url(base_url: &str, secret_key: &str, hook: Option<LogHook>) -> io::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let mut app = App::new(base_url, secret_key, hook, tx.clone());

    let mut terminal = TerminalGuard::enter()?;
    let stop = Arc::new(AtomicBool::new(false));
    let reader = spawn_event_reader(tx.clone(), stop.clone());

    let (w, h) = size().unwrap_or((80, 24));
    let _ = tx.send(Msg::Resize(w, h));
    app.init();

    let mut term_signal = termination_signal();
    let result = loop {
        terminal.draw(&mut app)?;
        let first = tokio::select! {
            msg = rx.recv() => msg,
            _ = &mut term_signal => break Ok(()),
        };
        let Some(first) = first else { break Ok(()) };
        // Apply everything already queued before redrawing.
        let mut quit = app.update(first);
        while !quit {
            match rx.try_recv() {
                Ok(msg) => quit = app.update(msg),
                Err(_) => break,
            }
        }
        if quit {
            break Ok(());
        }
    };

    stop.store(true, Ordering::Relaxed);
    let _ = reader.join();
    drop(terminal);
    result
}

/// Resolves on SIGTERM (bubbletea quits on it); never resolves elsewhere.
fn termination_signal() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            return Box::pin(async move {
                term.recv().await;
            });
        }
    }
    Box::pin(std::future::pending())
}

/// Reads terminal events on a dedicated thread, waking every 100ms only to check `stop`.
fn spawn_event_reader(tx: mpsc::UnboundedSender<Msg>, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match event::poll(Duration::from_millis(100)) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => break,
            }
            let msg = match event::read() {
                Ok(Event::Key(k)) => keys::Key::from_event(&k).map(Msg::Key),
                Ok(Event::Paste(text)) => Some(Msg::Paste(text)),
                Ok(Event::Resize(w, h)) => Some(Msg::Resize(w, h)),
                Ok(_) => None,
                Err(_) => break,
            };
            if let Some(msg) = msg
                && tx.send(msg).is_err()
            {
                break;
            }
        }
    })
}

/// Raw mode + alternate screen + bracketed paste, restored on drop (also on panic unwinding).
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(e) = execute!(stdout, EnterAlternateScreen, EnableBracketedPaste) {
            let _ = disable_raw_mode();
            return Err(e);
        }
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(TerminalGuard { terminal })
    }

    fn draw(&mut self, app: &mut App) -> io::Result<()> {
        self.terminal.draw(|f| app.draw(f)).map(drop)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), DisableBracketedPaste, LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}
