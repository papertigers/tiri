mod app;
mod input;
mod kitty;
mod layout;
mod pane;
mod render;
mod thumbnail;

use std::io::{self, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::{ExecutableCommand, cursor, event, terminal};

use app::App;
use layout::PaneId;
use render::Renderer;

const FRAME: Duration = Duration::from_millis(16);

pub enum Event {
    Terminal(event::Event),
    Output(PaneId, Vec<u8>),
    Exited(PaneId),
}

fn main() -> Result<()> {
    let _guard = TerminalGuard::enter()?;
    run()
}

fn run() -> Result<()> {
    let (tx, rx) = mpsc::channel();

    let input_tx = tx.clone();
    thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if input_tx.send(Event::Terminal(ev)).is_err() {
                break;
            }
        }
    });

    let (width, height) = terminal::size()?;
    let mut app = App::new(width, height, tx)?;
    let mut renderer = Renderer::default();
    let mut stdout = io::stdout().lock();
    let mut animating = false;
    let mut last_tick = Instant::now();

    loop {
        // Wake for the next animation frame, or when a pane's synchronized
        // update times out, whichever is sooner.
        let deadline = [
            animating.then(|| Instant::now() + FRAME),
            app.next_deadline(),
        ]
        .into_iter()
        .flatten()
        .min();
        let first = match deadline {
            Some(deadline) => {
                match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(ev) => Some(ev),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            None => match rx.recv() {
                Ok(ev) => Some(ev),
                Err(_) => break,
            },
        };
        for ev in first.into_iter().chain(rx.try_iter()) {
            match ev {
                Event::Terminal(event::Event::Key(key)) => app.key(key)?,
                Event::Terminal(event::Event::Paste(text)) => app.paste(&text),
                Event::Terminal(event::Event::Resize(w, h)) => {
                    app.resize(w, h);
                    renderer.invalidate();
                }
                Event::Terminal(_) => {}
                Event::Output(id, bytes) => app.pane_output(id, &bytes),
                Event::Exited(id) => app.pane_exited(id),
            }
        }
        if app.quit || app.is_empty() {
            break;
        }

        let now = Instant::now();
        app.expire_syncs(now);
        // Don't let a long idle wait turn into one giant animation step.
        let dt = if animating {
            now - last_tick
        } else {
            Duration::ZERO
        };
        last_tick = now;
        animating = app.strip_mut().tick(dt.max(Duration::from_millis(1)));

        let (frame, cursor) = app.draw();
        stdout.write_all(&app.take_graphics())?;
        renderer.draw(&mut stdout, frame, cursor)?;
    }

    app.shutdown();
    stdout.write_all(&app.take_graphics())?;
    stdout.flush()?;
    Ok(())
}

/// Puts the terminal into raw mode on the alternate screen, and restores it
/// on drop or panic.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        let mut out = io::stdout();
        out.execute(terminal::EnterAlternateScreen)?;
        out.execute(event::EnableBracketedPaste)?;
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            hook(info);
        }));
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

fn restore() {
    let mut out = io::stdout();
    let _ = out.execute(event::DisableBracketedPaste);
    let _ = out.execute(terminal::LeaveAlternateScreen);
    let _ = out.execute(cursor::Show);
    let _ = terminal::disable_raw_mode();
    let _ = out.flush();
}
