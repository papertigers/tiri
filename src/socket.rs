// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Where the server's socket lives, and keeping other users away from it.

use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// The socket used when none is given: `default` in a per-user directory,
/// `$XDG_RUNTIME_DIR/tiri` if there is one, otherwise `/tmp/tiri-<uid>`.
pub fn default_path() -> PathBuf {
    let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(runtime) => PathBuf::from(runtime).join("tiri"),
        None => PathBuf::from(format!("/tmp/tiri-{}", rustix::process::getuid().as_raw())),
    };
    dir.join("default")
}

/// Makes sure the socket's directory exists. For the default location it
/// must also belong to us and be closed to everyone else, since anyone who
/// can reach the socket can type into our shells; that's the same
/// protection tmux relies on. A socket given explicitly is the caller's
/// responsibility.
pub fn prepare_dir(socket: &Path, private: bool) -> Result<()> {
    let dir = socket
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("couldn't create {}", dir.display()))?;
    if !private {
        return Ok(());
    }
    let meta = fs::metadata(dir).with_context(|| format!("couldn't inspect {}", dir.display()))?;
    if meta.uid() != rustix::process::getuid().as_raw() {
        bail!("{} belongs to another user", dir.display());
    }
    if meta.mode() & 0o077 != 0 {
        bail!(
            "{} is open to other users; it should be mode 700",
            dir.display()
        );
    }
    Ok(())
}

/// The file clients lock while starting a server for `socket`, so only
/// one does.
pub fn lock_path(socket: &Path) -> PathBuf {
    socket.with_extension("lock")
}

/// Where a server started for `socket` writes its errors.
pub fn log_path(socket: &Path) -> PathBuf {
    socket.with_extension("log")
}
