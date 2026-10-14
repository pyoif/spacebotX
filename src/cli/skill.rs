//! `spacebot skill` — skill installation and lifecycle.
//!
//! Operates on the filesystem and per-agent SQLite directly rather than the
//! control API: skill installs are useful before the daemon has ever run,
//! and the usage store connects only the SQLite pool so the CLI never
//! contends for the redb lock a running daemon holds.

use anyhow::Context as _;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum SkillCommand {
    /// Install a skill from GitHub or skills.sh registry
    Add {
        /// Skill spec: owner/repo or owner/repo/skill-name
        spec: String,
        /// Agent ID to install for (defaults to first agent)
        #[arg(short, long)]
        agent: Option<String>,
        /// Install to instance-level skills directory (shared across all agents)
        #[arg(short, long)]
        instance: bool,
    },
    /// Install a skill from a .skill file
    Install {
        /// Path to .skill file
        path: std::path::PathBuf,
        /// Agent ID to install for (defaults to first agent)
        #[arg(short, long)]
        agent: Option<String>,
        /// Install to instance-level skills directory (shared across all agents)
        #[arg(short, long)]
        instance: bool,
    },
    /// List installed skills
    List {
        /// Agent ID (defaults to first agent)
        #[arg(short, long)]
        agent: Option<String>,
    },
    /// Remove an installed skill
    Remove {
        /// Skill name
        name: String,
        /// Agent ID (defaults to first agent)
        #[arg(short, long)]
        agent: Option<String>,
    },
    /// Show skill details
    Info {
        /// Skill name
        name: String,
        /// Agent ID (defaults to first agent)
        #[arg(short, long)]
        agent: Option<String>,
    },
    /// Pin a skill: blocks deletion, and all autonomous modification
    Pin {
        /// Skill name
        name: String,
        /// Agent ID (defaults to first agent)
        #[arg(short, long)]
        agent: Option<String>,
    },
    /// Unpin a skill
    Unpin {
        /// Skill name
        name: String,
        /// Agent ID (defaults to first agent)
        #[arg(short, long)]
        agent: Option<String>,
    },
    /// Hand a skill to automated curation (sets created_by = agent)
    Adopt {
        /// Skill name
        name: String,
        /// Agent ID (defaults to first agent)
        #[arg(short, long)]
        agent: Option<String>,
    },
    /// Archive a workspace skill (recoverable via restore)
    Archive {
        /// Skill name
        name: String,
        /// Agent ID (defaults to first agent)
        #[arg(short, long)]
        agent: Option<String>,
    },
    /// Restore the most recently archived copy of a skill
    Restore {
        /// Skill name
        name: String,
        /// Agent ID (defaults to first agent)
        #[arg(short, long)]
        agent: Option<String>,
    },
}

