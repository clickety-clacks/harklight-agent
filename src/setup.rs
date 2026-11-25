use std::{
    env,
    ffi::OsStr,
    fs,
    io::{self, Write},
    path::PathBuf,
    process::Command,
    thread,
    time::Duration,
};

use harklight_agent::{
    commands::MessageFieldOptions,
    enrollment::{self, EnrollmentError},
    journal::JournalStore,
    v2::{
        AgentClient, feed, sends,
        writes::{self, ControlWrite},
    },
};
use serde_json::Value;
use uuid::Uuid;

use crate::{Cli, device_identity, device_journal, fail, identity_dir, write_failure};

const SETUP_SENDER: &str = "app:agent/setup";

pub(super) fn run(cli: &Cli, invitation: Option<&OsStr>, label: Option<&str>) {
    let directory = identity_dir(cli);
    let mut show_code = true;
    let mut waiting = |code: &str| {
        if show_code {
            println!("Approve this computer in Devices:");
            println!("  Code on device:  {code}");
            println!("  Sender IDs:      app:agent/");
            println!("Waiting for approval…");
            io::stdout()
                .flush()
                .unwrap_or_else(|error| fail(error.to_string(), 1));
            show_code = false;
        }
    };

    let new_enrollment = if directory.join("enrollment.json").is_file() {
        if invitation.is_some() {
            fail(
                "This computer already has an enrollment. Run `harklight-agent setup` to check it."
                    .into(),
                2,
            );
        }
        false
    } else {
        let state = match enrollment_value(enrollment::resume_setup(&directory, &mut waiting)) {
            Some(state) => state,
            None => {
                let Some(invitation) = invitation else {
                    println!(
                        "This computer is not connected. Create an invitation in Devices and run `harklight-agent setup <invitation>`."
                    );
                    return;
                };
                let default_label = label.is_none().then(host_name).flatten();
                enrollment_value(enrollment::start_setup(
                    &directory,
                    invitation,
                    label.or(default_label.as_deref()),
                ))
            }
        };
        let mut state = state;
        while state["state"] == "pending" {
            waiting(
                state["approval_code"]
                    .as_str()
                    .expect("validated approval code"),
            );
            thread::sleep(Duration::from_secs(2));
            state = enrollment_value(enrollment::resume_setup(&directory, &mut waiting))
                .unwrap_or_else(|| fail("The saved enrollment request is missing.".into(), 1));
        }
        if state["state"] != "approved" {
            fail(
                format!(
                    "This enrollment request ended ({}). Move the identity directory at {} aside, create a new invitation in Devices, and run setup again.",
                    state["state"].as_str().unwrap_or("unknown"),
                    directory.display()
                ),
                2,
            );
        }
        true
    };

    let identity = device_identity(cli);
    let client =
        AgentClient::new(identity.clone()).unwrap_or_else(|error| fail(error.to_string(), 1));
    let grants = client
        .sender_grants()
        .unwrap_or_else(|error| fail(error.to_string(), 1));
    let Some(sender) = setup_sender(&grants) else {
        let mut name = directory.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".no-sender-grant-{}", Uuid::new_v4()));
        let backup = directory.with_file_name(name);
        let recovery = "This computer was approved without any Sender IDs. Remove it in Devices, create a new invitation and run setup again. Enter app:agent/ when approving.";
        match fs::rename(&directory, &backup) {
            Ok(()) => fail(
                format!(
                    "{recovery}\nSaved the old identity at {}.",
                    backup.display()
                ),
                2,
            ),
            Err(error) => fail(
                format!(
                    "{recovery}\nCould not move the old identity at {}: {error}",
                    directory.display()
                ),
                2,
            ),
        }
    };

    println!("Server: {}:{}", identity.name, identity.port);
    if let Some(person_id) = identity.person_id {
        println!("Account: {person_id}");
    }
    println!("Device: {}", identity.device_id);
    println!("Sender IDs: {}", grants.join(", "));
    install_skill();
    if new_enrollment {
        let mut journal = device_journal(cli, &identity.device_id);
        test_connection(&client.with_ack2(true), &mut journal, &sender);
    }
}

fn enrollment_value<T>(result: Result<T, EnrollmentError>) -> T {
    result.unwrap_or_else(|error| fail(error.message, error.exit_code))
}

fn host_name() -> Option<String> {
    let output = Command::new("hostname").output().ok()?;
    let name = String::from_utf8(output.stdout).ok()?;
    let name = name.trim();
    (output.status.success() && !name.is_empty()).then(|| name.to_owned())
}

