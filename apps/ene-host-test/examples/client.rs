//! Interactive inspection client; all settings changes are explicit commands.
use clap::Parser;
use ene_host_test::{
    client,
    proto::{chat::v1::*, settings::v1::*},
};
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Parser)]
struct Args {
    /// Private discovery file (defaults to the host's platform-specific runtime directory).
    #[arg(long)]
    connection: Option<std::path::PathBuf>,
    #[arg(long, default_value = "demo")]
    companion: String,
    #[arg(long, default_value = "")]
    work: String,
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = Args::parse();
    let file = match args.connection {
        Some(path) => path,
        None => ene_host_test::paths()?.1.join("connection.json"),
    };
    let client = client::connect(&file).await?;
    let target = Target {
        companion_id: args.companion,
        work_id: args.work,
        ..Default::default()
    };
    let read = ReadRequest {
        target: target.clone().into(),
        ..Default::default()
    };
    let get_settings = GetSettingsRequest {
        companion_id: target.companion_id.clone(),
        ..Default::default()
    };
    let reader = client.chat.clone();
    let watch = read.clone();
    let display = tokio::spawn(async move {
        loop {
            let mut events = match reader.subscribe(watch.clone()).await {
                Ok(events) => events,
                Err(error) => {
                    eprintln!("{error}");
                    return;
                }
            };
            loop {
                match events.message::<Event>().await {
                    Ok(Some(event)) => match event.to_owned_message().kind {
                        Some(event::Kind::TextDelta(delta)) => {
                            print!("{}", delta.text);
                            use std::io::Write;
                            let _ = std::io::stdout().flush();
                        }
                        Some(event::Kind::Snapshot(snapshot)) => {
                            println!("\n[snapshot] {}", serde_json::to_string(&snapshot).unwrap())
                        }
                        Some(event::Kind::Changed(_)) => {
                            println!("\n[changed] {} (/snapshot)", event.view().conversation_id)
                        }
                        Some(event::Kind::Recalled(input)) => {
                            println!("\n[recalled] {}: {}", input.why, input.text)
                        }
                        None => {}
                    },
                    Err(error) if error.code == connectrpc::ErrorCode::ResourceExhausted => break,
                    Ok(None) => return,
                    Err(error) => {
                        eprintln!("{error}");
                        return;
                    }
                }
            }
        }
    });
    println!(
        "/settings /model NAME /declare-tools /allow /revoke /stop /history /snapshot /approve ID REV /deny ID REV /retry KEY TEXT /quit"
    );
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let text = line.trim();
        let result: Result<(), Box<dyn std::error::Error + Send + Sync>> = async {
            match text {
                "/quit" => {}
                "/stop" => {
                    client
                        .chat
                        .stop(StopRequest {
                            target: target.clone().into(),
                            ..Default::default()
                        })
                        .await?;
                }
                "/history" => println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &client.chat.get_history(read.clone()).await?.into_owned()
                    )?
                ),
                "/snapshot" => println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &client.chat.get_snapshot(read.clone()).await?.into_owned()
                    )?
                ),
                _ if matches!(text, "/settings" | "/allow" | "/revoke" | "/declare-tools")
                    || text.starts_with("/model ") =>
                {
                    let mut settings = client
                        .settings
                        .get_settings(get_settings.clone())
                        .await?
                        .into_owned();
                    match text {
                        "/settings" => {
                            println!("{}", serde_json::to_string_pretty(&settings)?);
                            return Ok(());
                        }
                        "/declare-tools" => settings.declare_tool_calling = true,
                        "/allow" | "/revoke" => {
                            settings.model_allowed = text == "/allow";
                            settings.tools_allowed = text == "/allow";
                        }
                        _ => {
                            settings.companion_model = text[7..].trim().into();
                            settings.work_model.clone_from(&settings.companion_model);
                        }
                    }
                    let result = client
                        .settings
                        .set_settings(SetSettingsRequest {
                            companion_id: target.companion_id.clone(),
                            expected_revision: settings.revision,
                            settings: settings.into(),
                            ..Default::default()
                        })
                        .await?
                        .into_owned();
                    println!("{}", serde_json::to_string(&result)?);
                }
                _ if text.starts_with("/approve ") || text.starts_with("/deny ") => {
                    let parts: Vec<_> = text.split_whitespace().collect();
                    if parts.len() != 3 {
                        return Err("use /approve ID REV or /deny ID REV".into());
                    }
                    client
                        .chat
                        .respond(RespondRequest {
                            companion_id: target.companion_id.clone(),
                            request_id: parts[1].into(),
                            settings_revision: parts[2].parse()?,
                            approve: parts[0] == "/approve",
                            ..Default::default()
                        })
                        .await?;
                }
                "" => {}
                _ => {
                    let (key, message) = if let Some(retry) = text.strip_prefix("/retry ") {
                        let (key, message) = retry.split_once(' ').ok_or("use /retry KEY TEXT")?;
                        (key.to_owned(), message)
                    } else {
                        (uuid::Uuid::new_v4().to_string(), text)
                    };
                    println!("request key: {key}");
                    let accepted = client
                        .chat
                        .send_message(SendMessageRequest {
                            target: target.clone().into(),
                            request_key: key,
                            text: message.into(),
                            ..Default::default()
                        })
                        .await?;
                    println!("accepted {}", accepted.view().input_id);
                }
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            eprintln!("{error}");
        }
        if text == "/quit" {
            break;
        }
    }
    display.abort();
    Ok(())
}
