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

use std::io::{self, IsTerminal, Write as _};
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
pub async fn run(port: i64, secret_key: &str, hook: Option<LogHook>) -> io::Result<()> {
    run_with_base_url(&format!("http://127.0.0.1:{port}"), secret_key, hook).await
}

/// Like [`run`] but draws to `output` instead of stdout (standalone mode points stdout at
/// /dev/null for the embedded server and hands the TUI the real terminal).
pub async fn run_with_output<W: io::Write>(
    port: i64,
    secret_key: &str,
    hook: Option<LogHook>,
    output: W,
) -> io::Result<()> {
    run_loop(&format!("http://127.0.0.1:{port}"), secret_key, hook, output).await
}

/// `RunWithBaseURL`: takes over the terminal (alternate screen) until the user quits. A `hook`
/// selects standalone mode: no password gate, logs come from the in-process hook.
pub async fn run_with_base_url(base_url: &str, secret_key: &str, hook: Option<LogHook>) -> io::Result<()> {
    run_loop(base_url, secret_key, hook, io::stdout()).await
}

async fn run_loop<W: io::Write>(base_url: &str, secret_key: &str, hook: Option<LogHook>, output: W) -> io::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let mut app = App::new(base_url, secret_key, hook, tx.clone());

    let mut terminal = TerminalGuard::enter(output)?;
    let stop = Arc::new(AtomicBool::new(false));
    let reader = spawn_event_reader(tx.clone(), stop.clone());

    let (w, h) = size().unwrap_or((80, 24));
    let _ = tx.send(Msg::Resize(w, h));
    app.init();

    let mut term_signal = termination_signal();
    let result = 'main: loop {
        // A failed draw still falls through to the cleanup below.
        if let Err(e) = terminal.draw(&mut app) {
            break Err(e);
        }
        let first = tokio::select! {
            msg = rx.recv() => msg,
            _ = &mut term_signal => break Ok(()),
        };
        let Some(first) = first else { break Ok(()) };
        // Apply everything already queued before redrawing.
        let mut next = Some(first);
        while let Some(msg) = next {
            if let Msg::InputClosed(e) = msg {
                break 'main Err(io::Error::other(e));
            }
            if app.update(msg) {
                break 'main Ok(());
            }
            next = rx.try_recv().ok();
        }
    };

    stop.store(true, Ordering::Relaxed);
    let _ = reader.join();
    drop(terminal);
    result
}

/// Resolves on SIGTERM or an external SIGINT (bubbletea quits the program on both); never
/// resolves elsewhere. In raw mode Ctrl+C is a key, not a signal.
fn termination_signal() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) {
            return Box::pin(async move {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = int.recv() => {}
                }
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
                Err(e) => {
                    let _ = tx.send(Msg::InputClosed(e.to_string()));
                    break;
                }
            }
            let msg = match event::read() {
                Ok(Event::Key(k)) => keys::Key::from_event(&k).map(Msg::Key),
                Ok(Event::Paste(text)) => Some(Msg::Paste(text)),
                Ok(Event::Resize(w, h)) => Some(Msg::Resize(w, h)),
                Ok(_) => None,
                Err(e) => {
                    let _ = tx.send(Msg::InputClosed(e.to_string()));
                    break;
                }
            };
            if let Some(msg) = msg
                && tx.send(msg).is_err()
            {
                break;
            }
        }
    })
}

/// Raw mode + alternate screen + bracketed paste, restored on drop and by a panic hook (so a
/// panic message lands on the normal screen instead of being swallowed).
struct TerminalGuard<W: io::Write> {
    terminal: Terminal<CrosstermBackend<W>>,
}

impl<W: io::Write> TerminalGuard<W> {
    fn enter(mut output: W) -> io::Result<Self> {
        enable_raw_mode()?;
        if let Err(e) = execute!(output, EnterAlternateScreen, EnableBracketedPaste) {
            let _ = disable_raw_mode();
            return Err(e);
        }
        install_panic_hook();
        let terminal = Terminal::new(CrosstermBackend::new(output))?;
        Ok(TerminalGuard { terminal })
    }

    fn draw(&mut self, app: &mut App) -> io::Result<()> {
        self.terminal.draw(|f| app.draw(f)).map(drop)
    }
}

/// Chains a panic hook that leaves raw mode and the alternate screen before the previous hook
/// prints. Writes to stderr, which in standalone mode is the redirected stream, so it also tries
/// the controlling terminal.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        // Prefer the controlling terminal: stdout/stderr may point at /dev/null (standalone).
        let tty = std::fs::OpenOptions::new().write(true).open("/dev/tty").ok();
        let mut out: Box<dyn io::Write> = match tty {
            Some(f) => Box::new(f),
            None => Box::new(io::stdout()),
        };
        let _ = execute!(out, DisableBracketedPaste, LeaveAlternateScreen);
        if !io::stderr().is_terminal() {
            let _ = writeln!(out, "{info}");
        }
        previous(info);
    }));
}

impl<W: io::Write> Drop for TerminalGuard<W> {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), DisableBracketedPaste, LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}
