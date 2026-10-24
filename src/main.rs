use std::path::PathBuf;

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use uuid::Uuid;

use harklight_agent::commands::{self, CommandContext};

#[derive(Debug, Parser)]
#[command(
    name = "harklight-agent",
    version,
    about = "Send and receive request-bound Harklight conversations"
)]
struct Cli {
    #[arg(long, global = true, env = "HARKLIGHT_IDENTITY_DIR")]
    identity_dir: Option<PathBuf>,
    #[arg(long, global = true, env = "HARKLIGHT_CA_FILE")]
    ca_file: Option<PathBuf>,
    #[arg(long, global = true, env = "HARKLIGHT_ORIGIN_PORT")]
    origin_port: Option<u16>,
    #[arg(long, global = true, env = "HARKLIGHT_AGENT_STATE_DIR")]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Send {
        #[arg(long, value_name = "UUID|new")]
        session: String,
        #[arg(long)]
        sender_id: String,
        #[arg(long)]
        title: String,
        #[arg(long)]
        body: Option<String>,
        #[arg(long = "choice", action = clap::ArgAction::Append)]
        choices: Vec<String>,
        #[arg(long)]
        vendor_session_id: Option<String>,
        #[arg(long, value_enum)]
        route: Option<Route>,
        #[arg(long)]
        controller: bool,
        #[arg(long = "file", action = clap::ArgAction::Append)]
        files: Vec<PathBuf>,
    },
    Attach {
        #[arg(long)]
        session: Uuid,
        #[arg(long, value_name = "ID|none")]
        vendor_session_id: String,
        #[arg(long, value_enum)]
        route: Route,
        #[arg(long)]
        controller: bool,
    },
    Listen(ListenArgs),
    Ack {
        #[arg(long)]
        session: Uuid,
        #[arg(long)]
        event: String,
    },
    Reply {
        #[arg(long)]
        session: Uuid,
        #[arg(long)]
        request: String,
        #[arg(long)]
        in_reply_to: String,
        #[arg(long)]
        body: String,
        #[arg(long = "file", action = clap::ArgAction::Append)]
        files: Vec<PathBuf>,
        #[arg(long)]
        op_id: Option<Uuid>,
    },
    Revise {
        #[arg(long)]
        session: Uuid,
        #[arg(long)]
        request: String,
        #[arg(long)]
        base_revision: u64,
        #[arg(long)]
        title: String,
        #[arg(long)]
        body: Option<String>,
        #[arg(long = "choice", action = clap::ArgAction::Append)]
        choices: Vec<String>,
        #[arg(long)]
        clear_choices: bool,
        #[arg(long)]
        in_reply_to: Option<String>,
        #[arg(long = "file", action = clap::ArgAction::Append)]
        files: Vec<PathBuf>,
        #[arg(long)]
        op_id: Option<Uuid>,
    },
    Withdraw {
        #[arg(long)]
        session: Uuid,
        #[arg(long)]
        request: String,
        #[arg(long)]
        in_reply_to: Option<String>,
        #[arg(long)]
        op_id: Option<Uuid>,
    },
    Retract {
        #[arg(long)]
        session: Uuid,
        #[arg(long)]
        request: String,
        #[arg(long)]
        op_id: Option<Uuid>,
    },
    Result {
        #[arg(long)]
        session: Uuid,
        #[arg(long)]
        request: String,
        #[arg(long)]
        in_reply_to: String,
        #[arg(long, value_enum)]
        status: ResultStatus,
        #[arg(long)]
        op_id: Option<Uuid>,
    },
    Status {
        #[arg(long)]
        session: Uuid,
    },
    DiscardUploads {
        #[arg(long)]
        sender_id: String,
        #[arg(long)]
        message_id: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        op_id: Option<Uuid>,
    },
    Session {
        #[command(subcommand)]
        action: SessionCommand,
    },
    Retry {
        #[arg(long)]
        op_id: Uuid,
    },
    #[command(group(
        ArgGroup::new("file-use-result")
            .required(true)
            .multiple(false)
            .args(["used", "unusable"])
    ))]
    FileUse {
        #[arg(long)]
        session: Uuid,
        #[arg(long)]
        event: String,
        #[arg(long)]
        file: String,
        #[arg(long, group = "file-use-result")]
        used: bool,
        #[arg(long, group = "file-use-result")]
        unusable: bool,
        #[arg(long)]
        reason: Option<String>,
    },
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("mode").required(true).multiple(false).args(["follow", "next"]))) ]
struct ListenArgs {
    #[arg(long)]
    session: Uuid,
    #[arg(long, value_enum)]
    session_context: Option<SessionContext>,
    #[arg(long)]
    claude_code_version: Option<String>,
    #[arg(long)]
    background_tasks_enabled: bool,
    #[arg(long, group = "mode")]
    follow: bool,
    #[arg(long, group = "mode")]
    next: bool,
    #[arg(long)]
    controller: bool,
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    End {
        #[arg(long)]
        session: Uuid,
        #[arg(long, required = true)]
        gone: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Route {
    ClaudeCode,
    Codex,
    Other,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SessionContext {
    Interactive,
    Bare,
    Print,
    Sdk,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ResultStatus {
    Received,
    Applied,
    Refused,
}

impl From<ResultStatus> for &'static str {
    fn from(value: ResultStatus) -> Self {
        match value {
            ResultStatus::Received => "received",
            ResultStatus::Applied => "applied",
            ResultStatus::Refused => "refused",
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let context = match CommandContext::load(
        cli.identity_dir.as_deref(),
        cli.ca_file.as_deref(),
        cli.origin_port,
        cli.state_dir.as_deref(),
    ) {
        Ok(context) => context,
        Err(error) => fail(error.to_string(), 1),
    };

    let result = match cli.command {
        Command::Send {
            session,
            sender_id,
            title,
            body,
            choices,
            vendor_session_id,
            route,
            controller,
            files,
        } => commands::send(
            &context,
            &session,
            &sender_id,
            &title,
            body.as_deref(),
            &choices,
            vendor_session_id.as_deref(),
            route.map(Into::into),
            controller,
            &files,
        ),
        Command::Attach {
            session,
            vendor_session_id,
            route,
            controller,
        } => commands::attach(
            &context,
            session,
            &vendor_session_id,
            route.into(),
            controller,
        ),
        Command::Listen(args) => commands::listen(
            &context,
            args.session,
            args.session_context.map(Into::into),
            args.claude_code_version.as_deref(),
            args.background_tasks_enabled,
            args.follow,
            args.controller,
        ),
        Command::Ack { session, event } => commands::ack(&context, session, &event),
        Command::Reply {
            session,
            request,
            in_reply_to,
            body,
            files,
            op_id,
        } => commands::reply(
            &context,
            session,
            &request,
            &in_reply_to,
            &body,
            &files,
            op_id,
        ),
        Command::Revise {
            session,
            request,
            base_revision,
            title,
            body,
            choices,
            clear_choices,
            in_reply_to,
            files,
            op_id,
        } => commands::revise(
            &context,
            session,
            &request,
            base_revision,
            &title,
            body.as_deref(),
            &choices,
            clear_choices,
            in_reply_to.as_deref(),
            &files,
            op_id,
        ),
        Command::Withdraw {
            session,
            request,
            in_reply_to,
            op_id,
        } => commands::withdraw(&context, session, &request, in_reply_to.as_deref(), op_id),
        Command::Retract {
            session,
            request,
            op_id,
        } => commands::retract(&context, session, &request, op_id),
        Command::Result {
            session,
            request,
            in_reply_to,
            status,
            op_id,
        } => commands::result(
            &context,
            session,
            &request,
            &in_reply_to,
            status.into(),
            op_id,
        ),
        Command::Status { session } => commands::status(&context, session),
        Command::DiscardUploads {
            sender_id,
            message_id,
            revision,
            op_id,
        } => commands::discard_uploads(&context, &sender_id, &message_id, revision, op_id),
        Command::Session { action } => match action {
            SessionCommand::End { session, gone: _ } => commands::session_end(&context, session),
        },
        Command::Retry { op_id } => commands::retry(&context, op_id),
        Command::FileUse {
            session,
            event,
            file,
            used,
            unusable,
            reason,
        } => commands::file_use(
            &context,
            session,
            &event,
            &file,
            match (used, unusable) {
                (true, false) => "used",
                (false, true) => "unusable",
                _ => unreachable!("clap requires exactly one file-use outcome"),
            },
            reason.as_deref(),
        ),
    };

    if let Err(error) = result {
        fail(error.message, error.exit_code);
    }
}

impl From<Route> for commands::OriginHarness {
    fn from(value: Route) -> Self {
        match value {
            Route::ClaudeCode => Self::ClaudeCode,
            Route::Codex => Self::Codex,
            Route::Other => Self::Other,
        }
    }
}

impl From<SessionContext> for commands::SessionContext {
    fn from(value: SessionContext) -> Self {
        match value {
            SessionContext::Interactive => Self::Interactive,
            SessionContext::Bare => Self::Bare,
            SessionContext::Print => Self::Print,
            SessionContext::Sdk => Self::Sdk,
        }
    }
}

fn fail(message: String, exit_code: i32) -> ! {
    eprintln!("{message}");
    std::process::exit(exit_code);
}