fn setup_sender(grants: &[String]) -> Option<String> {
    let covers_setup = grants.iter().any(|grant| {
        grant == SETUP_SENDER || (grant.ends_with('/') && SETUP_SENDER.starts_with(grant))
    });
    if covers_setup {
        Some(SETUP_SENDER.into())
    } else {
        grants.first().map(|grant| {
            if grant.ends_with('/') {
                format!("{grant}setup")
            } else {
                grant.clone()
            }
        })
    }
}

fn install_skill() {
    const SKILL: &str = include_str!("../vendor/harklight/SKILL.md");

    let home = env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            fail(
                "HOME is not set. Set it and run setup again to install the skill.".into(),
                1,
            )
        });
    let claude_home = home.join(".claude");
    let has_codex = home.join(".codex").is_dir()
        || env::var_os("CODEX_HOME").is_some_and(|path| PathBuf::from(path).is_dir());
    let destinations = [
        (claude_home.is_dir(), claude_home.join("skills/harklight")),
        (has_codex, home.join(".agents/skills/harklight")),
    ];
    if !destinations.iter().any(|(present, _)| *present) {
        println!(
            "Claude Code and Codex directories were not found. Run setup again after installing either tool. Skill paths:"
        );
        for (_, directory) in destinations {
            println!("  {}", directory.join("SKILL.md").display());
        }
        return;
    }
    for (present, directory) in destinations {
        if present {
            let path = directory.join("SKILL.md");
            let temporary = directory.join(format!(".SKILL-{}.tmp", Uuid::new_v4()));
            if let Err(error) = fs::create_dir_all(&directory)
                .and_then(|()| fs::write(&temporary, SKILL))
                .and_then(|()| fs::rename(&temporary, &path))
            {
                let _ = fs::remove_file(&temporary);
                fail(
                    format!(
                        "Could not install Harklight skill at {}: {error}",
                        path.display()
                    ),
                    1,
                );
            }
            println!("Installed Harklight skill at {}", path.display());
        }
    }
}

fn test_connection(client: &AgentClient, journal: &mut JournalStore, sender: &str) {
    let output = sends::send(
        client,
        journal,
        &sends::SendInput {
            session: Some("new"),
            sender,
            title: "Harklight is connected",
            body: Some("Reply to confirm that this computer can receive your answer."),
            choices: &[],
            supersedes: &[],
            supersedes_local: &[],
            files: &[],
            fields: &MessageFieldOptions {
                message_type: Some("request".into()),
                reply: true,
                ..MessageFieldOptions::default()
            },
            answer_ui: None,
        },
        None,
        false,
    )
    .unwrap_or_else(|error| write_failure(error));
    let item_id = output["item_id"]
        .as_str()
        .expect("committed send has an item ID");
    let session = serde_json::from_value(output["session_handle"].clone())
        .expect("committed send has a session ID");

    println!("Sent a test question. Waiting for your answer…");
    let mut cursor = output["response"]["rev"]
        .as_u64()
        .expect("committed send has a revision");
    let answer = loop {
        let mut terminal = None;
        feed::run(
            client,
            journal,
            Some(session),
            feed::Start::After(cursor),
            feed::Mode::Next,
            |change| {
                if let Some(rev) = change["rev"].as_u64() {
                    cursor = cursor.max(rev);
                }
                if change["kind"] == "message"
                    && change["data"]["item_id"] == item_id
                    && !change["data"]["terminal"].is_null()
                {
                    terminal = Some(change["data"]["terminal"].clone());
                }
                Ok(())
            },
        )
        .unwrap_or_else(|error| fail(error.to_string(), error.exit_code()));
        if let Some(terminal) = terminal {
            if terminal["state"] != "answered" {
                fail("The test question closed before it was answered.".into(), 2);
            }
            break terminal["answer"].clone();
        }
    };

    writes::control(
        client,
        journal,
        None,
        ControlWrite::Receipt {
            item_id,
            entry_id: None,
            not_handed: None,
            agent: None,
            rerouted: false,
        },
    )
    .unwrap_or_else(|error| write_failure(error));
    println!(
        "Answer: {}",
        answer
            .as_str()
            .map_or_else(|| answer.to_string(), str::to_owned)
    );
    writes::control(
        client,
        journal,
        None,
        ControlWrite::Result {
            item_id,
            status: "applied",
        },
    )
    .unwrap_or_else(|error| write_failure(error));
    println!("Connected.");
}
