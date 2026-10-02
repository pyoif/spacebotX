//! `spacebot messaging` — messaging platform management over the control API.

use super::client::{self, ApiClient};
use super::output;
use anyhow::Context as _;
use clap::Subcommand;
use spacebot::api::messaging::{
    MessagingInstanceActionResponse, MessagingStatusResponse, PlatformStatus,
};

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum ToggleState {
    On,
    Off,
}

#[derive(Subcommand)]
pub enum MessagingCommand {
    /// Show platform configuration and adapter instances
    Status,
    /// Enable or disable a platform or named instance
    Toggle {
        /// Platform name (discord, slack, telegram, email, webhook, mattermost, signal)
        platform: String,
        /// Desired state
        state: ToggleState,
        /// Named adapter instance (defaults to the platform's default instance)
        #[arg(short, long)]
        adapter: Option<String>,
    },
    /// Disconnect a platform: remove credentials and bindings, stop the adapter
    Disconnect {
        /// Platform name
        platform: String,
        /// Only disconnect this named instance
        #[arg(short, long)]
        adapter: Option<String>,
    },
    /// Manage adapter instances
    #[command(subcommand)]
    Instance(InstanceCommand),
}

#[derive(Subcommand)]
pub enum InstanceCommand {
    /// Create or update an adapter instance
    Create {
        /// Platform name
        platform: String,
        /// Instance name (omit for the platform's default instance)
        #[arg(short, long)]
        name: Option<String>,
        /// Create the instance disabled
        #[arg(long)]
        disabled: bool,
        /// Read platform credentials as a JSON object from stdin
        /// (e.g. {"discord_token": "..."})
        #[arg(long)]
        stdin: bool,
    },
    /// Delete an adapter instance and its bindings
    Delete {
        /// Platform name
        platform: String,
        /// Instance name (omit for the platform's default instance)
        #[arg(short, long)]
        name: Option<String>,
    },
}

pub async fn run(ctx: &super::Context, messaging_cmd: MessagingCommand) -> anyhow::Result<()> {
    let client = ApiClient::from_context(ctx)?;

    match messaging_cmd {
        MessagingCommand::Status => {
            let value = client.get("messaging/status").await?;
            if ctx.json {
                output::json(&value);
                return Ok(());
            }
            let status: MessagingStatusResponse = client::parse(value)?;
            let platform_row = |name: &str, platform: &PlatformStatus| {
                vec![
                    name.to_string(),
                    yes_no(platform.configured),
                    yes_no(platform.enabled),
                ]
            };
            let rows = vec![
                platform_row("discord", &status.discord),
                platform_row("slack", &status.slack),
                platform_row("telegram", &status.telegram),
                platform_row("email", &status.email),
                platform_row("webhook", &status.webhook),
                platform_row("mattermost", &status.mattermost),
                platform_row("signal", &status.signal),
            ];
            output::table(&["PLATFORM", "CONFIGURED", "ENABLED"], &rows);

            if !status.instances.is_empty() {
                println!();
                let rows: Vec<Vec<String>> = status
                    .instances
                    .iter()
                    .map(|instance| {
                        vec![
                            instance.platform.clone(),
                            instance.name.clone().unwrap_or_else(|| "default".into()),
                            instance.runtime_key.clone(),
                            yes_no(instance.enabled),
                            instance.binding_count.to_string(),
                        ]
                    })
                    .collect();
                output::table(
                    &["PLATFORM", "INSTANCE", "ADAPTER", "ENABLED", "BINDINGS"],
                    &rows,
                );
            }
            Ok(())
        }
        MessagingCommand::Toggle {
            platform,
            state,
            adapter,
        } => {
            let enabled = matches!(state, ToggleState::On);
            let mut body = serde_json::json!({ "platform": platform, "enabled": enabled });
            if let Some(adapter) = &adapter {
                body["adapter"] = serde_json::json!(adapter);
            }
            let value = client.post("messaging/toggle", &body).await?;
            if ctx.json {
                output::json(&value);
                return Ok(());
            }
            report_action(&value, "Toggled.")
        }
        MessagingCommand::Disconnect { platform, adapter } => {
            let mut body = serde_json::json!({ "platform": platform });
            if let Some(adapter) = &adapter {
                body["adapter"] = serde_json::json!(adapter);
            }
            let value = client.post("messaging/disconnect", &body).await?;
            if ctx.json {
                output::json(&value);
                return Ok(());
            }
            report_action(&value, "Disconnected.")
        }
        MessagingCommand::Instance(instance_cmd) => match instance_cmd {
            InstanceCommand::Create {
                platform,
                name,
                disabled,
                stdin,
            } => {
                let mut body = serde_json::json!({ "platform": platform });
                if let Some(name) = &name {
                    body["name"] = serde_json::json!(name);
                }
                if disabled {
                    body["enabled"] = serde_json::json!(false);
                }
                if stdin {
                    let mut buf = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
                    let credentials: serde_json::Value = serde_json::from_str(&buf)
                        .context("failed to parse credentials as JSON")?;
                    if !credentials.is_object() {
                        anyhow::bail!("credentials must be a JSON object");
                    }
                    body["credentials"] = credentials;
                }

                let value = client.post("messaging/instances", &body).await?;
                if ctx.json {
                    output::json(&value);
                    return Ok(());
                }
                let result: MessagingInstanceActionResponse = client::parse(value)?;
                if !result.success {
                    anyhow::bail!("{}", result.message);
                }
                eprintln!("{}", result.message);
                Ok(())
            }
            InstanceCommand::Delete { platform, name } => {
                let mut body = serde_json::json!({ "platform": platform });
                if let Some(name) = &name {
                    body["name"] = serde_json::json!(name);
                }
                let value = client.delete_json("messaging/instances", &body).await?;
                if ctx.json {
                    output::json(&value);
                    return Ok(());
                }
                let result: MessagingInstanceActionResponse = client::parse(value)?;
                if !result.success {
                    anyhow::bail!("{}", result.message);
                }
                eprintln!("{}", result.message);
                Ok(())
            }
        },
    }
}

fn yes_no(value: bool) -> String {
    if value { "yes" } else { "no" }.to_string()
}

/// Render a `{ success, message }` response built ad hoc by the handler.
fn report_action(value: &serde_json::Value, fallback: &str) -> anyhow::Result<()> {
    let message = value["message"].as_str().unwrap_or(fallback);
    if !value["success"].as_bool().unwrap_or(false) {
        anyhow::bail!("{message}");
    }
    eprintln!("{message}");
    Ok(())
}
