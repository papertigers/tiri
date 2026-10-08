// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The ssh agent for programs in panes. Every pane's `SSH_AUTH_SOCK` is
//! one socket the server listens on, beside its own, and each connection
//! to it is passed through to the agent of the client used most recently.
//! The path never changes, so shells started long ago follow whichever
//! client you're at now, without re-reading anything.

use std::io;
use std::net::Shutdown;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

/// The socket panes reach their agent through, beside the server's.
pub fn path(socket: &Path) -> PathBuf {
    socket.with_extension("agent")
}

/// The socket panes' programs connect to, and where it leads.
pub struct AgentProxy {
    path: PathBuf,
    /// The agent connections go to: the latest client's, if it has one.
    target: Arc<Mutex<Option<PathBuf>>>,
    /// To tell this socket from one a later server puts at the same path.
    bound: Option<(u64, u64)>,
}

impl AgentProxy {
    /// Listens at `path`, replacing what's left there by a server before:
    /// the server's own socket is bound already, so no other one is using
    /// it. Connections go nowhere until there's a target.
    pub fn start(path: PathBuf) -> io::Result<Self> {
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let listener = UnixListener::bind(&path)?;
        // Whoever can reach it can use your keys.
        std::fs::set_permissions(
            &path,
            std::fs::Permissions::from_mode(0o600),
        )?;
        let bound =
            std::fs::metadata(&path).ok().map(|meta| (meta.dev(), meta.ino()));
        let target = Arc::new(Mutex::new(None));
        let shared = Arc::clone(&target);
        thread::spawn(move || accept(&listener, &shared));
        Ok(Self { path, target, bound })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Sends connections from now on to the agent at `agent`, or nowhere.
    pub fn set_target(&self, agent: Option<PathBuf>) {
        if let Ok(mut target) = self.target.lock() {
            *target = agent;
        }
    }
}

impl Drop for AgentProxy {
    fn drop(&mut self) {
        let current = std::fs::metadata(&self.path)
            .ok()
            .map(|meta| (meta.dev(), meta.ino()));
        if current.is_some() && current == self.bound {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Passes each connection on to the target agent, for as long as the
/// server runs. One with no agent to go to is closed: to ssh, the same as
/// no agent.
fn accept(listener: &UnixListener, target: &Mutex<Option<PathBuf>>) {
    for program in listener.incoming() {
        let Ok(program) = program else {
            continue;
        };
        let agent = target.lock().ok().and_then(|target| target.clone());
        let Some(agent) = agent else {
            continue;
        };
        match UnixStream::connect(&agent) {
            Ok(agent) => splice(program, agent),
            Err(e) => {
                log::debug!(
                    "couldn't reach the agent at {}: {e}",
                    agent.display()
                );
            }
        }
    }
}

/// Copies between `a` and `b` both ways, each on a thread of its own,
/// until both sides are done.
fn splice(a: UnixStream, b: UnixStream) {
    let (Ok(a2), Ok(b2)) = (a.try_clone(), b.try_clone()) else {
        return;
    };
    thread::spawn(move || copy(a, b));
    thread::spawn(move || copy(b2, a2));
}

/// Copies from `from` to `to` until `from` ends, then tells `to` there's
/// no more.
fn copy(mut from: UnixStream, mut to: UnixStream) {
    let _ = io::copy(&mut from, &mut to);
    let _ = to.shutdown(Shutdown::Write);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("tiri-agent-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A pretend agent that answers each request with it reversed.
    fn fake_agent(path: &Path) {
        let listener = UnixListener::bind(path).unwrap();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut request = Vec::new();
                stream.read_to_end(&mut request).unwrap();
                request.reverse();
                stream.write_all(&request).unwrap();
            }
        });
    }

    /// What the proxy answers `request` with. With no agent to go to, it
    /// hangs up at once, maybe before the request is even sent: then the
    /// write fails (a broken pipe), or the read (a reset), depending on the
    /// system and the timing. Either way that's no answer.
    fn ask(proxy: &Path, request: &[u8]) -> Vec<u8> {
        let mut stream = UnixStream::connect(proxy).unwrap();
        let _ = (stream.write_all(request))
            .and_then(|()| stream.shutdown(Shutdown::Write));
        let mut reply = Vec::new();
        let _ = stream.read_to_end(&mut reply);
        reply
    }

    #[test]
    fn connections_go_to_the_current_agent() {
        let dir = scratch_dir("route");
        let (one, two) = (dir.join("one"), dir.join("two"));
        fake_agent(&one);
        fake_agent(&two);
        let proxy = AgentProxy::start(path(&dir.join("default"))).unwrap();

        // With no agent, connections are closed unanswered.
        assert_eq!(ask(proxy.path(), b"abc"), b"");
        proxy.set_target(Some(one.clone()));
        assert_eq!(ask(proxy.path(), b"abc"), b"cba");
        // An agent that's gone is the same as none.
        proxy.set_target(Some(dir.join("gone")));
        assert_eq!(ask(proxy.path(), b"abc"), b"");
        proxy.set_target(Some(two));
        assert_eq!(ask(proxy.path(), b"xyz"), b"zyx");

        let mode = std::fs::metadata(proxy.path()).unwrap().mode();
        assert_eq!(mode & 0o777, 0o600);
        let path = proxy.path().to_owned();
        drop(proxy);
        assert!(!path.exists(), "removed when the server stops");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
