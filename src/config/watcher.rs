use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{
    Binding, Config, DiscordPermissions, MattermostPermissions, RuntimeConfig, SignalPermissions,
    SlackPermissions, TelegramPermissions, binding_runtime_adapter_key,
};
use sha2::{Digest, Sha256};

/// Per-agent context needed by the file watcher: (id, prompt_dir, identity_dir,
/// runtime_config, mcp_manager).
type WatchedAgent = (
    String,
    PathBuf,
    PathBuf,
    Arc<RuntimeConfig>,
    Arc<crate::mcp::McpManager>,
);

pub struct FileWatcherHandle {
    shutdown_tx: std::sync::mpsc::Sender<()>,
    _task: tokio::task::JoinHandle<()>,
}

impl Drop for FileWatcherHandle {
    fn drop(&mut self) {
        self.shutdown_tx.send(()).ok();
    }
}

/// Watches config, prompt, identity, and skill files for changes and triggers
/// hot reload on the corresponding RuntimeConfig.
///
/// Returns a JoinHandle that runs until dropped. File events are debounced
/// to 2 seconds so rapid edits (e.g. :w in vim hitting multiple writes) are
/// collapsed into a single reload.
#[allow(clippy::too_many_arguments)]
pub fn spawn_file_watcher(
    config_path: PathBuf,
    instance_dir: PathBuf,
    agents: Vec<WatchedAgent>,
    discord_permissions: Option<Arc<arc_swap::ArcSwap<DiscordPermissions>>>,
    slack_permissions: Option<Arc<arc_swap::ArcSwap<SlackPermissions>>>,
    telegram_permissions: Option<Arc<arc_swap::ArcSwap<TelegramPermissions>>>,
    mattermost_permissions: Option<Arc<arc_swap::ArcSwap<MattermostPermissions>>>,
    signal_permissions: Option<Arc<arc_swap::ArcSwap<SignalPermissions>>>,
    bindings: Arc<arc_swap::ArcSwap<Vec<Binding>>>,
    authority_defaults: Arc<arc_swap::ArcSwap<crate::commands::access::AdapterAuthorityDefaults>>,
    messaging_manager: Option<Arc<crate::messaging::MessagingManager>>,
    llm_manager: Arc<crate::llm::LlmManager>,
    agent_links: Arc<arc_swap::ArcSwap<Vec<crate::links::AgentLink>>>,
    agent_humans: Arc<arc_swap::ArcSwap<Vec<crate::config::HumanDef>>>,
) -> FileWatcherHandle {
    use notify::{Event, RecursiveMode, Watcher};
    use std::time::Duration;

    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();
    let task = tokio::task::spawn_blocking(move || {
        let (tx, rx) = std::sync::mpsc::channel::<Event>();

        let mut watcher = match notify::recommended_watcher(
            move |result: std::result::Result<Event, notify::Error>| {
                if let Ok(event) = result {
                    // Only forward data modification events, not metadata/access changes
                    use notify::EventKind;
                    match &event.kind {
                        EventKind::Create(_)
                        | EventKind::Modify(notify::event::ModifyKind::Data(_))
                        | EventKind::Remove(_) => {
                            let _ = tx.send(event);
                        }
                        // Also forward Any/Other modify events (some backends don't distinguish)
                        EventKind::Modify(notify::event::ModifyKind::Any) => {
                            let _ = tx.send(event);
                        }
                        _ => {}
                    }
                }
            },
        ) {
            Ok(w) => w,
            Err(error) => {
                tracing::error!(%error, "failed to create file watcher");
                return;
            }
        };

        // Watch config.toml
        if let Err(error) = watcher.watch(&config_path, RecursiveMode::NonRecursive) {
            tracing::warn!(%error, path = %config_path.display(), "failed to watch config file");
        }

        // Watch skills directories. Roots are created before watching so a
        // dir that doesn't exist yet at startup is still covered, and kept
        // for prefix-matching changed paths against actual skills roots.
        let mut skill_roots: Vec<PathBuf> = Vec::new();
        skill_roots.push(instance_dir.join("skills"));
        for (_, workspace, _, _, _) in &agents {
            skill_roots.push(workspace.join("skills"));
        }
        for root in &skill_roots {
            if let Err(error) = std::fs::create_dir_all(root) {
                tracing::warn!(%error, path = %root.display(), "failed to create skills dir");
                continue;
            }
            if let Err(error) = watcher.watch(root, RecursiveMode::Recursive) {
                tracing::warn!(%error, path = %root.display(), "failed to watch skills dir");
            }
        }

        // Watch per-agent directories
        for (_, _, identity_dir, _, _) in &agents {
            // Watch the agent root (identity_dir) for SOUL.md/IDENTITY.md/ROLE.md changes.
            // Identity files live outside the workspace, in the agent root directory.
            if let Err(error) = watcher.watch(identity_dir, RecursiveMode::NonRecursive) {
                tracing::warn!(%error, path = %identity_dir.display(), "failed to watch identity dir");
            }
        }

        tracing::info!("file watcher started");

        // Track config.toml content hash to skip no-op reloads
        let mut last_config_hash: u64 = std::fs::read(&config_path)
            .map(|bytes| {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                bytes.hash(&mut hasher);
                hasher.finish()
            })
            .unwrap_or(0);

        // Debounce loop: collect events for 2 seconds, then reload
        let debounce = Duration::from_secs(2);

        loop {
            if shutdown_rx.try_recv().is_ok() {
                break;
            }
            let first = match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(first) => first,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };
            // Drain any additional events within the debounce window
            let mut changed_paths: Vec<PathBuf> = first.paths;
            while let Ok(event) = rx.recv_timeout(debounce) {
                if shutdown_rx.try_recv().is_ok() {
                    return;
                }
                changed_paths.extend(event.paths);
            }

            // Categorize what changed
            let mut config_changed = changed_paths.iter().any(|p| p.ends_with("config.toml"));
            let identity_changed = changed_paths.iter().any(|p| {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                matches!(name, "SOUL.md" | "IDENTITY.md" | "ROLE.md")
            });
            let skills_changed = changed_paths
                .iter()
                .any(|p| skill_roots.iter().any(|root| p.starts_with(root)));

            // Skip entirely if nothing relevant changed
            if !config_changed && !identity_changed && !skills_changed {
                continue;
            }

            // Skip config reload if file content hasn't actually changed
            if config_changed {
                let current_hash: u64 = std::fs::read(&config_path)
                    .map(|bytes| {
                        use std::hash::{Hash, Hasher};
                        let mut hasher = std::collections::hash_map::DefaultHasher::new();
                        bytes.hash(&mut hasher);
                        hasher.finish()
                    })
                    .unwrap_or(0);
                if current_hash == last_config_hash {
                    config_changed = false;
                    // If config was the only thing that "changed", skip entirely
                    if !identity_changed && !skills_changed {
                        continue;
                    }
                } else {
                    last_config_hash = current_hash;
                }
            }

            let changed_summary: Vec<&str> = [
                config_changed.then_some("config"),
                identity_changed.then_some("identity"),
                skills_changed.then_some("skills"),
            ]
            .into_iter()
            .flatten()
            .collect();

            tracing::info!(
                changed = %changed_summary.join(", "),
                "file change detected, reloading"
            );

            // Reload config.toml if it changed
            let new_config = if config_changed {
                match Config::load_from_path(&config_path) {
                    Ok(config) => Some(config),
                    Err(error) => {
                        tracing::error!(%error, "failed to reload config.toml, keeping previous values");
                        None
                    }
                }
            } else {
                None
            };

            // Reload instance-level bindings, provider keys, and permissions
            if let Some(config) = &new_config {
                llm_manager.reload_config(config.llm.clone());

                bindings.store(Arc::new(config.bindings.clone()));
                tracing::info!("bindings reloaded ({} entries)", config.bindings.len());

                authority_defaults.store(Arc::new(
                    crate::commands::access::AdapterAuthorityDefaults::from_config(config),
                ));

                match crate::links::AgentLink::from_config(&config.links) {
                    Ok(links) => {
                        agent_links.store(Arc::new(links));
                        tracing::info!("agent links reloaded ({} entries)", config.links.len());
                    }
                    Err(error) => {
                        tracing::error!(%error, "failed to parse links from reloaded config");
                    }
                }

                agent_humans.store(Arc::new(config.humans.clone()));
                tracing::info!("agent humans reloaded ({} entries)", config.humans.len());

                if let Some(ref perms) = discord_permissions
                    && let Some(discord_config) = &config.messaging.discord
                {
                    let new_perms =
                        DiscordPermissions::from_config(discord_config, &config.bindings);
                    perms.store(Arc::new(new_perms));
                    tracing::info!("discord permissions reloaded");
                }

                if let Some(ref perms) = slack_permissions
                    && let Some(slack_config) = &config.messaging.slack
                {
                    let new_perms = SlackPermissions::from_config(slack_config, &config.bindings);
                    perms.store(Arc::new(new_perms));
                    tracing::info!("slack permissions reloaded");
                }

                if let Some(ref perms) = telegram_permissions
                    && let Some(telegram_config) = &config.messaging.telegram
                {
                    let new_perms =
                        TelegramPermissions::from_config(telegram_config, &config.bindings);
                    perms.store(Arc::new(new_perms));
                    tracing::info!("telegram permissions reloaded");
                }

                if let Some(ref perms) = mattermost_permissions
                    && let Some(mattermost_config) = &config.messaging.mattermost
                {
                    let new_perms =
                        MattermostPermissions::from_config(mattermost_config, &config.bindings);
                    perms.store(Arc::new(new_perms));
                    tracing::info!("mattermost permissions reloaded");
                }

                if let Some(ref perms) = signal_permissions
                    && let Some(signal_config) = &config.messaging.signal
                {
                    let new_perms = SignalPermissions::from_config(signal_config);
                    perms.store(Arc::new(new_perms));
                    tracing::info!("signal permissions reloaded");
                }

                // Reconcile adapter runtime state with the new config: start
                // newly enabled adapters, stop removed ones, restart changed ones.
                if let Some(ref manager) = messaging_manager {
                    let rt = tokio::runtime::Handle::current();
                    let manager = manager.clone();
                    let config = config.clone();
                    let discord_permissions = discord_permissions.clone();
                    let slack_permissions = slack_permissions.clone();
                    let telegram_permissions = telegram_permissions.clone();
                    let mattermost_permissions = mattermost_permissions.clone();
                    let signal_permissions = signal_permissions.clone();
                    let instance_dir = instance_dir.clone();

                    rt.spawn(async move {
                        match build_desired_configured_adapters(
                            &config,
                            &instance_dir,
                            discord_permissions,
                            slack_permissions,
                            telegram_permissions,
                            mattermost_permissions,
                            signal_permissions,
                        ) {
                            Ok(desired) => {
                                if let Err(error) = manager.reconcile_configured(desired).await {
                                    tracing::warn!(%error, "messaging adapter reconciliation encountered errors");
                                }
                            }
                            Err(error) => {
                                tracing::error!(%error, "failed to build desired messaging adapters from config change");
                            }
                        }
                    });
                }
            }

            // Apply reloads to each agent's RuntimeConfig
            for (agent_id, workspace, identity_dir, runtime_config, mcp_manager) in &agents {
                if let Some(config) = &new_config {
                    let rt = tokio::runtime::Handle::current();
                    rt.block_on(runtime_config.reload_config(config, agent_id, mcp_manager));
                }

                if identity_changed {
                    let rt = tokio::runtime::Handle::current();
                    let identity = rt.block_on(crate::identity::Identity::load(identity_dir));
                    runtime_config.reload_identity(identity);
                }

                if skills_changed {
                    let rt = tokio::runtime::Handle::current();
                    rt.block_on(async {
                        let skills = crate::skills::SkillSet::load(
                            &instance_dir.join("skills"),
                            &workspace.join("skills"),
                        )
                        .await;
                        runtime_config.reload_skills(skills).await;
                    });
                }
            }
        }

        tracing::info!("file watcher stopped");
    });

    FileWatcherHandle {
        shutdown_tx,
        _task: task,
    }
}

