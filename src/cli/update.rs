//! `spacebot update` — update status and self-update over the control API.
//!
//! `UpdateStatus` is rendered from the raw response rather than a typed
//! struct: the type lives in `crate::update` and is serialize-only.

use super::client::ApiClient;
use super::output;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum UpdateCommand {
    /// Show update status from the last background check
    Check {
        /// Run an immediate check against GitHub instead
        #[arg(long)]
        now: bool,
    },
    /// Request an in-place update (unavailable in this build; use rebuild+redeploy)
    Apply,
    /// Print the changelog
    Changelog,
}

pub async fn run(ctx: &super::Context, cmd: UpdateCommand) -> anyhow::Result<()> {
    let client = ApiClient::from_context(ctx)?;

    match cmd {
        UpdateCommand::Check { now } => {
            let value = if now {
                client.post("update-check", &serde_json::json!({})).await?
            } else {
                client.get("update-check").await?
            };
            if ctx.json {
                output::json(&value);
                return Ok(());
            }
            println!(
                "Current version:  {}",
                value["current_version"].as_str().unwrap_or("unknown")
            );
            println!(
                "Latest version:   {}",
                value["latest_version"].as_str().unwrap_or("unknown")
            );
            println!(
                "Update available: {}",
                value["update_available"].as_bool().unwrap_or(false)
            );
            println!(
                "Deployment:       {}",
                value["deployment"].as_str().unwrap_or("unknown")
            );
            println!(
                "Can apply:        {}",
                value["can_apply"].as_bool().unwrap_or(false)
            );
            if let Some(reason) = value["cannot_apply_reason"].as_str() {
                println!("  Reason:         {reason}");
            }
            if let Some(image) = value["docker_image"].as_str() {
                println!("Docker image:     {image}");
            }
            if let Some(url) = value["release_url"].as_str() {
                println!("Release URL:      {url}");
            }
            if let Some(checked_at) = value["checked_at"].as_str() {
                println!("Checked at:       {}", output::short_timestamp(checked_at));
            }
            if let Some(error) = value["error"].as_str() {
                println!("Last check error: {error}");
            }
            Ok(())
        }
        UpdateCommand::Apply => {
            let value = client.post("update-apply", &serde_json::json!({})).await?;
            if ctx.json {
                output::json(&value);
                return Ok(());
            }
            if value["status"].as_str() == Some("error") {
                anyhow::bail!("{}", value["error"].as_str().unwrap_or("update failed"));
            }
            eprintln!("Update started — the daemon will restart with the new image.");
            Ok(())
        }
        UpdateCommand::Changelog => {
            let value = client.get("changelog").await?;
            if ctx.json {
                output::json(&value);
                return Ok(());
            }
            let content = value["content"].as_str().unwrap_or("");
            print!("{content}");
            if !content.ends_with('\n') {
                println!();
            }
            Ok(())
        }
    }
}
