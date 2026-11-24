use std::path::PathBuf;

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use uuid::Uuid;

use harklight_agent::{
    commands::{self, MessageFieldOptions, ProgressReport},
    config::{EnrolledIdentity, default_state_root},
    enrollment,
    journal::JournalStore,
    v2::{AgentClient, V2Error, feed, mutations, sends, writes},
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
    /// Device API port, separate from the enrollment endpoint.
    #[arg(long, global = true, env = "HARKLIGHT_DEVICE_PORT", value_parser = clap::value_parser!(u16).range(1..))]
    device_port: Option<u16>,
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
        after_help = "For picture choices, provide an attention-client.ui/1 document and attach its images with --file:\n  harklight-agent send --session new --sender-id app:example --type request --title \"Choose the cover\" --answer-ui cover.json --file cover-a.jpg --file cover-b.jpg\n\nOnly after a definite refusal (exit 2), correct the named input and submit a new send. A prepared operation stays unchanged; use a new operation for the correction. For an unknown outcome (exit 7), retry only with `harklight-agent retry --op-id <KEY>` using the same key and unchanged inputs; do not submit a changed send."
    )]
    Send {
        #[arg(long, value_name = "UUID|new")]
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
        /// Resolve a same-sender local reference before sending.
        #[arg(long = "supersedes-local")]
        supersedes_local: Vec<String>,
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
        #[arg(long, requires = "op_id", conflicts_with_all = [
            "prepare", "session", "sender_id", "display_name", "title", "body", "choices",
            "message_type", "message_kind", "importance", "reply", "supersedes", "supersedes_local", "by_when",
            "stake", "thread", "default_choice", "default_at", "choice_styles", "files",
            "answer_ui_file", "progress", "progress_stage", "progress_note"
        ])]
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
        /// Resolve the request as a pre-cut-over message belonging to this sender.
        #[arg(long)]
        sender_id: Option<String>,
        #[arg(long)]
        op_id: Option<Uuid>,
    },
    Listen(ListenArgs),
    Read(ReadArgs),
    /// Read an authoritative sent message and its file references.
    #[command(group(ArgGroup::new("message-ref").required(true).args(["item_id", "sender_id"])))]
    MessageGet {
        #[arg(long)]
        item_id: Option<String>,
        #[arg(long, requires = "message_id")]
        sender_id: Option<String>,
        #[arg(long, requires = "sender_id")]
        message_id: Option<String>,
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
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
        item_id: String,
        #[arg(long)]
        chat: bool,
        #[arg(long, conflicts_with = "device_id")]
        device: Option<String>,
        #[arg(long)]
        device_id: Option<String>,
        #[arg(long)]
        op_id: Option<Uuid>,
    },
    /// Acknowledge an advisory delivered on the change feed.
    AdvisoryAck {
        #[arg(long)]
        item_id: String,
        #[arg(long)]
        advisory_id: String,
        #[arg(long)]
        op_id: Option<Uuid>,
    },
    /// Report handoff of an answer or a person chat entry.
    Receipt {
        #[arg(long)]
        item_id: String,
        #[arg(long)]
        entry_id: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, conflicts_with = "not_handed")]
        rerouted: bool,
        #[arg(long, value_name = "REASON")]
        not_handed: Option<String>,
        #[arg(long)]
        op_id: Option<Uuid>,
    },
    Reply {
        #[arg(long)]
        item_id: String,
        #[arg(long)]
        in_reply_to: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long = "file-id")]
        file_ids: Vec<String>,
        #[arg(long = "file")]
        files: Vec<PathBuf>,
        #[arg(long)]
        op_id: Option<Uuid>,
    },
    #[command(
        after_help = "Only after a definite refusal (exit 2), correct the named input and submit a new revision. A prepared operation stays unchanged; use a new operation for the correction. For an unknown outcome (exit 7), retry only with `harklight-agent retry --op-id <KEY>` using the same key and unchanged inputs; do not submit a changed revision."
    )]
    Revise {
        #[arg(long)]
        item_id: String,
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        base_revision: u64,
        #[arg(long, required_unless_present = "message_json")]
        title: Option<String>,
        #[arg(long, conflicts_with_all = [
            "title", "body", "choices", "clear_choices", "message_type", "message_kind",
            "reply", "importance", "by_when", "stake", "default_choice", "default_at",
            "clear_default", "choice_styles"
        ])]
        message_json: Option<PathBuf>,
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
        #[arg(long = "file-id")]
        file_ids: Vec<String>,
        #[arg(long, conflicts_with_all = ["file_ids","files"])]
        clear_files: bool,
        #[arg(long = "file", action = clap::ArgAction::Append)]
        files: Vec<PathBuf>,
        #[arg(long)]
        op_id: Option<Uuid>,
    },
    Retract {
        #[arg(long)]
        item_id: String,
        #[arg(long)]
        in_reply_to: Option<String>,
        #[arg(long)]
        op_id: Option<Uuid>,
    },
    Lifecycle {
        #[arg(long)]
        item_id: String,
        #[arg(long, value_enum)]
        action: LifecycleAction,
        #[arg(long)]
        op_id: Option<Uuid>,
    },
    Result {
        #[arg(long)]
        item_id: String,
        #[arg(long, value_enum)]
        status: ResultStatus,
        #[arg(long)]
        op_id: Option<Uuid>,
    },
    /// Upload a file for use in a message or reply.
    FileUpload {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        op_id: Option<Uuid>,
    },
    /// Verify and save a file from a message or chat entry.
    FileGet {
        #[arg(long)]
        file_id: String,
        #[arg(long)]
        output: PathBuf,
    },
    DiscardUploads {
        #[arg(long)]
        file_id: String,
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
        #[arg(long)]
        journal_id: Option<Uuid>,
    },
    /// Print the persistent ID of this operation journal.
    JournalId,
    #[command(
        about = "Give up an operation stuck with an unknown outcome",
        after_help = "Marks the unresolved operation abandoned so it stops blocking new operations for its request. The Server may or may not have applied it. An abandoned operation can no longer be retried; a committed or refused operation cannot be abandoned."
    )]
    Abandon {
        #[arg(long)]
        op_id: Uuid,
    },
    /// Report an attachment transfer, use, or both.
    #[command(group(ArgGroup::new("file-outcome").required(true).multiple(true)
        .args(["used","unusable","transferred","not_transferred"])))]
    FileUse {
        #[arg(long)]
        item_id: String,
        #[arg(long)]
        entry_id: String,
        #[arg(long)]
        file_id: String,
        #[arg(long, conflicts_with = "unusable")]
        used: bool,
        #[arg(long, requires = "reason")]
        unusable: bool,
        #[arg(long, conflicts_with = "not_transferred")]
        transferred: bool,
        #[arg(long, requires = "reason")]
        not_transferred: bool,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long)]
        op_id: Option<Uuid>,
    },
}

