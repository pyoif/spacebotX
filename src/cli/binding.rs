//! `spacebot binding` — agent/channel binding management over the control API.

use super::client::{self, ApiClient};
use super::output;
use clap::Subcommand;
use spacebot::api::bindings::{
    BindingsListResponse, CreateBindingResponse, DeleteBindingResponse, UpdateBindingResponse,
};

#[derive(Subcommand)]
pub enum BindingCommand {
    /// List bindings
    List {
        /// Filter by agent ID
        #[arg(short, long)]
        agent: Option<String>,
    },
    /// Create a binding between an agent and a messaging channel
    Create {
        /// Agent ID to bind
        agent_id: String,
        /// Platform channel (discord, slack, telegram, email, webhook, mattermost, signal)
        channel: String,
        /// Named adapter instance (defaults to the platform's default instance)
        #[arg(short, long)]
        adapter: Option<String>,
        /// Discord guild ID
        #[arg(long)]
        guild_id: Option<String>,
        /// Slack workspace ID
        #[arg(long)]
        workspace_id: Option<String>,
        /// Telegram chat ID
        #[arg(long)]
        chat_id: Option<String>,
        /// Mattermost team ID
        #[arg(long)]
        team_id: Option<String>,
        /// Restrict to a channel ID (repeatable)
        #[arg(long = "channel-id")]
        channel_ids: Vec<String>,
        /// Only respond when the agent is mentioned
        #[arg(long)]
        require_mention: bool,
        /// Allow DMs from a user ID (repeatable)
        #[arg(long = "dm-user")]
        dm_allowed_users: Vec<String>,
        /// User ID allowed to run authority-gated slash commands (repeatable)
        #[arg(long = "authority")]
        authority: Vec<String>,
    },
    /// Update a binding. The binding is matched by the positional args and
    /// original selector flags; the updated binding is written exactly as the
    /// --new-* and option flags specify, so omitted optional fields are cleared.
    Update {
        /// Agent ID of the binding to update
        agent_id: String,
        /// Channel of the binding to update
        channel: String,
        /// Adapter of the binding to update
        #[arg(long)]
        adapter: Option<String>,
        /// Guild ID of the binding to update
        #[arg(long)]
        guild_id: Option<String>,
        /// Workspace ID of the binding to update
        #[arg(long)]
        workspace_id: Option<String>,
        /// Chat ID of the binding to update
        #[arg(long)]
        chat_id: Option<String>,
        /// Team ID of the binding to update
        #[arg(long)]
        team_id: Option<String>,
        /// New agent ID (defaults to unchanged)
        #[arg(long)]
        new_agent_id: Option<String>,
        /// New channel (defaults to unchanged)
        #[arg(long)]
        new_channel: Option<String>,
        /// New adapter (omit to clear)
        #[arg(long)]
        new_adapter: Option<String>,
        /// New guild ID (omit to clear)
        #[arg(long)]
        new_guild_id: Option<String>,
        /// New workspace ID (omit to clear)
        #[arg(long)]
        new_workspace_id: Option<String>,
        /// New chat ID (omit to clear)
        #[arg(long)]
        new_chat_id: Option<String>,
        /// New team ID (omit to clear)
        #[arg(long)]
        new_team_id: Option<String>,
        /// Channel ID for the updated binding (repeatable, omit to clear)
        #[arg(long = "channel-id")]
        channel_ids: Vec<String>,
        /// Require a mention on the updated binding
        #[arg(long)]
        require_mention: bool,
        /// Allowed DM user ID for the updated binding (repeatable, omit to clear)
        #[arg(long = "dm-user")]
        dm_allowed_users: Vec<String>,
        /// Authority user ID for the updated binding (repeatable, omit to clear)
        #[arg(long = "authority")]
        authority: Vec<String>,
    },
    /// Delete a binding matched by agent, channel, and selector flags
    Delete {
        /// Agent ID of the binding
        agent_id: String,
        /// Channel of the binding
        channel: String,
        /// Adapter of the binding
        #[arg(long)]
        adapter: Option<String>,
        /// Guild ID of the binding
        #[arg(long)]
        guild_id: Option<String>,
        /// Workspace ID of the binding
        #[arg(long)]
        workspace_id: Option<String>,
        /// Chat ID of the binding
        #[arg(long)]
        chat_id: Option<String>,
        /// Team ID of the binding
        #[arg(long)]
        team_id: Option<String>,
    },
}