pub async fn run(ctx: &super::Context, skill_cmd: SkillCommand) -> anyhow::Result<()> {
    let config = super::load_config(&ctx.config_path)?;

    match skill_cmd {
        SkillCommand::Add {
            spec,
            agent,
            instance,
        } => {
            let target_dir = resolve_skills_dir(&config, agent.as_deref(), instance)?;

            println!("Installing skill from: {spec}");
            println!("Target directory: {}", target_dir.display());

            let installed = spacebot::skills::install_from_github(&spec, &target_dir)
                .await
                .context("failed to install skill")?;

            println!("\nSuccessfully installed {} skill(s):", installed.len());
            for name in installed {
                println!("  - {name}");
            }

            Ok(())
        }
        SkillCommand::Install {
            path,
            agent,
            instance,
        } => {
            let target_dir = resolve_skills_dir(&config, agent.as_deref(), instance)?;

            println!("Installing skill from: {}", path.display());
            println!("Target directory: {}", target_dir.display());

            let installed = spacebot::skills::install_from_file(&path, &target_dir)
                .await
                .context("failed to install skill")?;

            println!("\nSuccessfully installed {} skill(s):", installed.len());
            for name in installed {
                println!("  - {name}");
            }

            Ok(())
        }
        SkillCommand::List { agent } => {
            let (instance_dir, workspace_dir) = resolve_skill_dirs(&config, agent.as_deref())?;

            let skills = spacebot::skills::SkillSet::load(&instance_dir, &workspace_dir).await;

            if skills.is_empty() {
                println!("No skills installed");
                return Ok(());
            }

            println!("Installed skills ({}):\n", skills.len());

            for info in skills.list() {
                let source_label = match info.source {
                    spacebot::skills::SkillSource::Builtin => "builtin",
                    spacebot::skills::SkillSource::Instance => "instance",
                    spacebot::skills::SkillSource::Workspace => "workspace",
                };

                println!("  {} ({})", info.name, source_label);
                if !info.description.is_empty() {
                    println!("    {}", info.description);
                }
                println!();
            }

            Ok(())
        }
        SkillCommand::Remove { name, agent } => {
            let (instance_dir, workspace_dir) = resolve_skill_dirs(&config, agent.as_deref())?;

            let mut skills = spacebot::skills::SkillSet::load(&instance_dir, &workspace_dir).await;

            match skills.remove(&name).await? {
                Some(path) => {
                    println!("Removed skill: {name}");
                    println!("Path: {}", path.display());
                }
                None => {
                    eprintln!("Skill not found: {name}");
                    std::process::exit(1);
                }
            }

            Ok(())
        }
        SkillCommand::Info { name, agent } => {
            let (instance_dir, workspace_dir) = resolve_skill_dirs(&config, agent.as_deref())?;

            let skills = spacebot::skills::SkillSet::load(&instance_dir, &workspace_dir).await;

            let Some(skill) = skills.get(&name) else {
                eprintln!("Skill not found: {name}");
                std::process::exit(1);
            };

            let source_label = match skill.source {
                spacebot::skills::SkillSource::Builtin => "builtin",
                spacebot::skills::SkillSource::Instance => "instance",
                spacebot::skills::SkillSource::Workspace => "workspace",
            };

            println!("Skill: {}", skill.name);
            println!("Description: {}", skill.description);
            println!("Source: {source_label}");
            println!("Path: {}", skill.file_path.display());
            println!("Base directory: {}", skill.base_dir.display());

            match open_skill_usage_store(&config, agent.as_deref()).await {
                Ok(store) => match store.get(&name).await {
                    Ok(Some(record)) => {
                        println!("Created by: {}", record.created_by);
                        println!("State: {}", record.state);
                        println!("Pinned: {}", record.pinned);
                        println!(
                            "Reads: {} (last: {})",
                            record.read_count,
                            record.last_read_at.as_deref().unwrap_or("never")
                        );
                        println!(
                            "Patches: {} (last: {})",
                            record.patch_count,
                            record.last_patched_at.as_deref().unwrap_or("never")
                        );
                    }
                    Ok(None) => {}
                    Err(error) => eprintln!("(usage data unavailable: {error})"),
                },
                Err(error) => eprintln!("(usage data unavailable: {error})"),
            }

            // Show a preview of the content
            let total_chars = skill.content.chars().count();
            if total_chars > 500 {
                let preview: String = skill.content.chars().take(500).collect();
                println!("\nContent preview (first 500 chars):\n");
                println!("{preview}");
                println!("\n... ({} more characters)", total_chars - 500);
            } else {
                println!("\nContent:\n");
                println!("{}", skill.content);
            }

            Ok(())
        }
        SkillCommand::Pin { name, agent } => {
            let store = open_skill_usage_store(&config, agent.as_deref()).await?;
            store.set_pinned(&name, true).await?;
            println!("Pinned skill: {name}");
            Ok(())
        }
        SkillCommand::Unpin { name, agent } => {
            let store = open_skill_usage_store(&config, agent.as_deref()).await?;
            store.set_pinned(&name, false).await?;
            println!("Unpinned skill: {name}");
            Ok(())
        }
        SkillCommand::Adopt { name, agent } => {
            let store = open_skill_usage_store(&config, agent.as_deref()).await?;
            store.adopt(&name).await?;
            println!("Adopted skill into curation: {name}");
            Ok(())
        }
        SkillCommand::Archive { name, agent } => {
            let (instance_dir, workspace_dir) = resolve_skill_dirs(&config, agent.as_deref())?;
            let skills = spacebot::skills::SkillSet::load(&instance_dir, &workspace_dir).await;

            let Some(skill) = skills.get(&name) else {
                eprintln!("Skill not found: {name}");
                std::process::exit(1);
            };
            if skill.source != spacebot::skills::SkillSource::Workspace {
                eprintln!("Only workspace skills can be archived: {name}");
                std::process::exit(1);
            }

            // Open the store before moving the directory, so a store
            // failure can't leave the skill archived on disk with a row
            // still reporting it active.
            let store = open_skill_usage_store(&config, agent.as_deref()).await?;
            if let Some(record) = store.get(&name).await?
                && record.pinned
            {
                eprintln!("Skill is pinned; unpin it before archiving: {name}");
                std::process::exit(1);
            }

            let archived = spacebot::skills::archive_skill_dir(
                &workspace_dir,
                &skill.base_dir,
                &name.to_lowercase(),
            )
            .await?;

            store.set_archived(&name).await?;

            println!("Archived skill: {name}");
            println!("Path: {}", archived.display());
            Ok(())
        }
        SkillCommand::Restore { name, agent } => {
            let (_, workspace_dir) = resolve_skill_dirs(&config, agent.as_deref())?;

            let store = open_skill_usage_store(&config, agent.as_deref()).await?;

            let restored =
                spacebot::skills::restore_skill_dir(&workspace_dir, &name.to_lowercase()).await?;

            store.set_restored(&name).await?;

            println!("Restored skill: {name}");
            println!("Path: {}", restored.display());
            Ok(())
        }
    }
}