#[derive(Debug, Subcommand)]
enum EnrollCommand {
    Start {
        #[command(flatten)]
        manual: EnrollStartArgs,
        /// Private JSON invitation file or file containing a native join link.
        #[arg(long, conflicts_with_all = ["connect", "name", "port"])]
        invitation: Option<PathBuf>,
    },
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
struct FeedArgs {
    /// Take waiting work for this session only.
    #[arg(long, value_name = "UUID")]
    session: Option<Uuid>,
    /// Take every session's waiting work.
    #[arg(long)]
    all_sessions: bool,
    #[arg(long, conflicts_with = "snapshot")]
    after: Option<u64>,
    #[arg(long)]
    snapshot: bool,
    /// Declare that this consumer issues handoff receipts.
    #[arg(long)]
    ack2: bool,
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("pending_action").args(["prepare", "resume"])))]
struct ReadArgs {
    #[command(flatten)]
    feed: FeedArgs,
    /// Read a caller-owned pending baseline without opening the Agent journal.
    #[arg(long, requires_all = ["session", "pending_action"], conflicts_with_all = ["snapshot", "after"])]
    pending: bool,
    #[arg(long, requires = "pending")]
    prepare: bool,
    #[arg(long, requires = "pending", value_name = "TOKEN")]
    resume: Option<String>,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum OnExpired {
    Exit,
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("mode").args(["follow", "next"])))]
struct ListenArgs {
    #[command(flatten)]
    feed: FeedArgs,
    #[arg(long)]
    follow: bool,
    #[arg(long)]
    next: bool,
    #[arg(long, requires_all = ["follow", "session", "after"])]
    on_expired: Option<OnExpired>,
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    End {
        #[arg(long)]
        session: Uuid,
        #[arg(long)]
        op_id: Option<Uuid>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum LifecycleAction {
    Answered,
    Cleared,
    Expired,
    Superseded,
}

impl LifecycleAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Answered => "answered",
            Self::Cleared => "cleared",
            Self::Expired => "expired",
            Self::Superseded => "superseded",
        }
    }
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
            EnrollCommand::Start {
                manual,
                invitation: Some(path),
            } => enrollment::start_invitation(
                cli.identity_dir.as_deref(),
                path,
                manual.label.as_deref(),
            ),
            EnrollCommand::Start {
                manual: args,
                invitation: None,
            } => enrollment::start(
                cli.identity_dir.as_deref(),
                cli.ca_file.as_deref(),
                args.endpoint.connect.as_deref(),
                args.endpoint.name.as_deref(),
                args.endpoint.port.as_deref(),
                args.label.as_deref(),
                cli.device_port,
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

    match &cli.command {
        Command::Read(args) if args.pending => run_pending_read(&cli, args),
        Command::Read(args) => run_device_feed(&cli, Some(&args.feed), feed::Mode::Read),
        Command::Listen(args) => run_device_feed(
            &cli,
            Some(&args.feed),
            if args.next {
                feed::Mode::Next
            } else {
                feed::Mode::Follow
            },
        ),
        Command::Focus { follow: true } => run_device_feed(&cli, None, feed::Mode::Focus),
        Command::MessageGet { .. }
        | Command::Messages { .. }
        | Command::MessageEntries { .. }
        | Command::FileGet { .. }
        | Command::Focus { follow: false } => run_device_read(&cli),
        _ => run_device_write(&cli),
    }
}

