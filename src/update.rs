//! Update checking.
//!
//! Checks GitHub releases for new versions and surfaces an
//! [`UpdateStatus`]. It deliberately does **not** perform in-place container
//! updates: this fork removed the Docker self-update path (and its former
//! Docker-API client dependency). Container images are rebuilt and redeployed
//! out of band; the API/CLI surface reports `can_apply: false` with an
//! explanatory reason so clients degrade cleanly.

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};

use std::sync::Arc;
use std::time::Duration;

/// GitHub repository for release checks.
const GITHUB_REPO: &str = "spacedriveapp/spacebot";

/// Current binary version from Cargo.toml.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default check interval (1 hour).
const CHECK_INTERVAL: Duration = Duration::from_secs(3600);

/// Deployment environment, detected from SPACEBOT_DEPLOYMENT env var.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Deployment {
    Docker,
    /// Hosted on the Spacebot platform. Updates are managed by the platform
    /// via image rollouts — the instance itself cannot self-update.
    Hosted,
    Native,
}

impl Deployment {
    pub fn detect() -> Self {
        match std::env::var("SPACEBOT_DEPLOYMENT").as_deref() {
            Ok("docker") => Deployment::Docker,
            Ok("hosted") => Deployment::Hosted,
            _ if is_running_in_container() => Deployment::Docker,
            _ => Deployment::Native,
        }
    }
}

fn is_running_in_container() -> bool {
    if std::path::Path::new("/.dockerenv").exists() {
        return true;
    }

    let Ok(cgroup) = std::fs::read_to_string("/proc/1/cgroup") else {
        return false;
    };

    ["docker", "containerd", "kubepods", "podman"]
        .iter()
        .any(|marker| cgroup.contains(marker))
}

/// Result of an update check.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct UpdateStatus {
    pub current_version: String,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub release_url: Option<String>,
    pub release_notes: Option<String>,
    pub deployment: Deployment,
    /// Whether the Docker socket is accessible (enables one-click update).
    pub can_apply: bool,
    /// Human-readable reason when one-click apply is unavailable.
    pub cannot_apply_reason: Option<String>,
    /// Current container image reference when running in Docker.
    pub docker_image: Option<String>,
    pub checked_at: Option<chrono::DateTime<chrono::Utc>>,
    pub error: Option<String>,
}

impl Default for UpdateStatus {
    fn default() -> Self {
        Self {
            current_version: CURRENT_VERSION.to_string(),
            latest_version: None,
            update_available: false,
            release_url: None,
            release_notes: None,
            deployment: Deployment::detect(),
            can_apply: false,
            cannot_apply_reason: None,
            docker_image: None,
            checked_at: None,
            error: None,
        }
    }
}

/// Shared update status, readable from API handlers.
pub type SharedUpdateStatus = Arc<ArcSwap<UpdateStatus>>;

pub fn new_shared_status() -> SharedUpdateStatus {
    let mut status = UpdateStatus::default();
    match status.deployment {
        Deployment::Docker => {
            status.can_apply = false;
            status.cannot_apply_reason =
                Some(SELF_UPDATE_DISABLED_REASON.to_string());
        }
        Deployment::Native => {
            status.cannot_apply_reason =
                Some("Native/source installs update manually (rebuild + restart).".to_string());
        }
        Deployment::Hosted => {
            status.cannot_apply_reason = Some(
                "Hosted instances are updated by platform rollout, not self-service.".to_string(),
            );
        }
    }
    Arc::new(ArcSwap::from_pointee(status))
}

/// Reason reported when running in Docker: this fork has no in-place updater.
const SELF_UPDATE_DISABLED_REASON: &str =
    "In-place Docker self-update is not available in this build; rebuild and redeploy the image.";

/// Public accessor for the self-update-disabled reason (used by the API layer).
pub fn self_update_unavailable_reason() -> &'static str {
    SELF_UPDATE_DISABLED_REASON
}

/// Minimal GitHub release response.
#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    body: Option<String>,
}

