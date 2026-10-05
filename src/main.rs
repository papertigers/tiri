mod app;
mod client;
mod colors;
mod input;
mod kitty;
mod layout;
mod pane;
mod probe;
mod protocol;
mod render;
mod selection;
mod server;
mod socket;
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
    /// The server's socket. Defaults to $TIRI_SOCKET, then a private
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
    /// Run the server (started for you by the other commands)
    #[command(hide = true)]
    Server,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let (socket, default) = match cli
        .socket
        .or_else(|| std::env::var_os("TIRI_SOCKET").map(PathBuf::from))
    {
        Some(path) => (path, false),
        None => (socket::default_path(), true),
    };
    socket::prepare_dir(&socket, default)?;

    match cli.command.unwrap_or(Command::Attach { name: None }) {
        Command::Attach { name } => {
            client::attach(&socket, name.map_or(Target::Default, Target::Existing))
        }
        Command::New { name } => client::attach(&socket, Target::New(name)),
        Command::Ls => client::list(&socket),
        Command::KillServer => client::kill_server(&socket),
        Command::Server => server::run(&socket),
    }
}