fn run_pending_read(cli: &Cli, args: &ReadArgs) {
    use std::io::{self, Write};

    let session = args.feed.session.expect("pending read requires a session");
    let client = AgentClient::new(device_identity(cli))
        .unwrap_or_else(|error| fail(error.to_string(), 1))
        .with_ack2(args.feed.ack2);
    let mut stdout = io::stdout().lock();
    let result = feed::pending::run(&client, session, args.resume.as_deref(), |value| {
        writeln!(stdout, "{value}")?;
        stdout.flush()
    });
    if let Err(error) = result {
        if let Some(value) = error.machine_result() {
            writeln!(stdout, "{value}")
                .and_then(|_| stdout.flush())
                .unwrap_or_else(|error| fail(error.to_string(), 1));
            std::process::exit(error.exit_code());
        }
        fail(error.to_string(), error.exit_code());
    }
}

fn run_device_feed(cli: &Cli, args: Option<&FeedArgs>, mode: feed::Mode) {
    use std::io::{self, Write};

    if let Some(args) = args {
        if args.session.is_some() == args.all_sessions {
            fail(
                "Choose exactly one of --session UUID or --all-sessions. --all-sessions takes every session's waiting work.".into(),
                commands::EXIT_REFUSED,
            );
        }
        if let Command::Listen(listen) = &cli.command {
            if !listen.follow && !listen.next {
                fail("Choose --follow or --next.".into(), commands::EXIT_REFUSED);
            }
        }
    }

    let identity = device_identity(cli);
    let journal = device_journal(cli, &identity.device_id);
    let client = AgentClient::new(identity)
        .unwrap_or_else(|error| fail(error.to_string(), 1))
        .with_ack2(args.is_some_and(|args| args.ack2));
    let session = args.and_then(|args| args.session);
    let start = match args {
        Some(FeedArgs {
            after: Some(rev), ..
        }) => {
            if matches!(&cli.command, Command::Listen(args) if args.on_expired.is_some()) {
                feed::Start::AfterUntilExpired(*rev)
            } else {
                feed::Start::After(*rev)
            }
        }
        Some(FeedArgs { snapshot: true, .. }) => feed::Start::Snapshot,
        _ => feed::Start::Journal,
    };
    let failure_context = match mode {
        feed::Mode::Read => "read",
        feed::Mode::Focus => "focus: live updates paused",
        feed::Mode::Follow | feed::Mode::Next => "listen: live updates paused",
    };
    let mut stdout = io::stdout().lock();
    let result = feed::run(&client, &journal, session, start, mode, |value| {
        writeln!(stdout, "{value}")?;
        stdout.flush()
    });
    if let Err(error) = result {
        if let feed::FeedError::CursorExpired { rev } = error {
            let value = serde_json::json!({"kind":"cursor_expired","rev":rev});
            writeln!(stdout, "{value}")
                .and_then(|_| stdout.flush())
                .unwrap_or_else(|error| fail(error.to_string(), 1));
            std::process::exit(2);
        }
        if error.is_internal_error() {
            fail(format!("{failure_context}: {error}"), error.exit_code());
        }
        fail(error.to_string(), error.exit_code());
    }
}

