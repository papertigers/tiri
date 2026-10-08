// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Where the server's socket lives, and keeping other users away from it.

use std::fs::{self, DirBuilder};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// The socket used when none is given: `default` in a per-user directory,
/// `$XDG_RUNTIME_DIR/tiri` if there is one, otherwise `/tmp/tiri-<uid>`.
pub fn default_path() -> PathBuf {
    let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(runtime) => PathBuf::from(runtime).join("tiri"),
        None => PathBuf::from(format!(
            "/tmp/tiri-{}",
            rustix::process::getuid().as_raw()
        )),
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
    let meta = fs::metadata(dir)
        .with_context(|| format!("couldn't inspect {}", dir.display()))?;
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

/// The process at the other end of `stream`, and the user it runs as, as
/// the system recorded them when it connected: for stopping a server too
/// old to be asked. None where the system won't say.
pub fn peer_process(stream: &UnixStream) -> Option<(i32, u32)> {
    peer(stream.as_raw_fd())
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn peer(fd: RawFd) -> Option<(i32, u32)> {
    let mut pid: libc::pid_t = 0;
    let mut len = size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: `fd` is an open socket, and `pid` and `len` are valid for
    // writes of the sizes given.
    let found = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&raw mut pid).cast(),
            &mut len,
        )
    };
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: as above, for `uid` and `gid`.
    let user = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
    (found == 0 && user == 0).then_some((pid, uid))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer(fd: RawFd) -> Option<(i32, u32)> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `fd` is an open socket, and `cred` and `len` are valid for
    // writes of the sizes given.
    let found = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    (found == 0).then_some((cred.pid, cred.uid))
}

#[cfg(any(target_os = "illumos", target_os = "solaris"))]
fn peer(fd: RawFd) -> Option<(i32, u32)> {
    let mut cred: *mut libc::ucred_t = std::ptr::null_mut();
    // SAFETY: `fd` is an open socket; on success `cred` points to
    // credentials the system allocated, freed below.
    if unsafe { libc::getpeerucred(fd, &mut cred) } != 0 {
        return None;
    }
    // SAFETY: `cred` is valid until freed.
    let (pid, uid) =
        unsafe { (libc::ucred_getpid(cred), libc::ucred_geteuid(cred)) };
    // SAFETY: allocated by `getpeerucred`, and not used after.
    unsafe { libc::ucred_free(cred) };
    // Either is -1 if the system doesn't know it.
    (pid > 0 && uid != libc::uid_t::MAX).then_some((pid, uid))
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "linux",
    target_os = "android",
    target_os = "illumos",
    target_os = "solaris",
)))]
fn peer(_fd: RawFd) -> Option<(i32, u32)> {
    None
}