pub async fn run(ctx: &super::Context, binding_cmd: BindingCommand) -> anyhow::Result<()> {
    let client = ApiClient::from_context(ctx)?;

    match binding_cmd {
        BindingCommand::List { agent } => {
            let path = match &agent {
                Some(agent_id) => format!("bindings?agent_id={}", urlencoding::encode(agent_id)),
                None => "bindings".to_string(),
            };
            let value = client.get(&path).await?;
            if ctx.json {
                output::json(&value);
                return Ok(());
            }
            let response: BindingsListResponse = client::parse(value)?;
            if response.bindings.is_empty() {
                eprintln!("No bindings configured.");
                return Ok(());
            }
            let rows: Vec<Vec<String>> = response
                .bindings
                .iter()
                .map(|binding| {
                    vec![
                        binding.agent_id.clone(),
                        binding.channel.clone(),
                        binding.adapter.clone().unwrap_or_default(),
                        binding.guild_id.clone().unwrap_or_default(),
                        binding.workspace_id.clone().unwrap_or_default(),
                        binding.chat_id.clone().unwrap_or_default(),
                        binding.team_id.clone().unwrap_or_default(),
                        binding.channel_ids.join(","),
                        if binding.require_mention { "yes" } else { "" }.to_string(),
                    ]
                })
                .collect();
            output::table(
                &[
                    "AGENT",
                    "CHANNEL",
                    "ADAPTER",
                    "GUILD",
                    "WORKSPACE",
                    "CHAT",
                    "TEAM",
                    "CHANNELS",
                    "MENTION",
                ],
                &rows,
            );
            Ok(())
        }
        BindingCommand::Create {
            agent_id,
            channel,
            adapter,
            guild_id,
            workspace_id,
            chat_id,
            team_id,
            channel_ids,
            require_mention,
            dm_allowed_users,
            authority,
        } => {
            let mut body = serde_json::json!({
                "agent_id": agent_id,
                "channel": channel,
                "channel_ids": channel_ids,
                "require_mention": require_mention,
                "dm_allowed_users": dm_allowed_users,
                "authority": authority,
            });
            set_optional(&mut body, "adapter", &adapter);
            set_optional(&mut body, "guild_id", &guild_id);
            set_optional(&mut body, "workspace_id", &workspace_id);
            set_optional(&mut body, "chat_id", &chat_id);
            set_optional(&mut body, "team_id", &team_id);

            let value = client.post("bindings", &body).await?;
            if ctx.json {
                output::json(&value);
                return Ok(());
            }
            let result: CreateBindingResponse = client::parse(value)?;
            if !result.success {
                anyhow::bail!("{}", result.message);
            }
            eprintln!("{}", result.message);
            if result.restart_required {
                eprintln!("Note: Restart for the new credentials to take effect.");
            }
            Ok(())
        }
        BindingCommand::Update {
            agent_id,
            channel,
            adapter,
            guild_id,
            workspace_id,
            chat_id,
            team_id,
            new_agent_id,
            new_channel,
            new_adapter,
            new_guild_id,
            new_workspace_id,
            new_chat_id,
            new_team_id,
            channel_ids,
            require_mention,
            dm_allowed_users,
            authority,
        } => {
            let mut body = serde_json::json!({
                "original_agent_id": agent_id,
                "original_channel": channel,
                "agent_id": new_agent_id.as_deref().unwrap_or(&agent_id),
                "channel": new_channel.as_deref().unwrap_or(&channel),
                "channel_ids": channel_ids,
                "require_mention": require_mention,
                "dm_allowed_users": dm_allowed_users,
                "authority": authority,
            });
            set_optional(&mut body, "original_adapter", &adapter);
            set_optional(&mut body, "original_guild_id", &guild_id);
            set_optional(&mut body, "original_workspace_id", &workspace_id);
            set_optional(&mut body, "original_chat_id", &chat_id);
            set_optional(&mut body, "original_team_id", &team_id);
            set_optional(&mut body, "adapter", &new_adapter);
            set_optional(&mut body, "guild_id", &new_guild_id);
            set_optional(&mut body, "workspace_id", &new_workspace_id);
            set_optional(&mut body, "chat_id", &new_chat_id);
            set_optional(&mut body, "team_id", &new_team_id);

            let value = client.put("bindings", &body).await?;
            if ctx.json {
                output::json(&value);
                return Ok(());
            }
            let result: UpdateBindingResponse = client::parse(value)?;
            if !result.success {
                anyhow::bail!("{}", result.message);
            }
            eprintln!("{}", result.message);
            Ok(())
        }
        BindingCommand::Delete {
            agent_id,
            channel,
            adapter,
            guild_id,
            workspace_id,
            chat_id,
            team_id,
        } => {
            let mut body = serde_json::json!({
                "agent_id": agent_id,
                "channel": channel,
            });
            set_optional(&mut body, "adapter", &adapter);
            set_optional(&mut body, "guild_id", &guild_id);
            set_optional(&mut body, "workspace_id", &workspace_id);
            set_optional(&mut body, "chat_id", &chat_id);
            set_optional(&mut body, "team_id", &team_id);

            let value = client.delete_json("bindings", &body).await?;
            if ctx.json {
                output::json(&value);
                return Ok(());
            }
            let result: DeleteBindingResponse = client::parse(value)?;
            if !result.success {
                anyhow::bail!("{}", result.message);
            }
            eprintln!("{}", result.message);
            Ok(())
        }
    }
}

/// Set a JSON field only when the value is present, so absent fields keep the
/// API's `#[serde(default)]` semantics.
fn set_optional(body: &mut serde_json::Value, key: &str, value: &Option<String>) {
    if let Some(value) = value {
        body[key] = serde_json::json!(value);
    }
}