#[allow(clippy::too_many_arguments)]
fn build_desired_configured_adapters(
    config: &Config,
    instance_dir: &Path,
    discord_permissions: Option<Arc<arc_swap::ArcSwap<DiscordPermissions>>>,
    slack_permissions: Option<Arc<arc_swap::ArcSwap<SlackPermissions>>>,
    telegram_permissions: Option<Arc<arc_swap::ArcSwap<TelegramPermissions>>>,
    mattermost_permissions: Option<Arc<arc_swap::ArcSwap<MattermostPermissions>>>,
    signal_permissions: Option<Arc<arc_swap::ArcSwap<SignalPermissions>>>,
) -> anyhow::Result<Vec<crate::messaging::ConfiguredAdapter>> {
    let mut desired = Vec::new();

    if let Some(discord_config) = &config.messaging.discord
        && discord_config.enabled
    {
        if !discord_config.token.is_empty() {
            let permissions_snapshot =
                DiscordPermissions::from_config(discord_config, &config.bindings);
            let permissions = discord_permissions.unwrap_or_else(|| {
                Arc::new(arc_swap::ArcSwap::from_pointee(
                    permissions_snapshot.clone(),
                ))
            });
            let fingerprint = format!(
                "token={}|dm={:?}|allow_bot_messages={}|permissions={}",
                secret_fingerprint(&discord_config.token),
                sorted_strings(discord_config.dm_allowed_users.clone()),
                discord_config.allow_bot_messages,
                discord_permissions_fingerprint(&permissions_snapshot)
            );
            desired.push(crate::messaging::ConfiguredAdapter::new(
                crate::messaging::discord::DiscordAdapter::new(
                    "discord",
                    &discord_config.token,
                    permissions,
                ),
                fingerprint,
            ));
        }

        for instance in discord_config
            .instances
            .iter()
            .filter(|instance| instance.enabled)
        {
            if instance.token.is_empty() {
                continue;
            }
            let permissions_snapshot =
                DiscordPermissions::from_instance_config(instance, &config.bindings);
            let fingerprint = format!(
                "token={}|dm={:?}|allow_bot_messages={}|permissions={}",
                secret_fingerprint(&instance.token),
                sorted_strings(instance.dm_allowed_users.clone()),
                instance.allow_bot_messages,
                discord_permissions_fingerprint(&permissions_snapshot)
            );
            desired.push(crate::messaging::ConfiguredAdapter::new(
                crate::messaging::discord::DiscordAdapter::new(
                    binding_runtime_adapter_key("discord", Some(instance.name.as_str())),
                    &instance.token,
                    Arc::new(arc_swap::ArcSwap::from_pointee(permissions_snapshot)),
                ),
                fingerprint,
            ));
        }
    }

    if let Some(slack_config) = &config.messaging.slack
        && slack_config.enabled
    {
        if !slack_config.bot_token.is_empty() && !slack_config.app_token.is_empty() {
            let permissions_snapshot =
                SlackPermissions::from_config(slack_config, &config.bindings);
            let permissions = slack_permissions.unwrap_or_else(|| {
                Arc::new(arc_swap::ArcSwap::from_pointee(
                    permissions_snapshot.clone(),
                ))
            });
            let fingerprint = format!(
                "bot_token={}|app_token={}|dm={:?}|permissions={}",
                secret_fingerprint(&slack_config.bot_token),
                secret_fingerprint(&slack_config.app_token),
                sorted_strings(slack_config.dm_allowed_users.clone()),
                slack_permissions_fingerprint(&permissions_snapshot)
            );
            let adapter = crate::messaging::slack::SlackAdapter::new(
                "slack",
                &slack_config.bot_token,
                &slack_config.app_token,
                permissions,
            )?;
            desired.push(crate::messaging::ConfiguredAdapter::new(
                adapter,
                fingerprint,
            ));
        }

        for instance in slack_config
            .instances
            .iter()
            .filter(|instance| instance.enabled)
        {
            if instance.bot_token.is_empty() || instance.app_token.is_empty() {
                continue;
            }
            let permissions_snapshot =
                SlackPermissions::from_instance_config(instance, &config.bindings);
            let fingerprint = format!(
                "bot_token={}|app_token={}|dm={:?}|permissions={}",
                secret_fingerprint(&instance.bot_token),
                secret_fingerprint(&instance.app_token),
                sorted_strings(instance.dm_allowed_users.clone()),
                slack_permissions_fingerprint(&permissions_snapshot)
            );
            let adapter = crate::messaging::slack::SlackAdapter::new(
                binding_runtime_adapter_key("slack", Some(instance.name.as_str())),
                &instance.bot_token,
                &instance.app_token,
                Arc::new(arc_swap::ArcSwap::from_pointee(permissions_snapshot)),
            )?;
            desired.push(crate::messaging::ConfiguredAdapter::new(
                adapter,
                fingerprint,
            ));
        }
    }

    if let Some(telegram_config) = &config.messaging.telegram
        && telegram_config.enabled
    {
        if !telegram_config.token.is_empty() {
            let permissions_snapshot =
                TelegramPermissions::from_config(telegram_config, &config.bindings);
            let permissions = telegram_permissions.unwrap_or_else(|| {
                Arc::new(arc_swap::ArcSwap::from_pointee(
                    permissions_snapshot.clone(),
                ))
            });
            let fingerprint = format!(
                "token={}|dm={:?}|permissions={}",
                secret_fingerprint(&telegram_config.token),
                sorted_strings(telegram_config.dm_allowed_users.clone()),
                telegram_permissions_fingerprint(&permissions_snapshot)
            );
            desired.push(crate::messaging::ConfiguredAdapter::new(
                crate::messaging::telegram::TelegramAdapter::new(
                    "telegram",
                    &telegram_config.token,
                    permissions,
                ),
                fingerprint,
            ));
        }

        for instance in telegram_config
            .instances
            .iter()
            .filter(|instance| instance.enabled)
        {
            if instance.token.is_empty() {
                continue;
            }
            let permissions_snapshot =
                TelegramPermissions::from_instance_config(instance, &config.bindings);
            let fingerprint = format!(
                "token={}|dm={:?}|permissions={}",
                secret_fingerprint(&instance.token),
                sorted_strings(instance.dm_allowed_users.clone()),
                telegram_permissions_fingerprint(&permissions_snapshot)
            );
            desired.push(crate::messaging::ConfiguredAdapter::new(
                crate::messaging::telegram::TelegramAdapter::new(
                    binding_runtime_adapter_key("telegram", Some(instance.name.as_str())),
                    &instance.token,
                    Arc::new(arc_swap::ArcSwap::from_pointee(permissions_snapshot)),
                ),
                fingerprint,
            ));
        }
    }

    if let Some(email_config) = &config.messaging.email
        && email_config.enabled
    {
        if !email_config.imap_host.is_empty() {
            let fingerprint = format!(
                "imap_host={};imap_port={};imap_username={};imap_password={};imap_use_tls={};smtp_host={};smtp_port={};smtp_username={};smtp_password={};smtp_use_starttls={};from_address={};from_name={:?};poll_interval_secs={};folders={:?};allowed_senders={:?};max_body_bytes={};max_attachment_bytes={}",
                email_config.imap_host,
                email_config.imap_port,
                secret_fingerprint(&email_config.imap_username),
                secret_fingerprint(&email_config.imap_password),
                email_config.imap_use_tls,
                email_config.smtp_host,
                email_config.smtp_port,
                secret_fingerprint(&email_config.smtp_username),
                secret_fingerprint(&email_config.smtp_password),
                email_config.smtp_use_starttls,
                email_config.from_address,
                email_config.from_name,
                email_config.poll_interval_secs,
                sorted_strings(email_config.folders.clone()),
                sorted_strings(email_config.allowed_senders.clone()),
                email_config.max_body_bytes,
                email_config.max_attachment_bytes
            );
            let adapter = crate::messaging::email::EmailAdapter::from_config(email_config)?;
            desired.push(crate::messaging::ConfiguredAdapter::new(
                adapter,
                fingerprint,
            ));
        }

        for instance in email_config
            .instances
            .iter()
            .filter(|instance| instance.enabled)
        {
            if instance.imap_host.is_empty() {
                continue;
            }
            let fingerprint = format!(
                "name={};imap_host={};imap_port={};imap_username={};imap_password={};imap_use_tls={};smtp_host={};smtp_port={};smtp_username={};smtp_password={};smtp_use_starttls={};from_address={};from_name={:?};poll_interval_secs={};folders={:?};allowed_senders={:?};max_body_bytes={};max_attachment_bytes={}",
                instance.name,
                instance.imap_host,
                instance.imap_port,
                secret_fingerprint(&instance.imap_username),
                secret_fingerprint(&instance.imap_password),
                instance.imap_use_tls,
                instance.smtp_host,
                instance.smtp_port,
                secret_fingerprint(&instance.smtp_username),
                secret_fingerprint(&instance.smtp_password),
                instance.smtp_use_starttls,
                instance.from_address,
                instance.from_name,
                instance.poll_interval_secs,
                sorted_strings(instance.folders.clone()),
                sorted_strings(instance.allowed_senders.clone()),
                instance.max_body_bytes,
                instance.max_attachment_bytes
            );
            let adapter = crate::messaging::email::EmailAdapter::from_instance_config(
                binding_runtime_adapter_key("email", Some(instance.name.as_str())),
                instance,
            )?;
            desired.push(crate::messaging::ConfiguredAdapter::new(
                adapter,
                fingerprint,
            ));
        }
    }

    if let Some(webhook_config) = &config.messaging.webhook
        && webhook_config.enabled
    {
        let fingerprint = format!(
            "port={};bind={};auth_token={:?}",
            webhook_config.port,
            webhook_config.bind,
            webhook_config.auth_token.as_deref().map(secret_fingerprint)
        );
        desired.push(crate::messaging::ConfiguredAdapter::new(
            crate::messaging::webhook::WebhookAdapter::new(
                webhook_config.port,
                &webhook_config.bind,
                webhook_config.auth_token.clone(),
            ),
            fingerprint,
        ));
    }

    if let Some(mattermost_config) = &config.messaging.mattermost
        && mattermost_config.enabled
    {
        if !mattermost_config.base_url.is_empty() && !mattermost_config.token.is_empty() {
            let permissions_snapshot =
                MattermostPermissions::from_config(mattermost_config, &config.bindings);
            let permissions = mattermost_permissions.unwrap_or_else(|| {
                Arc::new(arc_swap::ArcSwap::from_pointee(
                    permissions_snapshot.clone(),
                ))
            });
            let fingerprint = format!(
                "base_url={};token={};team_id={:?};max_attachment_bytes={};permissions={}",
                mattermost_config.base_url,
                secret_fingerprint(&mattermost_config.token),
                mattermost_config.team_id,
                mattermost_config.max_attachment_bytes,
                mattermost_permissions_fingerprint(&permissions_snapshot)
            );
            match crate::messaging::mattermost::MattermostAdapter::new(
                "mattermost",
                &mattermost_config.base_url,
                mattermost_config.token.as_str(),
                mattermost_config.team_id.as_deref().map(Arc::from),
                mattermost_config.max_attachment_bytes,
                permissions,
            ) {
                Ok(adapter) => {
                    desired.push(crate::messaging::ConfiguredAdapter::new(
                        adapter,
                        fingerprint,
                    ));
                }
                Err(error) => {
                    tracing::error!(%error, "failed to build mattermost adapter from config change");
                }
            }
        }

        for instance in mattermost_config
            .instances
            .iter()
            .filter(|instance| instance.enabled)
        {
            if instance.base_url.is_empty() || instance.token.is_empty() {
                tracing::warn!(adapter = %instance.name, "skipping enabled mattermost instance with missing credentials");
                continue;
            }
            let permissions_snapshot =
                MattermostPermissions::from_instance_config(instance, &config.bindings);
            let fingerprint = format!(
                "base_url={};token={};team_id={:?};max_attachment_bytes={};permissions={}",
                instance.base_url,
                secret_fingerprint(&instance.token),
                instance.team_id,
                instance.max_attachment_bytes,
                mattermost_permissions_fingerprint(&permissions_snapshot)
            );
            match crate::messaging::mattermost::MattermostAdapter::new(
                binding_runtime_adapter_key("mattermost", Some(instance.name.as_str())),
                &instance.base_url,
                instance.token.as_str(),
                instance.team_id.as_deref().map(Arc::from),
                instance.max_attachment_bytes,
                Arc::new(arc_swap::ArcSwap::from_pointee(permissions_snapshot)),
            ) {
                Ok(adapter) => {
                    desired.push(crate::messaging::ConfiguredAdapter::new(
                        adapter,
                        fingerprint,
                    ));
                }
                Err(error) => {
                    tracing::error!(%error, adapter = %instance.name, "failed to build named mattermost adapter from config change");
                }
            }
        }
    }

    // Signal named instances start independently of the root enabled flag,
    // matching cold-start: multiple Signal accounts can run without a
    // "default" account being enabled.
    if let Some(signal_config) = &config.messaging.signal {
        let tmp_dir = instance_dir.join("tmp");
        if signal_config.enabled
            && !signal_config.http_url.is_empty()
            && !signal_config.account.is_empty()
        {
            let permissions_snapshot = SignalPermissions::from_config(signal_config);
            let permissions = signal_permissions.unwrap_or_else(|| {
                Arc::new(arc_swap::ArcSwap::from_pointee(
                    permissions_snapshot.clone(),
                ))
            });
            let fingerprint = format!(
                "http_url={};account={};ignore_stories={};permissions={}",
                secret_fingerprint(&signal_config.http_url),
                secret_fingerprint(&signal_config.account),
                signal_config.ignore_stories,
                signal_permissions_fingerprint(&permissions_snapshot)
            );
            let adapter = crate::messaging::signal::SignalAdapter::new(
                "signal",
                &signal_config.http_url,
                &signal_config.account,
                signal_config.ignore_stories,
                permissions,
                tmp_dir.clone(),
            );
            desired.push(crate::messaging::ConfiguredAdapter::new(
                adapter,
                fingerprint,
            ));
        }

        for instance in signal_config
            .instances
            .iter()
            .filter(|instance| instance.enabled)
        {
            if instance.http_url.is_empty() || instance.account.is_empty() {
                tracing::warn!(adapter = %instance.name, "skipping enabled signal instance with missing credentials");
                continue;
            }
            let permissions_snapshot = SignalPermissions::from_instance_config(instance);
            let fingerprint = format!(
                "http_url={};account={};ignore_stories={};permissions={}",
                secret_fingerprint(&instance.http_url),
                secret_fingerprint(&instance.account),
                instance.ignore_stories,
                signal_permissions_fingerprint(&permissions_snapshot)
            );
            let adapter = crate::messaging::signal::SignalAdapter::new(
                binding_runtime_adapter_key("signal", Some(instance.name.as_str())),
                &instance.http_url,
                &instance.account,
                instance.ignore_stories,
                Arc::new(arc_swap::ArcSwap::from_pointee(permissions_snapshot)),
                tmp_dir.clone(),
            );
            desired.push(crate::messaging::ConfiguredAdapter::new(
                adapter,
                fingerprint,
            ));
        }
    }

    Ok(desired)
}