fn device_identity(cli: &Cli) -> EnrolledIdentity {
    let identity_dir = cli
        .identity_dir
        .as_deref()
        .unwrap_or_else(|| fail("provide --identity-dir or HARKLIGHT_IDENTITY_DIR".into(), 1));
    let invitation_ca = identity_dir.join("invitation-ca.pem");
    let ca_file = cli
        .ca_file
        .as_deref()
        .or_else(|| invitation_ca.is_file().then_some(invitation_ca.as_path()))
        .unwrap_or_else(|| fail("provide --ca-file or HARKLIGHT_CA_FILE".into(), 1));
    let mut identity = EnrolledIdentity::load(identity_dir, ca_file)
        .unwrap_or_else(|error| fail(error.to_string(), 1));
    if let Some(device_port) = cli.device_port {
        identity.port = device_port;
    }
    identity
}

fn run_device_read(cli: &Cli) {
    let identity = device_identity(cli);
    let device_id = identity.device_id.clone();
    let client = AgentClient::new(identity).unwrap_or_else(|error| fail(error.to_string(), 1));
    let (operation, result) = match &cli.command {
        Command::MessageGet {
            item_id: Some(id),
            revision,
            ..
        } => ("message-get", read_message_revision(&client, id, *revision)),
        Command::MessageGet {
            sender_id: Some(sender),
            message_id: Some(id),
            revision,
            ..
        } => {
            let journal = device_journal(cli, &device_id);
            let cached = journal
                .v2_item_id(sender, id)
                .unwrap_or_else(|error| fail(error.to_string(), 1));
            let result = client
                .resolve_legacy_message(sender, id, cached.as_deref())
                .and_then(|message| {
                    let item_id = message["message"]["item_id"]
                        .as_str()
                        .ok_or(V2Error::InvalidResponse { request_id: None })?;
                    if cached.is_none() {
                        journal
                            .remember_v2_item(sender, id, item_id)
                            .unwrap_or_else(|error| fail(error.to_string(), 1));
                    }
                    match revision {
                        Some(revision) => client.message_revision(item_id, *revision),
                        None => Ok(message),
                    }
                });
            ("message-get", result)
        }
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
        Command::FileGet { file_id, output } => (
            "file-get",
            client.download_file(file_id).map(|mut file| {
                file.save(output).unwrap_or_else(|error| fail(error, 1));
                serde_json::json!({
                    "kind":"file_saved","file_id":file_id,
                    "size":file.size,"sha256":file.sha256,"path":output,
                })
            }),
        ),
        Command::Focus { follow: false } => ("focus", client.read(&["agent", "focus"], &[])),
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
            let exit = match &error {
                V2Error::Http {
                    token: Some(token), ..
                } if token == "unsupported" => 5,
                _ => commands::EXIT_REFUSED,
            };
            fail(error.to_string(), exit)
        }
        Err(error) if error.is_internal_error() => fail(format!("{operation}: {error}"), 1),
        Err(error) => fail(error.to_string(), 1),
    }
}

