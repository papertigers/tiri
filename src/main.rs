mod app;
mod input;
mod kitty;
mod layout;
mod pane;
mod render;
mod thumbnail;
mod workspace;

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::{ExecutableCommand, cursor, event, terminal};
use polling::{Events, Poller};

use app::App;

const FRAME: Duration = Duration::from_millis(16);

fn main() -> Result<()> {
    let _guard = TerminalGuard::enter()?;
    run()
}

fn run() -> Result<()> {
    // One poller watches every pane's PTY. Terminal input arrives on its own
    // thread (crossterm reads it blocking) and wakes the poller when it does.
    let poller = Arc::new(Poller::new()?);
    let (input_tx, input) = mpsc::channel();
    let waker = Arc::clone(&poller);
    thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if input_tx.send(ev).is_err() {
                break;
            }
            let _ = waker.notify();
        }
        let _ = waker.notify();
    });

    let (width, height) = terminal::size()?;
    // Each argument names a workspace to start with.
    let names: Vec<String> = std::env::args().skip(1).collect();
    let mut app = App::new(&names, Arc::clone(&poller));
    // This terminal is, for now, the one and only client.
    let mut client = app.attach(width, height)?;
    let mut stdout = io::stdout().lock();
    let mut events = Events::new();
    let mut animating = false;
    let mut last_tick = Instant::now();

    'main: loop {
        // Wake for the next animation frame, or when a pane's synchronized
        // update times out, whichever is sooner; otherwise only for I/O.
        let deadline = [
            animating.then(|| Instant::now() + FRAME),
            app.next_deadline(&client),
        ]
        .into_iter()
        .flatten()
        .min();
        app.arm_panes();
        events.clear();
        match poller.wait(
            &mut events,
            deadline.map(|d| d.saturating_duration_since(Instant::now())),
        ) {
            Ok(_) => {}
            // Signals such as SIGWINCH interrupt the wait; just go round.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
        for event in events.iter() {
            app.pane_ready(event);
        }
        loop {
            match input.try_recv() {
                Ok(event::Event::Key(key)) => app.key(&mut client, key)?,
                Ok(event::Event::Paste(text)) => app.paste(&client, &text),
                Ok(event::Event::Resize(w, h)) => app.resize(&mut client, w, h),
                Ok(_) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break 'main,
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
        animating = app.tick(dt.max(Duration::from_millis(1)));

        let (frame, cursor) = app.draw(&mut client);
        stdout.write_all(&client.take_graphics())?;
        client.render(&mut stdout, frame, cursor)?;
    }

    app.detach(&mut client);
    app.shutdown();
    stdout.write_all(&client.take_graphics())?;
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