fn sorted_strings(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values
}

fn sorted_u64s(mut values: Vec<u64>) -> Vec<u64> {
    values.sort_unstable();
    values
}

fn sorted_i64s(mut values: Vec<i64>) -> Vec<i64> {
    values.sort_unstable();
    values
}

fn format_u64_map(map: &std::collections::HashMap<u64, Vec<u64>>) -> String {
    let mut entries = map
        .iter()
        .map(|(key, values)| (*key, sorted_u64s(values.clone())))
        .collect::<Vec<_>>();
    entries.sort_by_key(|(key, _)| *key);
    format!("{entries:?}")
}

fn format_string_map(map: &std::collections::HashMap<String, Vec<String>>) -> String {
    let mut entries = map
        .iter()
        .map(|(key, values)| (key.clone(), sorted_strings(values.clone())))
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    format!("{entries:?}")
}

fn discord_permissions_fingerprint(permissions: &DiscordPermissions) -> String {
    format!(
        "guild_filter={:?};channel_filter={};dm_allowed_users={:?};allow_bot_messages={}",
        permissions.guild_filter.clone().map(sorted_u64s),
        format_u64_map(&permissions.channel_filter),
        sorted_u64s(permissions.dm_allowed_users.clone()),
        permissions.allow_bot_messages
    )
}

