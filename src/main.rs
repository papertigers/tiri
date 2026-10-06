// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod app;
mod client;
mod colors;
mod config;
mod effects;
mod input;
mod keys;
mod kitty;
mod layout;
mod pane;
mod probe;
mod protocol;
mod render;
mod selection;
mod server;
mod socket;
mod theme;
mod thumbnail;
mod workspace;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use protocol::Target;

/// A terminal multiplexer whose panes scroll sideways, like the niri window
/// manager. Workspaces stack vertically and outlive the terminal: detach
/// with Ctrl-a d and attach again later.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// The server's socket. Defaults to `$TIRI_SOCKET`, then a private
    /// per-user directory.
    #[arg(short = 'S', long, global = true)]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Attach to the server, starting it if needed (the default)
    Attach {
        /// The named workspace to land on
        name: Option<String>,
    },
    /// Create a named workspace with a shell, and attach to it
    New { name: String },
    /// List the workspaces
    Ls,
    /// Kill the server and every pane in it
    KillServer,
    /// Work with the config file
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Run the server (started for you by the other commands)
    #[command(hide = true)]
    Server,
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Print the default config, commented, as a starting point
    ///
    /// For example: tiri config default > ~/.config/tiri/config.kdl
    Default,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Attach { name: None }) {
        Command::Attach { name } => client::attach(
            &socket_path(cli.socket)?,
            name.map_or(Target::Default, Target::Existing),
        ),
        Command::New { name } => client::attach(&socket_path(cli.socket)?, Target::New(name)),
        Command::Ls => client::list(&socket_path(cli.socket)?),
        Command::KillServer => client::kill_server(&socket_path(cli.socket)?),
        Command::Server => {
            // Its errors are in its log already, which is where stderr goes.
            if server::run(&socket_path(cli.socket)?).is_err() {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Config {
            command: ConfigCommand::Default,
        } => {
            print!("{}", config::DEFAULT);
            Ok(())
        }
    }
}

/// The server's socket: `--socket`, then `$TIRI_SOCKET`, then the default,
/// with its directory made ready.
fn socket_path(arg: Option<PathBuf>) -> Result<PathBuf> {
    let (socket, default) = match arg.or_else(|| std::env::var_os("TIRI_SOCKET").map(PathBuf::from))
    {
        Some(path) => (path, false),
        None => (socket::default_path(), true),
    };
    socket::prepare_dir(&socket, default)?;
    Ok(socket)
}