/// Open the agent's per-agent SQLite for skill usage updates.
///
/// Connects only the SQLite pool — not the full `Db` bundle — so the CLI
/// never contends for the redb lock a running daemon holds.
async fn open_skill_usage_store(
    config: &spacebot::config::Config,
    agent_id: Option<&str>,
) -> anyhow::Result<spacebot::skills::SkillUsageStore> {
    let agent_config = get_agent_config(config, agent_id)?;
    let resolved = agent_config.resolve(&config.instance_dir, &config.defaults);

    let db_path = resolved.data_dir.join("agent.db");
    if !db_path.exists() {
        anyhow::bail!(
            "agent database not found at {} — has the agent run at least once?",
            db_path.display()
        );
    }

    // Foreign keys ON (see spacebot::db::connect_sqlite): the agent schema's
    // ON DELETE CASCADE constraints must actually fire.
    let pool = spacebot::db::connect_sqlite(&format!("sqlite:{}", db_path.display())).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;

    Ok(spacebot::skills::SkillUsageStore::new(pool))
}

fn resolve_skills_dir(
    config: &spacebot::config::Config,
    agent_id: Option<&str>,
    instance: bool,
) -> anyhow::Result<std::path::PathBuf> {
    if instance {
        Ok(config.skills_dir())
    } else {
        let agent_config = get_agent_config(config, agent_id)?;
        let resolved = agent_config.resolve(&config.instance_dir, &config.defaults);
        Ok(resolved.skills_dir())
    }
}

fn resolve_skill_dirs(
    config: &spacebot::config::Config,
    agent_id: Option<&str>,
) -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
    let agent_config = get_agent_config(config, agent_id)?;
    let resolved = agent_config.resolve(&config.instance_dir, &config.defaults);
    Ok((config.skills_dir(), resolved.skills_dir()))
}

fn get_agent_config<'a>(
    config: &'a spacebot::config::Config,
    agent_id: Option<&str>,
) -> anyhow::Result<&'a spacebot::config::AgentConfig> {
    let agent_id = match agent_id {
        Some(id) => id,
        None => {
            if config.agents.is_empty() {
                anyhow::bail!("no agents configured");
            }
            &config.agents[0].id
        }
    };

    config
        .agents
        .iter()
        .find(|a| a.id == agent_id)
        .with_context(|| format!("agent not found: {agent_id}"))
}
