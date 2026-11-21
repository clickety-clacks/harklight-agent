use std::path::PathBuf;

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use uuid::Uuid;

use harklight_agent::{
    commands::{self, CommandContext, MessageFieldOptions, ProgressReport},
    config::EnrolledIdentity,
    enrollment,
    v2::{AgentClient, V2Error},
};

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
    Enroll {
        #[command(subcommand)]
        action: EnrollCommand,
    },
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
        #[arg(
            long = "type",
            value_name = "fyi|request",
            required_unless_present_any = ["execute", "progress"],
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
        #[arg(long)]
        origin_listener: bool,
        #[arg(long = "file", action = clap::ArgAction::Append)]
        files: Vec<PathBuf>,
        #[arg(
            long = "answer-ui",
            value_name = "PATH",
            help = "Send this attention-client.ui/1 answer document (requires --type request)"
        )]
        answer_ui_file: Option<PathBuf>,
        #[arg(
            long,
            value_name = "PERCENT",
            value_parser = clap::value_parser!(u8).range(0..=100),
            requires = "progress_stage",
            help = "Send a Progress FYI at this percent (0-100); update it later with `progress`"
        )]
        progress: Option<u8>,
        #[arg(
            long,
            value_name = "LABEL",
            requires = "progress",
            help = "Current stage label of a Progress message"
        )]
        progress_stage: Option<String>,
        #[arg(
            long,
            value_name = "TEXT",
            requires = "progress",
            help = "Note for this Progress report"
        )]
        progress_note: Option<String>,
        #[arg(long, conflicts_with = "execute")]
        prepare: bool,
        #[arg(long, conflicts_with = "prepare")]
        execute: bool,
        #[arg(long)]
        op_id: Option<Uuid>,
    },
    #[command(
        after_help = "Updates a Progress message in place under the same message ID. Give only the new percent, the current stage and an optional note: the CLI builds the bar's segments from the message's current revision. Raising the percent adds width to the stage, the same percent adds a zero-width visit (a stage change or note), and lowering it trims the newest widths first while keeping earlier notes.\n\nFor an unknown outcome (exit 7), retry only with `harklight-agent retry --op-id <KEY>`; do not run a changed update. If the update is refused as stale, run `harklight-agent read --session <UUID>` to pick up the current revision, then run the update again."
    )]
    Progress {
        #[arg(long)]
        session: Uuid,
        #[arg(
            long,
            value_name = "MESSAGE_ID",
            help = "The Progress message to update"
        )]
        request: String,
        #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
        percent: u8,
        #[arg(long, value_name = "LABEL")]
        stage: String,
        #[arg(long, value_name = "TEXT")]
        note: Option<String>,
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
        #[arg(long)]
        origin_listener: bool,
        #[arg(
            long,
            help = "Declare that this controller sends system ACK2 for this handle"
        )]
        ack2: bool,
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
    /// Move an existing supported route-other handle under the origin listener.
    Cover {
        #[arg(long)]
        session: Uuid,
    },
    Read {
        #[arg(long)]
        session: Uuid,
    },
    /// Read an authoritative sent message and its file references.
    #[command(group(ArgGroup::new("message-ref").required(true).args(["item_id", "sender_id"])))]
    MessageGet {
        #[arg(long)]
        item_id: Option<String>,
        #[arg(long, requires = "message_id")]
        sender_id: Option<String>,
        #[arg(long, requires = "sender_id")]
        message_id: Option<String>,
        #[arg(long, requires = "sender_id")]
        revision: Option<u64>,
    },
    /// List sent messages, newest first.
    Messages {
        #[arg(long)]
        session: Option<Uuid>,
        #[arg(long)]
        page: Option<String>,
    },
    /// Read a page of entries for a sent message.
    MessageEntries {
        #[arg(long)]
        item_id: String,
        #[arg(long)]
        before: Option<String>,
        #[arg(long)]
        limit: Option<u64>,
    },
    /// Read the latest reported focus, or follow focus changes.
    Focus {
        #[arg(long)]
        follow: bool,
    },
    /// Ask a Harklight client to open a message or its chat.
    Show {
        #[arg(long)]
        sender_id: String,
        #[arg(long)]
        message_id: String,
        #[arg(long)]
        chat: bool,
        #[arg(long, conflicts_with = "client")]
        device: Option<String>,
        #[arg(long, conflicts_with = "device")]
        client: Option<String>,
    },
    Ack {
        #[arg(long)]
        session: Uuid,
        #[arg(
            long,
            required_unless_present = "advisory",
            conflicts_with = "advisory"
        )]
        event: Option<String>,
        #[arg(long, required_unless_present = "event", conflicts_with = "event")]
        advisory: Option<Uuid>,
    },
    #[command(
        about = "Record a receiving-system ACK2 outcome for a person entry",
        after_help = "This system-level processing receipt is separate from the agent Seen command `ack`. For an unknown outcome (exit 7), retry only with `harklight-agent retry --op-id <KEY>` using the same saved operation key."
    )]
    Ack2 {
        #[arg(long)]
        session: Uuid,
        #[arg(long)]
        event: String,
        #[arg(
            long,
            help = "Human-readable name of the agent that received the entry"
        )]
        agent: Option<String>,
        #[arg(long, conflicts_with = "not_handed")]
        rerouted: bool,
        #[arg(
            long = "not-handed",
            value_name = "REASON",
            num_args = 0..=1,
            default_missing_value = "",
            conflicts_with = "rerouted"
        )]
        not_handed: Option<String>,
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
    #[command(
        about = "Give up an operation stuck with an unknown outcome",
        after_help = "Marks the unresolved operation abandoned so it stops blocking new operations for its request. The Server may or may not have applied it. An abandoned operation can no longer be retried; a committed or refused operation cannot be abandoned."
    )]
    Abandon {
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

#[derive(Debug, Subcommand)]
enum EnrollCommand {
    Start(EnrollStartArgs),
    Retry(EnrollStartArgs),
    Status(EnrollStatusArgs),
}

#[derive(Debug, Args)]
struct EnrollEndpointArgs {
    #[arg(long)]
    connect: Option<String>,
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    port: Option<String>,
}

#[derive(Debug, Args)]
struct EnrollStartArgs {
    #[command(flatten)]
    endpoint: EnrollEndpointArgs,
    #[arg(long)]
    label: Option<String>,
}

#[derive(Debug, Args)]
struct EnrollStatusArgs {
    #[command(flatten)]
    endpoint: EnrollEndpointArgs,
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("mode").required(true).multiple(false).args(["follow", "next"]))) ]
struct ListenArgs {
    #[arg(long, required_unless_present = "origin", conflicts_with = "origin")]
    session: Option<Uuid>,
    #[arg(long, requires = "follow", conflicts_with_all = ["next", "controller", "ack2", "session"])]
    origin: bool,
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
    #[arg(
        long,
        help = "Declare that this controller sends system ACK2 for this handle"
    )]
    ack2: bool,
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
    if let Command::Enroll { action } = &cli.command {
        let result = match action {
            EnrollCommand::Start(args) => enrollment::start(
                cli.identity_dir.as_deref(),
                cli.ca_file.as_deref(),
                args.endpoint.connect.as_deref(),
                args.endpoint.name.as_deref(),
                args.endpoint.port.as_deref(),
                args.label.as_deref(),
                cli.origin_port,
            ),
            EnrollCommand::Retry(args) => enrollment::retry(
                cli.identity_dir.as_deref(),
                cli.ca_file.as_deref(),
                args.endpoint.connect.as_deref(),
                args.endpoint.name.as_deref(),
                args.endpoint.port.as_deref(),
                args.label.as_deref(),
            ),
            EnrollCommand::Status(args) => enrollment::status(
                cli.identity_dir.as_deref(),
                cli.ca_file.as_deref(),
                args.endpoint.connect.as_deref(),
                args.endpoint.name.as_deref(),
                args.endpoint.port.as_deref(),
            ),
        };
        if let Err(error) = result {
            fail(error.message, error.exit_code);
        }
        return;
    }

    if matches!(
        cli.command,
        Command::MessageGet { revision: None, .. }
            | Command::Messages { .. }
            | Command::MessageEntries { .. }
    ) {
        run_device_read(&cli);
        return;
    }

    let context = match CommandContext::load(
        cli.identity_dir.as_deref(),
        cli.ca_file.as_deref(),
        cli.origin_port,
        cli.state_dir.as_deref(),
    ) {
        Ok(context) => context,
        Err(error) => {
            if let Command::Reconnect { session, .. } = &cli.command {
                let safe = commands::reconnect_configuration_error(*session, "reconnect");
                fail(safe.message, safe.exit_code);
            }
            if let Command::Attach {
                session,
                controller: true,
                ..
            } = &cli.command
            {
                let safe = commands::reconnect_configuration_error(*session, "attach");
                fail(safe.message, safe.exit_code);
            }
            fail(error.to_string(), 1)
        }
    };

    let result = match cli.command {
        Command::Enroll { .. } => unreachable!("enroll was dispatched before loading an identity"),
        Command::Send {
            session,
            sender_id,
            display_name,
            title,
            body,
            choices,
            message_type,
            message_kind,
            importance,
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
            origin_listener,
            files,
            answer_ui_file,
            progress,
            progress_stage,
            progress_note,
            prepare,
            execute,
            op_id,
        } => {
            let progress = match (progress, progress_stage) {
                (Some(percent), Some(stage)) => Some(ProgressReport {
                    percent,
                    stage,
                    note: progress_note,
                }),
                _ => None,
            };
            let message_type = if progress.is_some() && message_type.is_none() {
                Some("fyi".to_string())
            } else {
                message_type
            };
            commands::send_command_with_answer_ui_file(
                &context,
                session.as_deref(),
                sender_id.as_deref(),
                title.as_deref(),
                body.as_deref(),
                &choices,
                &supersedes,
                vendor_session_id.as_deref(),
                route.map(Into::into),
                controller,
                &files,
                prepare,
                execute,
                op_id,
                &MessageFieldOptions {
                    origin_listener,
                    message_type,
                    message_kind,
                    reply,
                    importance,
                    by_when,
                    stake,
                    thread,
                    display_name,
                    default_choice,
                    default_at,
                    choice_styles,
                    clear_default: false,
                    answer_ui: None,
                    progress,
                },
                answer_ui_file.as_deref(),
            )
        }
        Command::Attach {
            session,
            vendor_session_id,
            route,
            controller,
            origin_listener,
            ack2,
        } => commands::attach_with_origin(
            &context,
            session,
            &vendor_session_id,
            route.into(),
            controller,
            ack2,
            origin_listener,
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
        Command::Cover { session } => commands::cover(&context, session),
        Command::Listen(args) if args.origin => commands::listen_origin(&context),
        Command::Listen(args) => commands::listen(
            &context,
            args.session.expect("session required without --origin"),
            args.session_context.map(Into::into),
            args.claude_code_version.as_deref(),
            args.background_tasks_enabled,
            args.follow,
            args.controller,
            args.ack2,
        ),
        Command::Read { session } => commands::read(&context, session),
        Command::MessageGet {
            sender_id: Some(sender_id),
            message_id: Some(message_id),
            revision: Some(revision),
            ..
        } => commands::message_get(&context, &sender_id, &message_id, Some(revision)),
        Command::MessageGet { .. } | Command::Messages { .. } | Command::MessageEntries { .. } => {
            unreachable!("device reads were dispatched before loading the origin context")
        }
        Command::Focus { follow } => commands::focus(&context, follow),
        Command::Show {
            sender_id,
            message_id,
            chat,
            device,
            client,
        } => commands::show(
            &context,
            &sender_id,
            &message_id,
            chat,
            device.as_deref(),
            client.as_deref(),
        ),
        Command::Ack {
            session,
            event,
            advisory,
        } => match (event, advisory) {
            (Some(event), None) => commands::ack(&context, session, &event),
            (None, Some(advisory)) => commands::ack_advisory(&context, session, advisory),
            _ => unreachable!("clap requires exactly one acknowledgment target"),
        },
        Command::Ack2 {
            session,
            event,
            agent,
            rerouted,
            not_handed,
        } => commands::ack2(
            &context,
            session,
            &event,
            agent.as_deref(),
            rerouted,
            not_handed.as_deref(),
        ),
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
                origin_listener: false,
                message_type,
                message_kind,
                reply,
                importance,
                by_when,
                stake,
                thread: None,
                display_name: None,
                default_choice,
                default_at,
                choice_styles,
                clear_default,
                answer_ui: None,
                progress: None,
            },
        ),
        Command::Progress {
            session,
            request,
            percent,
            stage,
            note,
            op_id,
        } => commands::progress_update(
            &context,
            session,
            &request,
            &ProgressReport {
                percent,
                stage,
                note,
            },
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
        Command::Abandon { op_id } => commands::abandon(&context, op_id),
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

fn run_device_read(cli: &Cli) {
    let identity_dir = cli
        .identity_dir
        .as_deref()
        .unwrap_or_else(|| fail("provide --identity-dir or HARKLIGHT_IDENTITY_DIR".into(), 1));
    let ca_file = cli
        .ca_file
        .as_deref()
        .unwrap_or_else(|| fail("provide --ca-file or HARKLIGHT_CA_FILE".into(), 1));
    let client = EnrolledIdentity::load(identity_dir, ca_file)
        .and_then(AgentClient::new)
        .unwrap_or_else(|error| fail(error.to_string(), 1));

    let (operation, result) = match &cli.command {
        Command::MessageGet {
            item_id: Some(id), ..
        } => ("message-get", client.message(id)),
        Command::MessageGet {
            sender_id: Some(sender),
            message_id: Some(id),
            ..
        } => (
            "message-get",
            client.legacy_message(sender, id).and_then(|message| {
                message.ok_or_else(|| V2Error::Http {
                    status: 404,
                    token: Some("not_found".into()),
                    field: None,
                    request_id: None,
                    retry_after: None,
                })
            }),
        ),
        Command::Messages { session, page } => {
            ("messages", client.messages(*session, page.as_deref()))
        }
        Command::MessageEntries {
            item_id,
            before,
            limit,
        } => (
            "message-entries",
            client.message_entries(item_id, before.as_deref(), *limit),
        ),
        _ => unreachable!("clap requires a complete message selector"),
    };
    match result {
        Ok(response) => println!("{response}"),
        Err(error @ V2Error::Http { status, .. }) if (400..500).contains(&status) => {
            if let V2Error::Http {
                ref token,
                ref field,
                ref request_id,
                ..
            } = error
            {
                let mut output = serde_json::json!({
                    "kind":"refused", "operation":operation, "status":status, "error":token,
                });
                if let Some(field) = field {
                    output["field"] = field.as_str().into();
                }
                if let Some(request_id) = request_id {
                    output["request_id"] = request_id.as_str().into();
                }
                println!("{output}");
            }
            fail(error.to_string(), commands::EXIT_REFUSED)
        }
        Err(error) => fail(error.to_string(), 1),
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
    fn ack_accepts_exactly_one_ordinary_event_or_advisory_id() {
        let session = "123e4567-e89b-42d3-a456-426614174000";
        let advisory = "123e4567-e89b-42d3-a456-426614174001";
        assert!(
            Cli::try_parse_from([
                "harklight-agent",
                "ack",
                "--session",
                session,
                "--event",
                "17"
            ])
            .is_ok()
        );
        let cli = Cli::try_parse_from([
            "harklight-agent",
            "ack",
            "--session",
            session,
            "--advisory",
            advisory,
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Ack {
                event: None,
                advisory: Some(_),
                ..
            }
        ));
        assert!(Cli::try_parse_from(["harklight-agent", "ack", "--session", session]).is_err());
        assert!(
            Cli::try_parse_from([
                "harklight-agent",
                "ack",
                "--session",
                session,
                "--event",
                "17",
                "--advisory",
                advisory,
            ])
            .is_err()
        );
    }

    #[test]
    fn origin_listener_options_are_exclusive_before_configuration_or_network() {
        assert!(Cli::try_parse_from(["agent", "listen", "--origin", "--follow"]).is_ok());
        for extra in ["--controller", "--ack2", "--next"] {
            assert!(
                Cli::try_parse_from(["agent", "listen", "--origin", "--follow", extra]).is_err()
            );
        }
        assert!(Cli::try_parse_from(["agent", "listen", "--origin"]).is_err());
        assert!(
            Cli::try_parse_from([
                "agent",
                "listen",
                "--origin",
                "--follow",
                "--session",
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
            ])
            .is_err()
        );
    }

    #[test]
    fn reconnect_accepts_saved_or_complete_attachment_inputs() {
        let handle = "123e4567-e89b-42d3-a456-426614174000";
        let cli =
            Cli::try_parse_from(["harklight-agent", "reconnect", "--session", handle]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Reconnect {
                vendor_session_id: None,
                route: None,
                controller: false,
                ..
            }
        ));
        let cli = Cli::try_parse_from([
            "harklight-agent",
            "reconnect",
            "--session",
            handle,
            "--vendor-session-id",
            "none",
            "--route",
            "other",
            "--controller",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Reconnect {
                vendor_session_id: Some(_),
                route: Some(Route::Other),
                controller: true,
                ..
            }
        ));
        assert!(
            Cli::try_parse_from([
                "harklight-agent",
                "reconnect",
                "--session",
                handle,
                "--vendor-session-id",
                "none"
            ])
            .is_err()
        );
    }

    #[test]
    fn enrollment_cli_keeps_required_inputs_for_local_validation() {
        assert!(
            Cli::try_parse_from([
                "harklight-agent",
                "enroll",
                "start",
                "--connect",
                "192.0.2.7",
                "--name",
                "harklight.example",
                "--label",
                "Work laptop"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "harklight-agent",
                "enroll",
                "start",
                "--identity-dir",
                "/tmp/harklight-test-identity",
                "--ca-file",
                "/tmp/harklight-test-ca.pem",
                "--connect",
                "harklight.example",
                "--name",
                "harklight.example",
                "--port",
                "18444",
                "--origin-port",
                "18450",
                "--label",
                "Work laptop"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "harklight-agent",
                "enroll",
                "status",
                "--connect",
                "harklight.example",
                "--name",
                "harklight.example",
                "--port",
                "18444",
                "--label",
                "not-accepted"
            ])
            .is_err()
        );
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
    fn send_accepts_a_first_progress_report() {
        let cli = Cli::try_parse_from([
            "harklight-agent",
            "send",
            "--session",
            "new",
            "--sender-id",
            "app:example",
            "--route",
            "claude-code",
            "--title",
            "Ship the release",
            "--progress",
            "10",
            "--progress-stage",
            "spec_writing",
            "--progress-note",
            "Scope agreed",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Send {
                progress: Some(10),
                progress_stage: Some(stage),
                progress_note: Some(note),
                ..
            } if stage == "spec_writing" && note == "Scope agreed"
        ));
    }

    #[test]
    fn progress_inputs_are_limited_to_a_percent_stage_and_note() {
        let parse = |args: &[&str]| {
            let mut all = vec!["harklight-agent"];
            all.extend_from_slice(args);
            Cli::try_parse_from(all)
        };
        let session = Uuid::new_v4().to_string();
        let cli = parse(&[
            "progress",
            "--session",
            &session,
            "--request",
            "progress-1",
            "--percent",
            "45",
            "--stage",
            "testing",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Progress { percent: 45, stage, note: None, .. } if stage == "testing"
        ));
        assert!(
            parse(&[
                "progress",
                "--session",
                &session,
                "--request",
                "progress-1",
                "--percent",
                "101",
                "--stage",
                "testing",
            ])
            .is_err()
        );
        assert!(
            parse(&[
                "send",
                "--session",
                "new",
                "--sender-id",
                "app:example",
                "--title",
                "T",
                "--progress",
                "10",
            ])
            .is_err(),
            "--progress needs --progress-stage"
        );
        assert!(
            parse(&[
                "send",
                "--session",
                "new",
                "--sender-id",
                "app:example",
                "--title",
                "T",
                "--progress-stage",
                "coding",
            ])
            .is_err(),
            "--progress-stage needs --progress"
        );
    }

    #[test]
    fn send_requires_a_type_unless_executing_or_reporting_progress() {
        let base = [
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
        ];
        assert!(Cli::try_parse_from(base).is_err());
        assert!(Cli::try_parse_from(base.into_iter().chain(["--type", "fyi"])).is_ok());
        assert!(
            Cli::try_parse_from(base.into_iter().chain([
                "--progress",
                "10",
                "--progress-stage",
                "coding"
            ]))
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "harklight-agent",
                "send",
                "--execute",
                "--op-id",
                "123e4567-e89b-42d3-a456-426614174000"
            ])
            .is_ok()
        );
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
            "--type",
            "fyi",
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
            "--type",
            "fyi",
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
    fn message_get_parses_exact_identity_and_optional_revision() {
        let cli = Cli::try_parse_from([
            "harklight-agent",
            "message-get",
            "--sender-id",
            "app:example",
            "--message-id",
            "message-1",
            "--revision",
            "3",
        ])
        .unwrap();

        assert!(matches!(
            cli.command,
            Command::MessageGet {
                sender_id: Some(sender_id),
                message_id: Some(message_id),
                revision: Some(3),
                ..
            } if sender_id == "app:example" && message_id == "message-1"
        ));

        assert!(
            Cli::try_parse_from([
                "harklight-agent",
                "message-get",
                "--sender-id",
                "app:example",
            ])
            .is_err()
        );
    }

    #[test]
    fn device_message_reads_accept_opaque_ids_and_reject_mixed_selectors() {
        let cli =
            Cli::try_parse_from(["agent", "message-get", "--item-id", "opaque/item"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::MessageGet { item_id: Some(id), revision: None, .. } if id == "opaque/item"
        ));
        assert!(
            Cli::try_parse_from([
                "agent",
                "message-get",
                "--item-id",
                "opaque/item",
                "--sender-id",
                "app:example",
                "--message-id",
                "local"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["agent", "messages", "--page", "opaque+next"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "agent",
                "message-entries",
                "--item-id",
                "opaque/item",
                "--before",
                "opaque+entry"
            ])
            .is_ok()
        );
    }

    #[test]
    fn focus_and_show_parse_their_contract_arguments() {
        let focus = Cli::try_parse_from(["harklight-agent", "focus", "--follow"]).unwrap();
        assert!(matches!(focus.command, Command::Focus { follow: true }));

        let show = Cli::try_parse_from([
            "harklight-agent",
            "show",
            "--sender-id",
            "app:example",
            "--message-id",
            "message-1",
            "--chat",
            "--device",
            "Owner's iPhone",
        ])
        .unwrap();
        assert!(matches!(
            show.command,
            Command::Show {
                sender_id,
                message_id,
                chat: true,
                device: Some(device),
                client: None,
            } if sender_id == "app:example"
                && message_id == "message-1"
                && device == "Owner's iPhone"
        ));

        assert!(
            Cli::try_parse_from([
                "harklight-agent",
                "show",
                "--sender-id",
                "app:example",
                "--message-id",
                "message-1",
                "--device",
                "iPhone",
                "--client",
                "a".repeat(64).as_str(),
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
            "--importance",
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
                importance: Some(importance),
                clear_default: true,
                choice_styles,
                ..
            } if message_type == "request"
                && message_kind == "success"
                && importance == "now"
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
    fn importance_is_visible_on_send_and_revise() {
        let mut command = Cli::command();
        let send_help = command
            .find_subcommand_mut("send")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(send_help.contains("--importance"));
        assert!(send_help.contains("--answer-ui"));
        assert!(send_help.contains("--file cover-a.jpg --file cover-b.jpg"));

        let revise_help = command
            .find_subcommand_mut("revise")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(revise_help.contains("--importance"));
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

    #[test]
    fn ack2_is_a_separate_receiver_command_and_controller_capability_is_declared_by_flag() {
        let handle = "123e4567-e89b-42d3-a456-426614174000";
        let attach = Cli::try_parse_from([
            "harklight-agent",
            "attach",
            "--session",
            handle,
            "--vendor-session-id",
            "none",
            "--route",
            "other",
            "--controller",
            "--ack2",
        ])
        .unwrap();
        assert!(matches!(
            attach.command,
            Command::Attach {
                controller: true,
                ack2: true,
                ..
            }
        ));

        let listen = Cli::try_parse_from([
            "harklight-agent",
            "listen",
            "--session",
            handle,
            "--follow",
            "--controller",
            "--ack2",
        ])
        .unwrap();
        assert!(matches!(
            listen.command,
            Command::Listen(ListenArgs {
                controller: true,
                ack2: true,
                ..
            })
        ));

        let handed = Cli::try_parse_from([
            "harklight-agent",
            "ack2",
            "--session",
            handle,
            "--event",
            "812",
            "--agent",
            "Studio",
            "--rerouted",
        ])
        .unwrap();
        assert!(matches!(
            handed.command,
            Command::Ack2 {
                agent: Some(agent),
                rerouted: true,
                not_handed: None,
                ..
            } if agent == "Studio"
        ));
        let not_handed = Cli::try_parse_from([
            "harklight-agent",
            "ack2",
            "--session",
            handle,
            "--event",
            "812",
            "--not-handed",
            "The receiver could not hand off this entry.",
        ])
        .unwrap();
        assert!(matches!(
            not_handed.command,
            Command::Ack2 {
                agent: None,
                rerouted: false,
                not_handed: Some(reason),
                ..
            } if reason == "The receiver could not hand off this entry."
        ));
        assert!(
            Cli::try_parse_from([
                "harklight-agent",
                "ack2",
                "--session",
                handle,
                "--event",
                "812",
                "--rerouted",
                "--not-handed",
                "not available",
            ])
            .is_err()
        );

        let mut command = Cli::command();
        let ack2_help = command
            .find_subcommand_mut("ack2")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(ack2_help.contains("receiving-system ACK2"));
        assert!(ack2_help.contains("separate from the agent Seen command `ack`"));
    }
}