/// Check GitHub for the latest release and compare with current version.
pub async fn check_for_update(status: &SharedUpdateStatus) {
    let result = fetch_latest_release().await;

    let current = status.load();
    let capability = detect_apply_capability(current.deployment);
    let mut next = UpdateStatus {
        current_version: CURRENT_VERSION.to_string(),
        deployment: current.deployment,
        can_apply: capability.can_apply,
        cannot_apply_reason: capability.cannot_apply_reason,
        docker_image: capability.docker_image,
        checked_at: Some(chrono::Utc::now()),
        ..Default::default()
    };

    match result {
        Ok(release) => {
            let tag = release
                .tag_name
                .strip_prefix('v')
                .unwrap_or(&release.tag_name);
            let is_newer = is_newer_version(tag, CURRENT_VERSION);

            next.latest_version = Some(tag.to_string());
            next.update_available = is_newer;
            next.release_url = Some(release.html_url);
            next.release_notes = release.body;

            if is_newer {
                tracing::info!(
                    current = CURRENT_VERSION,
                    latest = tag,
                    "new version available"
                );
            }
        }
        Err(error) => {
            tracing::warn!(%error, "failed to check for updates");
            next.error = Some(error.to_string());
        }
    }

    status.store(Arc::new(next));
}

/// Spawn a background task that checks for updates periodically.
pub fn spawn_update_checker(status: SharedUpdateStatus) {
    tokio::spawn(async move {
        // Initial check after a short delay to not block startup
        tokio::time::sleep(Duration::from_secs(10)).await;
        check_for_update(&status).await;

        loop {
            tokio::time::sleep(CHECK_INTERVAL).await;
            check_for_update(&status).await;
        }
    });
}

/// Fetch the latest release from GitHub.
async fn fetch_latest_release() -> anyhow::Result<GitHubRelease> {
    let url = format!(
        "https://api.github.com/repos/{}/releases/latest",
        GITHUB_REPO
    );

    let client = reqwest::Client::builder()
        .user_agent(format!("spacebot/{}", CURRENT_VERSION))
        .timeout(Duration::from_secs(15))
        .build()?;

    let response = client.get(&url).send().await?;

    if !response.status().is_success() {
        anyhow::bail!("GitHub API returned {}", response.status());
    }

    Ok(response.json().await?)
}

/// Compare two semver strings. Returns true if `latest` is newer than `current`.
fn is_newer_version(latest: &str, current: &str) -> bool {
    let Ok(latest) = semver::Version::parse(latest) else {
        return false;
    };
    let Ok(current) = semver::Version::parse(current) else {
        return false;
    };
    latest > current
}

/// Whether the host can apply updates in-place.
///
/// This build has no in-place updater (the Docker path was removed), so this
/// is always `false` and carries the reason. The `UpdateStatus` shape is kept
/// so API/CLI consumers continue to work unchanged.
#[derive(Debug, Clone)]
struct ApplyCapability {
    can_apply: bool,
    cannot_apply_reason: Option<String>,
    docker_image: Option<String>,
}

fn detect_apply_capability(deployment: Deployment) -> ApplyCapability {
    let reason = match deployment {
        Deployment::Docker => SELF_UPDATE_DISABLED_REASON,
        Deployment::Hosted => {
            "Hosted instances are updated by platform rollout, not self-service."
        }
        Deployment::Native => "Native/source installs update manually (rebuild + restart).",
    };
    ApplyCapability {
        can_apply: false,
        cannot_apply_reason: Some(reason.to_string()),
        docker_image: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_newer_version() {
        assert!(is_newer_version("0.2.0", "0.1.0"));
        assert!(is_newer_version("1.0.0", "0.9.9"));
        assert!(!is_newer_version("0.1.0", "0.1.0"));
        assert!(!is_newer_version("0.0.9", "0.1.0"));
    }

    #[test]
    fn apply_capability_is_disabled_everywhere() {
        for deployment in [Deployment::Docker, Deployment::Hosted, Deployment::Native] {
            let cap = detect_apply_capability(deployment);
            assert!(!cap.can_apply, "{deployment:?} must not be self-updatable");
            assert!(cap.cannot_apply_reason.is_some());
        }
    }
}