fn slack_permissions_fingerprint(permissions: &SlackPermissions) -> String {
    format!(
        "workspace_filter={:?};channel_filter={};dm_allowed_users={:?}",
        permissions.workspace_filter.clone().map(sorted_strings),
        format_string_map(&permissions.channel_filter),
        sorted_strings(permissions.dm_allowed_users.clone())
    )
}

fn telegram_permissions_fingerprint(permissions: &TelegramPermissions) -> String {
    format!(
        "chat_filter={:?};dm_allowed_users={:?}",
        permissions.chat_filter.clone().map(sorted_i64s),
        sorted_i64s(permissions.dm_allowed_users.clone())
    )
}

fn mattermost_permissions_fingerprint(permissions: &MattermostPermissions) -> String {
    format!(
        "team_filter={:?};channel_filter={};dm_allowed_users={:?}",
        permissions.team_filter.clone().map(sorted_strings),
        format_string_map(&permissions.channel_filter),
        sorted_strings(permissions.dm_allowed_users.clone())
    )
}

fn signal_permissions_fingerprint(permissions: &SignalPermissions) -> String {
    format!(
        "group_filter={:?};dm_allowed_users={:?};group_allowed_users={:?}",
        permissions.group_filter.clone().map(sorted_strings),
        sorted_strings(permissions.dm_allowed_users.clone()),
        sorted_strings(permissions.group_allowed_users.clone())
    )
}

fn secret_fingerprint(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    hex::encode(&digest[..8])
}
