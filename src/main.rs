//! mg: Gmail from the command line, built for agents.

mod api;
mod auth;
mod error;
mod gmail;
mod output;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::{Parser, Subcommand};

use crate::api::Gmail;
use crate::error::{Error, Result};
use crate::gmail::search::Mailbox;
use crate::output::{Mode, View};

/// Gmail from the command line, built for agents.
///
/// Prints JSON when piped and text in a terminal. Errors go to stderr; exit
/// codes: 1 usage or local error, 2 auth, 3 Gmail API.
#[derive(Parser)]
#[command(name = "mg", version)]
struct Cli {
    /// Print compact JSON (default when piped)
    #[arg(long, global = true, conflicts_with = "text")]
    json: bool,
    /// Print human-readable text (default in a terminal)
    #[arg(long, global = true)]
    text: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Set up and manage Google sign-in
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Search threads with a Gmail query (same syntax as the Gmail search box)
    Search {
        /// e.g. 'from:bank has:attachment newer_than:30d'
        query: String,
        /// Maximum threads to return (1-500)
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=500))]
        limit: u32,
    },
    /// List a common mailbox
    #[command(name = "box")]
    Mailbox {
        #[arg(value_enum)]
        which: Mailbox,
        /// Maximum threads to return (1-500)
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=500))]
        limit: u32,
    },
    /// Show a thread as plain text (accepts a thread or message id)
    Thread {
        id: String,
        /// Keep quoted history ("On ... wrote:", "> " lines)
        #[arg(long)]
        full: bool,
        /// Maximum characters per message body
        #[arg(long, default_value_t = 4000)]
        max_chars: usize,
    },
    /// List or save attachments
    Attachment {
        #[command(subcommand)]
        command: AttachmentCommand,
    },
}

#[derive(Subcommand)]
enum AuthCommand {
    /// Import the Desktop-app OAuth client JSON from Google Cloud console
    Setup {
        #[arg(long)]
        client_json: PathBuf,
    },
    /// Sign in with the browser and store the refresh token in the keyring
    Login,
    /// Revoke access at Google and remove the keyring entry
    Logout,
    /// Show whether mg is set up and the sign-in still works
    Status,
}

#[derive(Subcommand)]
enum AttachmentCommand {
    /// List attachments of a thread or message
    List {
        /// Thread or message id
        id: String,
    },
    /// Save attachments of a message to a directory
    Save {
        /// Message id (see the MESSAGE column of `attachment list`)
        message_id: String,
        /// Part from `attachment list` (or an attachment id); omit to save all
        part: Option<String>,
        /// Directory to save into (created if missing)
        #[arg(long)]
        out: PathBuf,
    },
}

fn run(command: Command) -> Result<View> {
    match command {
        Command::Auth { command } => match command {
            AuthCommand::Setup { client_json } => auth::setup(&client_json),
            AuthCommand::Login => auth::login(),
            AuthCommand::Logout => auth::logout(),
            AuthCommand::Status => auth::status(),
        },
        Command::Search { query, limit } => gmail::search::run(&Gmail::connect()?, &query, limit),
        Command::Mailbox { which, limit } => {
            gmail::search::run(&Gmail::connect()?, which.query(), limit)
        }
        Command::Thread { id, full, max_chars } => {
            gmail::validate_id(&id, "thread")?;
            gmail::thread::run(&Gmail::connect()?, &id, full, max_chars)
        }
        Command::Attachment { command } => match command {
            AttachmentCommand::List { id } => {
                gmail::validate_id(&id, "thread")?;
                gmail::attachment::list(&Gmail::connect()?, &id)
            }
            AttachmentCommand::Save { message_id, part, out } => {
                gmail::validate_id(&message_id, "message")?;
                gmail::attachment::save(&Gmail::connect()?, &message_id, part.as_deref(), &out)
            }
        },
    }
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => return parse_error(e),
    };
    let mode = Mode::detect(cli.json, cli.text);
    match run(cli.command) {
        Ok(view) => {
            output::emit(mode, &view);
            ExitCode::SUCCESS
        }
        Err(err) => {
            output::emit_error(mode, &err);
            ExitCode::from(err.exit_code() as u8)
        }
    }
}

/// Help and version print normally; usage errors exit 1 (JSON when piped).
fn parse_error(e: clap::Error) -> ExitCode {
    if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
        let _ = e.print();
        return ExitCode::SUCCESS;
    }
    let args: Vec<String> = std::env::args().collect();
    let mode = Mode::detect(
        args.iter().any(|a| a == "--json"),
        args.iter().any(|a| a == "--text"),
    );
    if mode == Mode::Text {
        let _ = e.print();
    } else {
        // First paragraph of clap's message, on one line.
        let rendered = e.render().to_string();
        let message = rendered
            .lines()
            .take_while(|l| !l.trim().is_empty())
            .map(str::trim)
            .collect::<Vec<_>>()
            .join(" ");
        let message = message.strip_prefix("error: ").unwrap_or(&message);
        output::emit_error(mode, &Error::usage(message).hint("see `mg --help`"));
    }
    ExitCode::from(1)
}