fn read_message_revision(
    client: &AgentClient,
    item_id: &str,
    revision: Option<u64>,
) -> Result<serde_json::Value, V2Error> {
    revision.map_or_else(
        || client.message(item_id),
        |rev| client.message_revision(item_id, rev),
    )
}

fn device_journal(cli: &Cli, device_id: &str) -> JournalStore {
    try_device_journal(cli, device_id).unwrap_or_else(|error| fail(error, 1))
}

fn try_device_journal(cli: &Cli, device_id: &str) -> Result<JournalStore, String> {
    let state_root = cli
        .state_dir
        .clone()
        .map(Ok)
        .unwrap_or_else(default_state_root)
        .map_err(|error| error.to_string())?;
    JournalStore::open(&state_root, device_id, Uuid::nil()).map_err(|error| error.to_string())
}

fn run_device_write(cli: &Cli) {
    use writes::ControlWrite;

    let identity = device_identity(cli);
    let mut journal = match try_device_journal(cli, &identity.device_id) {
        Ok(journal) => journal,
        Err(detail) => match &cli.command {
            Command::Retry { op_id, journal_id } => {
                let expected = journal_id.map(|id| id.to_string());
                write_failure(writes::unavailable_retry(
                    *op_id,
                    expected.as_deref(),
                    &detail,
                ))
            }
            _ => fail(detail, 1),
        },
    };
    let client = AgentClient::new(identity).unwrap_or_else(|error| fail(error.to_string(), 1));
    let result = match &cli.command {
        Command::Send {
            execute: true,
            op_id: Some(key),
            ..
        } => sends::execute(&client, &mut journal, *key),
        Command::Send {
            session,
            sender_id: Some(sender),
            display_name,
            title: Some(title),
            body,
            choices,
            message_type,
            message_kind,
            importance,
            reply,
            supersedes,
            supersedes_local,
            by_when,
            stake,
            thread,
            default_choice,
            default_at,
            choice_styles,
            files,
            answer_ui_file,
            progress,
            progress_stage,
            progress_note,
            prepare,
            op_id,
            ..
        } => {
            let progress = progress
                .zip(progress_stage.as_ref())
                .map(|(percent, stage)| ProgressReport {
                    percent,
                    stage: stage.clone(),
                    note: progress_note.clone(),
                });
            let fields = MessageFieldOptions {
                message_type: message_type
                    .clone()
                    .or_else(|| progress.as_ref().map(|_| "fyi".into())),
                message_kind: message_kind.clone(),
                reply: *reply,
                importance: importance.clone(),
                by_when: by_when.clone(),
                stake: stake.clone(),
                thread: thread.clone(),
                display_name: display_name.clone(),
                default_choice: default_choice.clone(),
                default_at: default_at.clone(),
                choice_styles: choice_styles.clone(),
                progress,
                ..MessageFieldOptions::default()
            };
            sends::send(
                &client,
                &mut journal,
                &sends::SendInput {
                    session: session.as_deref(),
                    sender,
                    title,
                    body: body.as_deref(),
                    choices,
                    supersedes,
                    supersedes_local,
                    files,
                    fields: &fields,
                    answer_ui: answer_ui_file.as_deref(),
                },
                *op_id,
                *prepare,
            )
        }
        Command::Reply {
            item_id,
            in_reply_to,
            body,
            file_ids,
            files,
            op_id,
        } => mutations::reply(
            &client,
            &mut journal,
            item_id,
            body.as_deref(),
            in_reply_to.as_deref(),
            file_ids,
            files,
            *op_id,
        ),
        Command::Revise {
            item_id,
            base_revision,
            message_json: Some(path),
            file_ids,
            clear_files,
            files,
            in_reply_to,
            op_id,
            ..
        } => {
            let message = std::fs::read(path)
                .map_err(|error| format!("could not read revision JSON: {error}"))
                .and_then(|bytes| {
                    serde_json::from_slice(&bytes)
                        .map_err(|error| format!("invalid revision JSON: {error}"))
                })
                .unwrap_or_else(|error| fail(error, 1));
            mutations::revise(
                &client,
                &mut journal,
                mutations::Revision {
                    item_id,
                    revision: *base_revision,
                    message,
                    file_ids: (*clear_files || !file_ids.is_empty()).then_some(file_ids.as_slice()),
                    files,
                    in_reply_to: in_reply_to.as_deref(),
                    operation: "revise",
                    session: None,
                },
                *op_id,
            )
        }
        Command::Revise {
            item_id,
            base_revision,
            title: Some(title),
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
            file_ids,
            clear_files,
            files,
            in_reply_to,
            op_id,
            ..
        } => {
            let fields = MessageFieldOptions {
                message_type: message_type.clone(),
                message_kind: message_kind.clone(),
                reply: *reply,
                importance: importance.clone(),
                by_when: by_when.clone(),
                stake: stake.clone(),
                default_choice: default_choice.clone(),
                default_at: default_at.clone(),
                clear_default: *clear_default,
                choice_styles: choice_styles.clone(),
                ..MessageFieldOptions::default()
            };
            mutations::revise_interactive(
                &client,
                &mut journal,
                mutations::InteractiveRevision {
                    item_id,
                    revision: *base_revision,
                    title,
                    body: body.as_deref(),
                    choices,
                    clear_choices: *clear_choices,
                    fields: &fields,
                    file_ids: (*clear_files || !file_ids.is_empty()).then_some(file_ids.as_slice()),
                    files,
                    in_reply_to: in_reply_to.as_deref(),
                },
                *op_id,
            )
        }
        Command::Progress {
            session,
            request,
            percent,
            stage,
            note,
            sender_id,
            op_id,
        } => mutations::progress(
            &client,
            &mut journal,
            *session,
            request,
            &ProgressReport {
                percent: *percent,
                stage: stage.clone(),
                note: note.clone(),
            },
            sender_id.as_deref(),
            *op_id,
        ),
        Command::Retract {
            item_id,
            in_reply_to,
            op_id,
        } => writes::control(
            &client,
            &mut journal,
            *op_id,
            ControlWrite::Retract {
                item_id,
                in_reply_to: in_reply_to.as_deref(),
            },
        ),
        Command::Lifecycle {
            item_id,
            action,
            op_id,
        } => writes::control(
            &client,
            &mut journal,
            *op_id,
            ControlWrite::Lifecycle {
                item_id,
                action: action.as_str(),
            },
        ),
        Command::Session {
            action: SessionCommand::End { session, op_id },
        } => writes::control(
            &client,
            &mut journal,
            *op_id,
            ControlWrite::EndSession(*session),
        ),
        Command::AdvisoryAck {
            item_id,
            advisory_id,
            op_id,
        } => writes::post(
            &client,
            &mut journal,
            "advisory-ack",
            None,
            *op_id,
            &["agent", "messages", item_id, "advisories", "ack"],
            serde_json::json!({"advisory_id":advisory_id}),
        ),
        Command::Result {
            item_id,
            status,
            op_id,
        } => writes::control(
            &client,
            &mut journal,
            *op_id,
            ControlWrite::Result {
                item_id,
                status: (*status).into(),
            },
        ),
        Command::Receipt {
            item_id,
            entry_id,
            agent,
            rerouted,
            not_handed,
            op_id,
        } => writes::control(
            &client,
            &mut journal,
            *op_id,
            ControlWrite::Receipt {
                item_id,
                entry_id: entry_id.as_deref(),
                agent: agent.as_deref(),
                rerouted: *rerouted,
                not_handed: not_handed.as_deref(),
            },
        ),
        Command::FileUse {
            item_id,
            entry_id,
            file_id,
            reason,
            op_id,
            used,
            unusable,
            transferred,
            not_transferred,
        } => writes::control(
            &client,
            &mut journal,
            *op_id,
            ControlWrite::FileUse {
                item_id,
                entry_id,
                file_id,
                transfer: transferred
                    .then_some("transferred")
                    .or_else(|| not_transferred.then_some("not_transferred")),
                usage: used
                    .then_some("used")
                    .or_else(|| unusable.then_some("unusable")),
                reason: reason.as_deref(),
            },
        ),
        Command::Show {
            item_id,
            chat,
            device,
            device_id,
            op_id,
        } => writes::control(
            &client,
            &mut journal,
            *op_id,
            ControlWrite::Show {
                item_id,
                chat: *chat,
                device_name: device.as_deref(),
                device_id: device_id.as_deref(),
            },
        ),
        Command::FileUpload { file, op_id } => {
            writes::upload_file(&client, &mut journal, file, *op_id)
        }
        Command::DiscardUploads { file_id, op_id } => {
            writes::discard_upload(&client, &mut journal, file_id, *op_id)
        }
        Command::Retry { op_id, journal_id } => {
            let journal_id = journal_id.map(|id| id.to_string());
            writes::retry(&client, &mut journal, *op_id, journal_id.as_deref())
        }
        Command::JournalId => journal
            .journal_id()
            .map(|id| {
                serde_json::json!({
                    "kind":"journal","journal_id":id
                })
            })
            .map_err(writes::WriteFailure::from),
        Command::Abandon { op_id } => writes::abandon(&mut journal, *op_id),
        _ => unreachable!("device write dispatch requires a write command"),
    };
    match result {
        Ok(output) => println!("{output}"),
        Err(error) => write_failure(error),
    }
}

