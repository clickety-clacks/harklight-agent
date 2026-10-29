use std::path::PathBuf;

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use uuid::Uuid;

use harklight_agent::commands::{self, CommandContext, MessageFieldOptions};

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
        #[arg(long, value_name = "UUID|new", required_unless_present = "execute")]
        session: Option<String>,
        #[arg(long, required_unless_present = "execute")]
        sender_id: Option<String>,
        #[arg(long, required_unless_present = "execute")]
        title: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(
            long = "choice",
            action = clap::ArgAction::Append,
            help = "Add this exact value choice; may be repeated"
        )]
        choices: Vec<String>,
        #[arg(
            long,
            conflicts_with = "choices",
            help = "Offer only the Seen action for an informational message"
        )]
        informational: bool,
        #[arg(long = "supersedes", action = clap::ArgAction::Append, value_name = "MESSAGE_ID")]
        supersedes: Vec<String>,
        #[arg(long)]
        urgency: Option<String>,
        #[arg(long)]
        by_when: Option<String>,
        #[arg(long)]
        stake: Option<String>,
        #[arg(long)]
        thread: Option<String>,
        #[arg(long = "default")]
        default_choice: Option<String>,
        #[arg(long = "default-at")]
        default_at: Option<String>,
        #[arg(long = "choice-style", action = clap::ArgAction::Append)]
        choice_styles: Vec<String>,
        #[arg(long)]
        vendor_session_id: Option<String>,
        #[arg(long, value_enum)]
        route: Option<Route>,
        #[arg(long)]
        controller: bool,
        #[arg(long = "file", action = clap::ArgAction::Append)]
        files: Vec<PathBuf>,
        #[arg(long, conflicts_with = "execute")]
        prepare: bool,
        #[arg(long, conflicts_with = "prepare")]
        execute: bool,
        #[arg(long)]
        op_id: Option<Uuid>,
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
    Read {
        #[arg(long)]
        session: Uuid,
    },
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
        urgency: Option<String>,
        #[arg(long)]
        by_when: Option<String>,
        #[arg(long)]
        stake: Option<String>,
        #[arg(long = "default")]
        default_choice: Option<String>,
        #[arg(long = "default-at")]
        default_at: Option<String>,
        #[arg(long)]
        clear_default: bool,
        #[arg(long = "choice-style", action = clap::ArgAction::Append)]
        choice_styles: Vec<String>,
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
            informational,
            supersedes,
            urgency,
            by_when,
            stake,
            thread,
            default_choice,
            default_at,
            choice_styles,
            vendor_session_id,
            route,
            controller,
            files,
            prepare,
            execute,
            op_id,
        } => commands::send_command_with_fields(
            &context,
            session.as_deref(),
            sender_id.as_deref(),
            title.as_deref(),
            body.as_deref(),
            &choices,
            informational,
            &supersedes,
            vendor_session_id.as_deref(),
            route.map(Into::into),
            controller,
            &files,
            prepare,
            execute,
            op_id,
            &MessageFieldOptions {
                urgency,
                by_when,
                stake,
                thread,
                default_choice,
                default_at,
                choice_styles,
                clear_default: false,
            },
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
        Command::Read { session } => commands::read(&context, session),
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
            urgency,
            by_when,
            stake,
            default_choice,
            default_at,
            clear_default,
            choice_styles,
            in_reply_to,
            files,
            op_id,
        } => commands::revise_with_fields(
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
            &MessageFieldOptions {
                urgency,
                by_when,
                stake,
                thread: None,
                default_choice,
                default_at,
                choice_styles,
                clear_default,
            },
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_prepare_accepts_existing_inputs_and_required_operation_id() {
        let cli = Cli::try_parse_from([
            "harklight-agent",
            "send",
            "--prepare",
            "--op-id",
            "123e4567-e89b-42d3-a456-426614174000",
            "--session",
            "new",
            "--sender-id",
            "app:example",
            "--title",
            "A title",
            "--route",
            "claude-code",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Send {
                prepare: true,
                execute: false,
                op_id: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn send_accepts_multiple_superseded_message_ids_in_order() {
        let cli = Cli::try_parse_from([
            "harklight-agent",
            "send",
            "--session",
            "new",
            "--sender-id",
            "app:example",
            "--title",
            "Updated plan",
            "--route",
            "claude-code",
            "--supersedes",
            "earlier-1",
            "--supersedes",
            "earlier-2",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Send { supersedes, .. }
                if supersedes == vec!["earlier-1".to_string(), "earlier-2".to_string()]
        ));
    }

    #[test]
    fn send_execute_needs_only_the_saved_operation_id() {
        let cli = Cli::try_parse_from([
            "harklight-agent",
            "send",
            "--execute",
            "--op-id",
            "123e4567-e89b-42d3-a456-426614174000",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Send {
                prepare: false,
                execute: true,
                session: None,
                sender_id: None,
                title: None,
                op_id: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn send_prepare_and_execute_are_mutually_exclusive() {
        assert!(
            Cli::try_parse_from([
                "harklight-agent",
                "send",
                "--prepare",
                "--execute",
                "--op-id",
                "123e4567-e89b-42d3-a456-426614174000",
            ])
            .is_err()
        );
    }

    #[test]
    fn read_is_a_one_shot_session_command_without_listener_options() {
        let cli = Cli::try_parse_from([
            "harklight-agent",
            "read",
            "--session",
            "123e4567-e89b-42d3-a456-426614174000",
        ])
        .unwrap();
        assert!(
            matches!(cli.command, Command::Read { session } if session.to_string() == "123e4567-e89b-42d3-a456-426614174000")
        );

        assert!(
            Cli::try_parse_from([
                "harklight-agent",
                "read",
                "--session",
                "123e4567-e89b-42d3-a456-426614174000",
                "--controller",
            ])
            .is_err()
        );
    }

    #[test]
    fn informational_send_is_explicit_and_cannot_include_choices() {
        let cli = Cli::try_parse_from([
            "harklight-agent",
            "send",
            "--session",
            "new",
            "--sender-id",
            "app:example",
            "--title",
            "A notice",
            "--route",
            "claude-code",
            "--informational",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Send {
                informational: true,
                choices,
                ..
            } if choices.is_empty()
        ));

        assert!(
            Cli::try_parse_from([
                "harklight-agent",
                "send",
                "--session",
                "new",
                "--sender-id",
                "app:example",
                "--title",
                "A notice",
                "--route",
                "claude-code",
                "--informational",
                "--choice",
                "Approve",
            ])
            .is_err()
        );
    }

    #[test]
    fn send_and_revise_accept_the_message_field_flags_but_revise_has_no_thread_flag() {
        let send = Cli::try_parse_from([
            "harklight-agent",
            "send",
            "--session",
            "new",
            "--sender-id",
            "app:example",
            "--title",
            "A request",
            "--choice",
            "Hold",
            "--choice",
            "Ship",
            "--urgency",
            "high",
            "--by-when",
            "2026-10-30T22:00:00Z",
            "--stake",
            "A one-week delay",
            "--thread",
            "release",
            "--default",
            "Ship",
            "--default-at",
            "2026-10-30T23:00:00Z",
            "--choice-style",
            "Ship=destructive",
            "--choice-style",
            "Hold=secondary",
        ])
        .unwrap();

        assert!(matches!(
            send.command,
            Command::Send {
                urgency: Some(urgency),
                by_when: Some(by_when),
                stake: Some(stake),
                thread: Some(thread),
                default_choice: Some(default_choice),
                default_at: Some(default_at),
                choice_styles,
                ..
            } if urgency == "high"
                && by_when == "2026-10-30T22:00:00Z"
                && stake == "A one-week delay"
                && thread == "release"
                && default_choice == "Ship"
                && default_at == "2026-10-30T23:00:00Z"
                && choice_styles == vec!["Ship=destructive", "Hold=secondary"]
        ));

        let revise = Cli::try_parse_from([
            "harklight-agent",
            "revise",
            "--session",
            "123e4567-e89b-42d3-a456-426614174000",
            "--request",
            "message-1",
            "--base-revision",
            "1",
            "--title",
            "Updated request",
            "--urgency",
            "now",
            "--clear-default",
            "--choice-style",
            "Ship=primary",
        ])
        .unwrap();
        assert!(matches!(
            revise.command,
            Command::Revise {
                urgency: Some(urgency),
                clear_default: true,
                choice_styles,
                ..
            } if urgency == "now" && choice_styles == vec!["Ship=primary"]
        ));

        assert!(
            Cli::try_parse_from([
                "harklight-agent",
                "revise",
                "--session",
                "123e4567-e89b-42d3-a456-426614174000",
                "--request",
                "message-1",
                "--base-revision",
                "1",
                "--title",
                "Updated request",
                "--thread",
                "release",
            ])
            .is_err()
        );
    }
}
