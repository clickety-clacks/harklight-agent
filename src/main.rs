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

fn send_cli_warnings(
    message_type: Option<&str>,
    informational: bool,
    urgency: Option<&str>,
    execute: bool,
) -> Vec<&'static str> {
    if execute {
        return Vec::new();
    }

    let mut warnings = Vec::new();
    if urgency.is_some() {
        warnings.push("warning: --urgency is deprecated; use --importance");
    }
    if message_type.is_none() && !informational {
        warnings.push(
            "warning: add --type request (or --type fyi); untyped sends will be refused by the Server",
        );
    }
    warnings
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(
        after_help = "For picture choices, provide an attention-client.ui/1 document and attach its images with --file:\n  harklight-agent send --session new --sender-id app:example --route claude-code --type request --title \"Choose the cover\" --answer-ui cover.json --file cover-a.jpg --file cover-b.jpg\n\nOnly after a definite refusal (exit 2), correct the named input and submit a new send. A prepared operation stays unchanged; use a new operation for the correction. For an unknown outcome (exit 7), retry only with `harklight-agent retry --op-id <KEY>` using the same key and unchanged inputs; do not submit a changed send."
    )]
    Send {
        #[arg(long, value_name = "UUID|new", required_unless_present = "execute")]
        session: Option<String>,
        #[arg(long, required_unless_present = "execute")]
        sender_id: Option<String>,
        #[arg(
            long,
            help = "Display this exact string to the recipient, independent of sender-id authentication"
        )]
        display_name: Option<String>,
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
        #[arg(long, help = "Equivalent to --type fyi; offer only the Seen action")]
        informational: bool,
        #[arg(
            long = "type",
            value_name = "fyi|request",
            help = "Choose an FYI or a request that needs an answer"
        )]
        message_type: Option<String>,
        #[arg(
            long = "kind",
            value_name = "blocker|success",
            help = "Label this message as a blocker or success"
        )]
        message_kind: Option<String>,
        #[arg(
            long,
            help = "Your importance estimate; Harklight decides the priority the recipient sees"
        )]
        importance: Option<String>,
        #[arg(long, hide = true)]
        urgency: Option<String>,
        #[arg(long, help = "Add a free-text Reply action to a request")]
        reply: bool,
        #[arg(long = "supersedes", action = clap::ArgAction::Append, value_name = "MESSAGE_ID")]
        supersedes: Vec<String>,
        #[arg(long, help = "RFC 3339 time by which this matters")]
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
        #[arg(
            long = "answer-ui",
            value_name = "PATH",
            help = "Send this attention-client.ui/1 answer document (requires --type request)"
        )]
        answer_ui_file: Option<PathBuf>,
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
    /// Replace this existing origin handle's transport and resume its saved feed.
    Reconnect {
        #[arg(long)]
        session: Uuid,
        #[arg(long, value_name = "ID|none", requires = "route")]
        vendor_session_id: Option<String>,
        #[arg(long, value_enum, requires = "vendor_session_id")]
        route: Option<Route>,
        #[arg(long, requires = "route")]
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
    #[command(
        after_help = "Only after a definite refusal (exit 2), correct the named input and submit a new revision. A prepared operation stays unchanged; use a new operation for the correction. For an unknown outcome (exit 7), retry only with `harklight-agent retry --op-id <KEY>` using the same key and unchanged inputs; do not submit a changed revision."
    )]
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
        #[arg(
            long = "type",
            value_name = "fyi|request",
            help = "Set the revised message type"
        )]
        message_type: Option<String>,
        #[arg(
            long = "kind",
            value_name = "blocker|success",
            help = "Set the revised message kind"
        )]
        message_kind: Option<String>,
        #[arg(long, help = "Add a free-text Reply action to a request")]
        reply: bool,
        #[arg(
            long,
            help = "Your importance estimate; Harklight decides the priority the recipient sees"
        )]
        importance: Option<String>,
        #[arg(long, hide = true)]
        urgency: Option<String>,
        #[arg(long, help = "RFC 3339 time by which this matters")]
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
        Err(error) => {
            if let Command::Reconnect { session, .. } = &cli.command {
                let safe = commands::reconnect_configuration_error(*session);
                fail(safe.message, safe.exit_code);
            }
            fail(error.to_string(), 1)
        }
    };

    let result = match cli.command {
        Command::Send {
            session,
            sender_id,
            display_name,
            title,
            body,
            choices,
            informational,
            message_type,
            message_kind,
            importance,
            urgency,
            reply,
            supersedes,
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
            answer_ui_file,
            prepare,
            execute,
            op_id,
        } => {
            for warning in send_cli_warnings(
                message_type.as_deref(),
                informational,
                urgency.as_deref(),
                execute,
            ) {
                eprintln!("{warning}");
            }
            commands::send_command_with_answer_ui_file(
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
                    message_type,
                    message_kind,
                    reply,
                    importance,
                    urgency,
                    by_when,
                    stake,
                    thread,
                    display_name,
                    default_choice,
                    default_at,
                    choice_styles,
                    clear_default: false,
                    answer_ui: None,
                },
                answer_ui_file.as_deref(),
            )
        }
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
        Command::Reconnect {
            session,
            vendor_session_id,
            route,
            controller,
        } => commands::reconnect(
            &context,
            session,
            vendor_session_id.as_deref(),
            route.map(Into::into),
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
            message_type,
            message_kind,
            reply,
            importance,
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
        } => {
            if urgency.is_some() {
                eprintln!("warning: --urgency is deprecated; use --importance");
            }
            commands::revise_with_fields(
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
                    message_type,
                    message_kind,
                    reply,
                    importance,
                    urgency,
                    by_when,
                    stake,
                    thread: None,
                    display_name: None,
                    default_choice,
                    default_at,
                    choice_styles,
                    clear_default,
                    answer_ui: None,
                },
            )
        }
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
    use clap::CommandFactory;

    #[test]
    fn send_warnings_cover_untyped_legacy_and_prepared_execution_cases() {
        assert_eq!(
            send_cli_warnings(None, false, None, false),
            vec![
                "warning: add --type request (or --type fyi); untyped sends will be refused by the Server"
            ]
        );
        assert!(send_cli_warnings(Some("request"), false, None, false).is_empty());
        assert!(send_cli_warnings(None, true, None, false).is_empty());
        assert_eq!(
            send_cli_warnings(Some("fyi"), false, Some("high"), false),
            vec!["warning: --urgency is deprecated; use --importance"]
        );
        assert!(send_cli_warnings(None, false, None, true).is_empty());
    }

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
            "--type",
            "request",
            "--kind",
            "blocker",
            "--importance",
            "high",
            "--reply",
            "--choice",
            "Approve",
            "--stake",
            "A one-week delay",
            "--by-when",
            "2026-11-01T17:00:00Z",
            "--answer-ui",
            "cover.json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Send {
                prepare: true,
                execute: false,
                op_id: Some(_),
                message_type: Some(message_type),
                message_kind: Some(message_kind),
                importance: Some(importance),
                reply: true,
                choices,
                stake: Some(stake),
                by_when: Some(by_when),
                answer_ui_file: Some(path),
                ..
            } if message_type == "request"
                && message_kind == "blocker"
                && importance == "high"
                && choices == vec!["Approve"]
                && stake == "A one-week delay"
                && by_when == "2026-11-01T17:00:00Z"
                && path == PathBuf::from("cover.json")
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
    fn send_accepts_an_arbitrary_display_name_string() {
        let display_name = "  Mei\n/💧  ";
        let cli = Cli::try_parse_from([
            "harklight-agent",
            "send",
            "--session",
            "new",
            "--sender-id",
            "app:example",
            "--display-name",
            display_name,
            "--title",
            "A title",
            "--route",
            "claude-code",
        ])
        .unwrap();

        assert!(matches!(
            cli.command,
            Command::Send {
                sender_id: Some(sender_id),
                display_name: Some(name),
                ..
            } if sender_id == "app:example" && name == display_name
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
    fn informational_send_is_parsed_as_the_fyi_compatibility_flag() {
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

        let with_choice = Cli::try_parse_from([
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
        .unwrap();
        assert!(matches!(
            with_choice.command,
            Command::Send {
                informational: true,
                choices,
                ..
            } if choices == vec!["Approve"]
        ));
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
            "--type",
            "request",
            "--kind",
            "blocker",
            "--reply",
            "--importance",
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
                message_type: Some(message_type),
                message_kind: Some(message_kind),
                reply: true,
                importance: Some(importance),
                urgency: None,
                by_when: Some(by_when),
                stake: Some(stake),
                thread: Some(thread),
                default_choice: Some(default_choice),
                default_at: Some(default_at),
                choice_styles,
                ..
            } if message_type == "request"
                && message_kind == "blocker"
                && importance == "high"
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
            "--type",
            "request",
            "--kind",
            "success",
            "--reply",
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
                message_type: Some(message_type),
                message_kind: Some(message_kind),
                reply: true,
                urgency: Some(urgency),
                importance: None,
                clear_default: true,
                choice_styles,
                ..
            } if message_type == "request"
                && message_kind == "success"
                && urgency == "now"
                && choice_styles == vec!["Ship=primary"]
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

    #[test]
    fn importance_is_visible_while_urgency_remains_a_hidden_alias() {
        let mut command = Cli::command();
        let send_help = command
            .find_subcommand_mut("send")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(send_help.contains("--importance"));
        assert!(!send_help.contains("--urgency"));
        assert!(send_help.contains("--answer-ui"));
        assert!(send_help.contains("--file cover-a.jpg --file cover-b.jpg"));

        let revise_help = command
            .find_subcommand_mut("revise")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(revise_help.contains("--importance"));
        assert!(!revise_help.contains("--urgency"));
        assert!(send_help.contains("priority the recipient sees"));
        assert!(revise_help.contains("priority the recipient sees"));
    }

    #[test]
    fn send_and_revise_help_explain_safe_correction_and_retry() {
        let mut command = Cli::command();
        let send_help = command
            .find_subcommand_mut("send")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(send_help.contains("Only after a definite refusal (exit 2)"));
        assert!(send_help.contains("submit a new send"));
        assert!(send_help.contains("unknown outcome (exit 7)"));
        assert!(send_help.contains("retry --op-id <KEY>"));

        let revise_help = command
            .find_subcommand_mut("revise")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(revise_help.contains("Only after a definite refusal (exit 2)"));
        assert!(revise_help.contains("submit a new revision"));
        assert!(revise_help.contains("unknown outcome (exit 7)"));
        assert!(revise_help.contains("retry --op-id <KEY>"));
    }
}