fn write_failure(error: writes::WriteFailure) -> ! {
    if let Some(output) = &error.output {
        println!("{output}");
    }
    fail(error.to_string(), error.exit_code)
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
    fn listen_uses_an_explicit_caller_cursor_or_snapshot() {
        let session = "123e4567-e89b-42d3-a456-426614174000";
        let cli = Cli::try_parse_from([
            "agent",
            "listen",
            "--session",
            session,
            "--after",
            "42",
            "--follow",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Listen(ListenArgs {
                feed: FeedArgs {
                    after: Some(42),
                    ..
                },
                follow: true,
                ..
            })
        ));
        assert!(
            Cli::try_parse_from(["agent", "listen", "--next", "--snapshot", "--after", "42"])
                .is_err()
        );
        assert!(Cli::try_parse_from(["agent", "listen", "--follow", "--origin"]).is_err());
        assert!(
            Cli::try_parse_from(["agent", "listen", "--all-sessions", "--snapshot", "--next"])
                .is_ok()
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
            matches!(cli.command, Command::Read(ReadArgs { feed: FeedArgs { session: Some(session), .. }, .. }) if session.to_string() == "123e4567-e89b-42d3-a456-426614174000")
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
            "--item-id",
            "item/one",
            "--chat",
            "--device",
            "Office",
        ])
        .unwrap();
        assert!(matches!(
            show.command,
            Command::Show { item_id, chat: true, device: Some(device), .. }
            if item_id == "item/one" && device == "Office"
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
            "--item-id",
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
                "--item-id",
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
    fn receipts_distinguish_answer_and_entry_and_require_a_reason_for_refusal() {
        let handed = Cli::try_parse_from([
            "agent",
            "receipt",
            "--item-id",
            "item",
            "--agent",
            "Desk",
            "--rerouted",
        ])
        .unwrap();
        assert!(matches!(
            handed.command,
            Command::Receipt {
                entry_id: None,
                rerouted: true,
                not_handed: None,
                ..
            }
        ));
        let chat = Cli::try_parse_from([
            "agent",
            "receipt",
            "--item-id",
            "item",
            "--entry-id",
            "entry",
            "--not-handed",
            "failed",
        ])
        .unwrap();
        assert!(matches!(
            chat.command,
            Command::Receipt { entry_id: Some(id), not_handed: Some(reason), .. }
            if id == "entry" && reason == "failed"
        ));
        assert!(
            Cli::try_parse_from(["agent", "receipt", "--item-id", "item", "--not-handed"]).is_err()
        );
        assert!(
            Cli::try_parse_from([
                "agent",
                "receipt",
                "--item-id",
                "item",
                "--rerouted",
                "--not-handed",
                "failed"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "agent",
                "file-use",
                "--item-id",
                "item",
                "--entry-id",
                "entry",
                "--file-id",
                "a",
                "--unusable",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "agent",
                "file-use",
                "--item-id",
                "item",
                "--entry-id",
                "entry",
                "--file-id",
                "a",
                "--used",
            ])
            .is_ok()
        );
    }
}
