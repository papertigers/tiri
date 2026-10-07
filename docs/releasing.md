# Releasing

Releases are made by [release-plz](https://release-plz.dev), from GitHub
Actions (`.github/workflows/release-plz.yml`, configured in
`release-plz.toml`).

1. Every push to `main` opens or updates a PR titled `release vX.Y.Z`. It
   bumps the version in `Cargo.toml` and `Cargo.lock`, and adds the PRs
   merged since the last release to `CHANGELOG.md`.
2. Edit that PR as needed. release-plz bumps the patch version by default;
   change the version in `Cargo.toml` (and `Cargo.lock`) for a minor one,
   and tidy the changelog entry.
3. Merge it. The release job then tags `vX.Y.Z`, makes a GitHub release
   with the changelog entry, and publishes the crate to crates.io.

## When the protocol changed

`PROTOCOL` in `src/protocol.rs` says which clients and servers can talk. A
release that changed it needs at least a minor version, and its changelog
entry should say so: after upgrading, servers left running from before have
to be stopped with `tiri kill-server` (which closes their panes).

## Setup

- Trusted publishing on crates.io: in the crate's settings there, add a
  trusted publisher for GitHub, with owner `papertigers`, repository
  `tiri`, workflow `release-plz.yml`, and no environment. The release job
  then publishes with a token made for that run alone, so there's no
  crates.io token to keep in secrets.
- `RELEASE_PLZ_TOKEN` (optional): a fine-grained token with read and write
  access to Contents and Pull requests. PRs opened with the default token
  don't start other workflows, so CI wouldn't run on the release PR; with
  this one it does. Without it, close and reopen the release PR to run CI.
- In the repository's settings, under Actions, General, Workflow
  permissions: allow GitHub Actions to create pull requests.
- Each release needs the tag of the one before: release-plz reads the
  changelog's starting point from it.
