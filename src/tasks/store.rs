//! Global task CRUD storage (SQLite).
//!
//! Operates against the instance-level `tasks.db` database with globally
//! unique task numbers and explicit owner/assigned agent relationships.
//!
//! Every path that materially changes a task goes through
//! [`TaskStore::update_with_dependencies_and_status_policy`] or
//! [`TaskStore::create_with_dependencies`], both of which write the task row,
//! its immutable revision, and its dependency edges in one transaction.

use crate::error::{Result, TaskError};
use crate::tasks::revisions::{TaskMutationContext, TaskRevisionSnapshot};

use anyhow::Context as _;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(test)]
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{Row as _, SqlitePool};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    PendingApproval,
    Backlog,
    Ready,
    InProgress,
    Done,
    Failed,
}

impl TaskStatus {
    pub const ALL: [TaskStatus; 6] = [
        TaskStatus::PendingApproval,
        TaskStatus::Backlog,
        TaskStatus::Ready,
        TaskStatus::InProgress,
        TaskStatus::Done,
        TaskStatus::Failed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::PendingApproval => "pending_approval",
            TaskStatus::Backlog => "backlog",
            TaskStatus::Ready => "ready",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Done => "done",
            TaskStatus::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending_approval" => Some(TaskStatus::PendingApproval),
            "backlog" => Some(TaskStatus::Backlog),
            "ready" => Some(TaskStatus::Ready),
            "in_progress" => Some(TaskStatus::InProgress),
            "done" => Some(TaskStatus::Done),
            "failed" => Some(TaskStatus::Failed),
            _ => None,
        }
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskPriority {
    Critical,
    High,
    Medium,
    Low,
}

impl TaskPriority {
    pub const ALL: [TaskPriority; 4] = [
        TaskPriority::Critical,
        TaskPriority::High,
        TaskPriority::Medium,
        TaskPriority::Low,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            TaskPriority::Critical => "critical",
            TaskPriority::High => "high",
            TaskPriority::Medium => "medium",
            TaskPriority::Low => "low",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "critical" => Some(TaskPriority::Critical),
            "high" => Some(TaskPriority::High),
            "medium" => Some(TaskPriority::Medium),
            "low" => Some(TaskPriority::Low),
            _ => None,
        }
    }
}

impl std::fmt::Display for TaskPriority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Which kind of worker executes a task.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, JsonSchema, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TaskWorkerType {
    Builtin,
    Opencode,
}

impl TaskWorkerType {
    pub const ALL: [TaskWorkerType; 2] = [TaskWorkerType::Builtin, TaskWorkerType::Opencode];

    pub fn as_str(self) -> &'static str {
        match self {
            TaskWorkerType::Builtin => "builtin",
            TaskWorkerType::Opencode => "opencode",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "builtin" => Some(TaskWorkerType::Builtin),
            "opencode" => Some(TaskWorkerType::Opencode),
            _ => None,
        }
    }
}

impl std::fmt::Display for TaskWorkerType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Where a task's worker runs relative to its project checkout.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, JsonSchema, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TaskWorktreeMode {
    /// Run in the project (or repo) root checkout.
    Root,
    /// Run in an existing worktree, selected by the task's `worktree_id`.
    Existing,
    /// Create a fresh worktree for this task at spawn time.
    Create,
}

impl TaskWorktreeMode {
    pub const ALL: [TaskWorktreeMode; 3] = [
        TaskWorktreeMode::Root,
        TaskWorktreeMode::Existing,
        TaskWorktreeMode::Create,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            TaskWorktreeMode::Root => "root",
            TaskWorktreeMode::Existing => "existing",
            TaskWorktreeMode::Create => "create",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "root" => Some(TaskWorktreeMode::Root),
            "existing" => Some(TaskWorktreeMode::Existing),
            "create" => Some(TaskWorktreeMode::Create),
            _ => None,
        }
    }
}

impl std::fmt::Display for TaskWorktreeMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, utoipa::ToSchema)]
pub struct TaskSubtask {
    pub title: String,
    pub completed: bool,
}

/// How a dependency edge blocks its dependent.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, JsonSchema, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TaskDependencyKind {
    /// The dependent needs the dependency's outcome — blocked until done.
    Gate,
    /// The dependent builds on the dependency's code — blocked only until
    /// its branch exists (worktree provisioned) or it is done.
    Stack,
}

impl TaskDependencyKind {
    pub const ALL: [TaskDependencyKind; 2] = [TaskDependencyKind::Gate, TaskDependencyKind::Stack];

    pub fn as_str(self) -> &'static str {
        match self {
            TaskDependencyKind::Gate => "gate",
            TaskDependencyKind::Stack => "stack",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "gate" => Some(TaskDependencyKind::Gate),
            "stack" => Some(TaskDependencyKind::Stack),
            _ => None,
        }
    }
}

impl std::fmt::Display for TaskDependencyKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A dependency edge as seen from the dependent task, with enough context to
/// render and to compute blockedness without another query.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TaskDependencyEdge {
    pub depends_on_task_number: i64,
    pub depends_on_title: String,
    pub depends_on_status: TaskStatus,
    pub kind: TaskDependencyKind,
    /// Whether this edge currently permits the dependent to run.
    pub satisfied: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Task {
    pub id: String,
    pub task_number: i64,
    pub title: String,
    pub description: Option<String>,
    pub status: TaskStatus,
    pub priority: TaskPriority,
    pub owner_agent_id: String,
    pub assigned_agent_id: Option<String>,
    pub subtasks: Vec<TaskSubtask>,
    pub metadata: Value,
    /// Goal this task contributes to, when linked.
    pub goal_id: Option<String>,
    pub source_memory_id: Option<String>,
    pub worker_id: Option<String>,
    /// Execution plan: which worker kind runs this task. `None` inherits the
    /// project default, or leaves the choice to the executing turn.
    pub worker_type: Option<TaskWorkerType>,
    /// Execution plan: project this task's work belongs to.
    pub project_id: Option<String>,
    /// Execution plan: repo within the project, needed when `worktree_mode`
    /// is `create` on a multi-repo project.
    pub repo_id: Option<String>,
    /// Execution plan: where the worker runs relative to the checkout.
    pub worktree_mode: Option<TaskWorktreeMode>,
    /// Execution plan: existing worktree to run in (`worktree_mode: existing`).
    pub worktree_id: Option<String>,
    /// Skills whose content is injected into the executing worker's context
    /// unconditionally — a contract, unlike advisory `suggested_skills`.
    pub required_skills: Vec<String>,
    /// Dependency edges, hydrated by the store's read paths (not stored on
    /// the tasks row).
    #[serde(default)]
    pub depends_on: Vec<TaskDependencyEdge>,
    /// Number of the task's latest revision, and the token a caller passes back
    /// as `expected_revision` to prove it is editing the version it read.
    pub revision: i64,
    pub created_by: String,
    pub approved_at: Option<String>,
    pub approved_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
}

impl Task {
    /// The agent responsible for this task: the assignee when claimed,
    /// falling back to the owner for unassigned tasks.
    pub fn effective_agent_id(&self) -> &str {
        self.assigned_agent_id
            .as_deref()
            .unwrap_or(&self.owner_agent_id)
    }

    /// Task numbers of unsatisfied dependencies.
    pub fn blocked_by(&self) -> Vec<i64> {
        self.depends_on
            .iter()
            .filter(|edge| !edge.satisfied)
            .map(|edge| edge.depends_on_task_number)
            .collect()
    }

    /// The stack parent's task number, when this task has a stack edge.
    pub fn stack_parent(&self) -> Option<i64> {
        self.depends_on
            .iter()
            .find(|edge| edge.kind == TaskDependencyKind::Stack)
            .map(|edge| edge.depends_on_task_number)
    }
}

/// Project-level execution defaults that a task's own fields override.
///
/// Kept here (rather than importing `ProjectSettings`) so plan resolution
/// stays decoupled from how projects store their settings blob.
#[derive(Debug, Clone, Default)]
pub struct ExecutionDefaults {
    pub worker_type: Option<TaskWorkerType>,
    pub worktree_mode: Option<TaskWorktreeMode>,
    pub required_skills: Vec<String>,
}

/// The resolved answer to "how and where does this task run".
///
/// Task fields win over project defaults; `required_skills` is the union,
/// project defaults first.
#[derive(Debug, Clone, Default, PartialEq, Serialize, utoipa::ToSchema)]
pub struct ExecutionPlan {
    pub worker_type: Option<TaskWorkerType>,
    pub project_id: Option<String>,
    pub repo_id: Option<String>,
    pub worktree_mode: Option<TaskWorktreeMode>,
    pub worktree_id: Option<String>,
    pub required_skills: Vec<String>,
}

impl ExecutionPlan {
    pub fn resolve(task: &Task, project_defaults: Option<&ExecutionDefaults>) -> Self {
        let defaults = project_defaults.cloned().unwrap_or_default();

        let mut required_skills = defaults.required_skills;
        for skill in &task.required_skills {
            if !required_skills
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(skill))
            {
                required_skills.push(skill.clone());
            }
        }

        Self {
            worker_type: task.worker_type.or(defaults.worker_type),
            project_id: task.project_id.clone(),
            repo_id: task.repo_id.clone(),
            worktree_mode: task.worktree_mode.or(defaults.worktree_mode),
            worktree_id: task.worktree_id.clone(),
            required_skills,
        }
    }

    /// True when nothing in the plan is specified.
    pub fn is_empty(&self) -> bool {
        self.worker_type.is_none()
            && self.project_id.is_none()
            && self.worktree_mode.is_none()
            && self.worktree_id.is_none()
            && self.required_skills.is_empty()
    }

    /// One-line human summary for prompts and approval surfaces.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(worker_type) = self.worker_type {
            parts.push(format!("worker: {worker_type}"));
        }
        if let Some(project_id) = &self.project_id {
            parts.push(format!("project: {project_id}"));
        }
        if let Some(mode) = self.worktree_mode {
            match (mode, &self.worktree_id) {
                (TaskWorktreeMode::Existing, Some(id)) => {
                    parts.push(format!("worktree: existing ({id})"));
                }
                _ => parts.push(format!("worktree: {mode}")),
            }
        }
        if !self.required_skills.is_empty() {
            parts.push(format!(
                "requires skills: {}",
                self.required_skills.join(", ")
            ));
        }
        parts.join(" · ")
    }
}

#[derive(Debug, Clone)]
pub struct CreateTaskInput {
    pub owner_agent_id: String,
    pub assigned_agent_id: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub status: TaskStatus,
    pub priority: TaskPriority,
    pub subtasks: Vec<TaskSubtask>,
    pub metadata: Value,
    pub source_memory_id: Option<String>,
    pub created_by: String,
    pub worker_type: Option<TaskWorkerType>,
    pub project_id: Option<String>,
    pub repo_id: Option<String>,
    pub worktree_mode: Option<TaskWorktreeMode>,
    pub worktree_id: Option<String>,
    pub required_skills: Vec<String>,
    /// Attribution recorded on the task's revision 1.
    pub context: TaskMutationContext,
}

impl Default for CreateTaskInput {
    fn default() -> Self {
        Self {
            owner_agent_id: String::new(),
            assigned_agent_id: None,
            title: String::new(),
            description: None,
            status: TaskStatus::Backlog,
            priority: TaskPriority::Medium,
            subtasks: Vec::new(),
            metadata: serde_json::json!({}),
            source_memory_id: None,
            created_by: String::new(),
            worker_type: None,
            project_id: None,
            repo_id: None,
            worktree_mode: None,
            worktree_id: None,
            required_skills: Vec::new(),
            context: TaskMutationContext::default(),
        }
    }
}

/// A nullable field in an update: absent leaves the stored value alone, present
/// sets it — including to `None`, which restoring a revision that cleared the
/// field depends on.
pub type Patch<T> = Option<Option<T>>;

/// Resolve a patch against the value currently stored.
fn patch<T>(patch: Patch<T>, current: Option<T>) -> Option<T> {
    match patch {
        Some(next) => next,
        None => current,
    }
}

#[derive(Debug, Clone, Default)]
pub struct UpdateTaskInput {
    pub title: Option<String>,
    pub description: Patch<String>,
    pub status: Option<TaskStatus>,
    pub priority: Option<TaskPriority>,
    pub subtasks: Option<Vec<TaskSubtask>>,
    pub metadata: Option<Value>,
    /// Replace metadata outright instead of deep-merging the patch into it.
    /// Restore sets this so a key removed in the restored revision stays gone.
    pub replace_metadata: bool,
    pub worker_id: Option<String>,
    pub clear_worker_id: bool,
    pub approved_by: Option<String>,
    pub complete_subtask: Option<usize>,
    /// Reassign the task to a different agent, or unassign it.
    pub assigned_agent_id: Patch<String>,
    pub worker_type: Patch<TaskWorkerType>,
    pub project_id: Patch<String>,
    pub repo_id: Patch<String>,
    pub worktree_mode: Patch<TaskWorktreeMode>,
    pub worktree_id: Patch<String>,
    pub goal_id: Patch<String>,
    pub required_skills: Option<Vec<String>>,
    /// Attribution and optimistic-concurrency expectations for this mutation.
    pub context: TaskMutationContext,
}

#[derive(Debug, Clone)]
pub struct TaskUpdateResult {
    pub previous_status: TaskStatus,
    pub task: Task,
    /// The revision this mutation appended, or `None` when nothing material
    /// changed and no history was written.
    pub new_revision: Option<i64>,
}

#[derive(Debug, Clone)]
pub enum WorkerTaskUpdateResult {
    Updated(Box<TaskUpdateResult>),
    NotAssigned,
    WrongTask { assigned_task_number: i64 },
}

/// Filters for listing tasks from the global store.
#[derive(Debug, Clone, Default)]
pub struct TaskListFilter {
    /// Convenience: matches tasks where `owner_agent_id` OR `assigned_agent_id`
    /// equals this value. Mutually exclusive with the individual fields below.
    pub agent_id: Option<String>,
    pub owner_agent_id: Option<String>,
    pub assigned_agent_id: Option<String>,
    pub status: Option<TaskStatus>,
    pub priority: Option<TaskPriority>,
    pub created_by: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct TaskStore {
    pool: SqlitePool,
}

impl TaskStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Maximum number of retries when a concurrent create races on the
    /// `task_number` UNIQUE constraint.
    const MAX_CREATE_RETRIES: usize = 3;

    pub async fn create(&self, input: CreateTaskInput) -> Result<Task> {
        self.create_with_dependencies(input, &[]).await
    }

    /// Create a task and its initial dependency edges atomically.
    pub async fn create_with_dependencies(
        &self,
        input: CreateTaskInput,
        edges: &[(i64, TaskDependencyKind)],
    ) -> Result<Task> {
        let subtasks_json =
            serde_json::to_string(&input.subtasks).context("failed to serialize subtasks")?;
        let metadata_json = input.metadata.to_string();
        let required_skills_json = serde_json::to_string(&input.required_skills)
            .context("failed to serialize required skills")?;
        input.context.validate()?;

        for attempt in 0..Self::MAX_CREATE_RETRIES {
            let mut tx = self
                .pool
                .begin()
                .await
                .context("failed to open task create transaction")?;

            // Atomically allocate the next task number from the high-water-mark
            // sequence. This avoids number reuse after hard deletes.
            let task_number: i64 = sqlx::query_scalar(
                "UPDATE task_number_seq SET next_number = next_number + 1 \
                 WHERE id = 1 RETURNING next_number - 1",
            )
            .fetch_one(&mut *tx)
            .await
            .context("failed to allocate next task number")?;

            let task_id = uuid::Uuid::new_v4().to_string();

            let insert_result = sqlx::query(
                r#"
                INSERT INTO tasks (
                    id, task_number, title, description, status, priority,
                    owner_agent_id, assigned_agent_id,
                    subtasks, metadata, source_memory_id, created_by,
                    worker_type, project_id, repo_id, worktree_mode, worktree_id,
                    required_skills
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                "#,
            )
            .bind(&task_id)
            .bind(task_number)
            .bind(&input.title)
            .bind(&input.description)
            .bind(input.status.as_str())
            .bind(input.priority.as_str())
            .bind(&input.owner_agent_id)
            .bind(&input.assigned_agent_id)
            .bind(&subtasks_json)
            .bind(&metadata_json)
            .bind(&input.source_memory_id)
            .bind(&input.created_by)
            .bind(input.worker_type.map(TaskWorkerType::as_str))
            .bind(&input.project_id)
            .bind(&input.repo_id)
            .bind(input.worktree_mode.map(TaskWorktreeMode::as_str))
            .bind(&input.worktree_id)
            .bind(&required_skills_json)
            .execute(&mut *tx)
            .await;

            match insert_result {
                Ok(_) => {
                    let task = sqlx::query(&format!(
                        "{SELECT_COLUMNS} FROM tasks WHERE task_number = ?"
                    ))
                    .bind(task_number)
                    .fetch_one(&mut *tx)
                    .await
                    .context("failed to fetch created task")?;
                    let task = task_from_row(task)?;

                    Self::replace_dependencies_in_tx(&mut tx, &task, edges).await?;

                    // Revision 1 and the task row commit together: a task can
                    // never exist without the snapshot it started from.
                    let dependencies = Self::dependency_pairs_in_tx(&mut tx, &task.id).await?;
                    let snapshot = TaskRevisionSnapshot::capture(&task, &dependencies);
                    Self::insert_revision_in_tx(
                        &mut tx,
                        &task.id,
                        0,
                        &snapshot,
                        &input.context,
                        None,
                    )
                    .await?;

                    tx.commit()
                        .await
                        .context("failed to commit task create transaction")?;

                    return self
                        .get_by_number(task_number)
                        .await?
                        .context("task inserted but not found")
                        .map_err(Into::into);
                }
                Err(sqlx::Error::Database(ref db_error))
                    if db_error.code().as_deref() == Some("2067") =>
                {
                    // UNIQUE constraint violation — another concurrent create won the
                    // race for this task_number. Roll back and retry.
                    tracing::debug!(attempt, task_number, "task_number collision, retrying");
                    // tx is dropped here which rolls back automatically.
                    continue;
                }
                Err(error) => {
                    return Err(anyhow::anyhow!("failed to insert task: {error}").into());
                }
            }
        }

        Err(anyhow::anyhow!(
            "failed to create task after {} retries due to concurrent task_number collisions",
            Self::MAX_CREATE_RETRIES
        )
        .into())
    }

    /// List tasks with optional filters. Uses the global store — no agent_id
    /// is required, but callers can filter by owner or assigned agent.
    pub async fn list(&self, filter: TaskListFilter) -> Result<Vec<Task>> {
        let mut query = String::from(SELECT_COLUMNS);
        query.push_str(" FROM tasks WHERE 1=1");

        if filter.agent_id.is_some() {
            query.push_str(" AND (owner_agent_id = ? OR assigned_agent_id = ?)");
        }
        if filter.owner_agent_id.is_some() {
            query.push_str(" AND owner_agent_id = ?");
        }
        if filter.assigned_agent_id.is_some() {
            query.push_str(" AND assigned_agent_id = ?");
        }
        if filter.status.is_some() {
            query.push_str(" AND status = ?");
        }
        if filter.priority.is_some() {
            query.push_str(" AND priority = ?");
        }
        if filter.created_by.is_some() {
            query.push_str(" AND created_by = ?");
        }
        query.push_str(" ORDER BY task_number DESC LIMIT ?");

        let mut sql = sqlx::query(&query);
        if let Some(ref agent) = filter.agent_id {
            sql = sql.bind(agent).bind(agent);
        }
        if let Some(ref owner) = filter.owner_agent_id {
            sql = sql.bind(owner);
        }
        if let Some(ref assigned) = filter.assigned_agent_id {
            sql = sql.bind(assigned);
        }
        if let Some(status) = filter.status {
            sql = sql.bind(status.as_str());
        }
        if let Some(priority) = filter.priority {
            sql = sql.bind(priority.as_str());
        }
        if let Some(ref created_by) = filter.created_by {
            sql = sql.bind(created_by);
        }
        sql = sql.bind(filter.limit.unwrap_or(100).clamp(1, 500));

        let rows = sql
            .fetch_all(&self.pool)
            .await
            .context("failed to list tasks")?;

        let mut tasks: Vec<Task> = rows.into_iter().map(task_from_row).collect::<Result<_>>()?;
        self.hydrate_dependencies(&mut tasks).await?;
        Ok(tasks)
    }

    /// Attach dependency edges to the given tasks in one batch query.
    async fn hydrate_dependencies(&self, tasks: &mut [Task]) -> Result<()> {
        if tasks.is_empty() {
            return Ok(());
        }

        let placeholders = vec!["?"; tasks.len()].join(", ");
        let query = format!(
            "SELECT d.task_id, d.kind, dep.task_number, dep.title, dep.status, dep.worktree_id \
             FROM task_dependencies d \
             JOIN tasks dep ON dep.id = d.depends_on_task_id \
             WHERE d.task_id IN ({placeholders}) \
             ORDER BY dep.task_number ASC"
        );
        let mut sql = sqlx::query(&query);
        for task in tasks.iter() {
            sql = sql.bind(&task.id);
        }
        let rows = sql
            .fetch_all(&self.pool)
            .await
            .context("failed to load task dependencies")?;

        let mut edges: std::collections::HashMap<String, Vec<TaskDependencyEdge>> =
            std::collections::HashMap::new();
        for row in rows {
            let task_id: String = row.try_get("task_id").context("dependency task_id")?;
            let kind_value: String = row.try_get("kind").context("dependency kind")?;
            let kind = TaskDependencyKind::parse(&kind_value)
                .with_context(|| format!("invalid dependency kind in database: {kind_value}"))?;
            let status_value: String = row.try_get("status").context("dependency status")?;
            let status = TaskStatus::parse(&status_value)
                .with_context(|| format!("invalid task status in database: {status_value}"))?;
            let worktree_id: Option<String> = row
                .try_get::<Option<String>, _>("worktree_id")
                .ok()
                .flatten();

            let satisfied = match kind {
                TaskDependencyKind::Gate => status == TaskStatus::Done,
                TaskDependencyKind::Stack => status == TaskStatus::Done || worktree_id.is_some(),
            };

            edges.entry(task_id).or_default().push(TaskDependencyEdge {
                depends_on_task_number: row
                    .try_get("task_number")
                    .context("dependency task_number")?,
                depends_on_title: row.try_get("title").context("dependency title")?,
                depends_on_status: status,
                kind,
                satisfied,
            });
        }

        for task in tasks.iter_mut() {
            task.depends_on = edges.remove(&task.id).unwrap_or_default();
        }
        Ok(())
    }

    /// Replace a task's dependency edges.
    ///
    /// Rejects unknown task numbers, self-dependencies, more than one stack
    /// edge, and any edge that would create a cycle.
    pub async fn set_dependencies(
        &self,
        task_number: i64,
        edges: &[(i64, TaskDependencyKind)],
    ) -> Result<Task> {
        let task = self
            .get_by_number(task_number)
            .await?
            .with_context(|| format!("task #{task_number} not found"))?;

        let mut tx = self
            .pool
            .begin()
            .await
            .context("failed to open dependency transaction")?;
        Self::replace_dependencies_in_tx(&mut tx, &task, edges).await?;
        tx.commit()
            .await
            .context("failed to commit dependency transaction")?;

        self.get_by_number(task_number)
            .await?
            .context("task vanished during dependency update")
            .map_err(Into::into)
    }

    /// List ready tasks assigned to the given agent.
    pub async fn list_ready(&self, assigned_agent_id: &str, limit: i64) -> Result<Vec<Task>> {
        self.list(TaskListFilter {
            assigned_agent_id: Some(assigned_agent_id.to_string()),
            status: Some(TaskStatus::Ready),
            limit: Some(limit),
            ..Default::default()
        })
        .await
    }

    /// Fetch a single task by its globally unique number.
    pub async fn get_by_number(&self, task_number: i64) -> Result<Option<Task>> {
        let row = sqlx::query(&format!(
            "{SELECT_COLUMNS} FROM tasks WHERE task_number = ?"
        ))
        .bind(task_number)
        .fetch_optional(&self.pool)
        .await
        .context("failed to fetch task by number")?;

        let Some(task) = row.map(task_from_row).transpose()? else {
            return Ok(None);
        };
        let mut tasks = [task];
        self.hydrate_dependencies(&mut tasks).await?;
        let [task] = tasks;
        Ok(Some(task))
    }

    /// Bind tasks to the worktree already carrying their conventional name.
    ///
    /// A task provisioned before the binding was recorded has a `task-<number>`
    /// worktree on disk and no `worktree_id`, so nothing connects the two and a
    /// retry rediscovers the worktree by name every time. `candidates` is
    /// `(worktree_name, worktree_id)`; the caller supplies them so this does
    /// not reach into the project tables.
    ///
    /// The binding is inferred from the naming convention rather than observed
    /// when the worktree was provisioned, and the revision it writes says so —
    /// that is what keeps an inferred binding distinguishable from a real one.
    /// Idempotent: a task that already has a binding is never revisited.
    pub async fn backfill_worktree_bindings(
        &self,
        candidates: &[(String, String)],
    ) -> Result<usize> {
        if candidates.is_empty() {
            return Ok(0);
        }

        let unbound: Vec<i64> = sqlx::query_scalar(
            "SELECT task_number FROM tasks WHERE worktree_id IS NULL ORDER BY task_number",
        )
        .fetch_all(self.pool())
        .await
        .context("failed to list tasks without a worktree binding")?;

        let mut bound = 0usize;
        for task_number in unbound {
            let name = format!("task-{task_number}");
            let Some((_, worktree_id)) = candidates.iter().find(|(known, _)| *known == name) else {
                continue;
            };

            let context = TaskMutationContext::new(
                crate::tasks::TaskAuthorKind::System,
                Some("migration".to_string()),
                crate::tasks::TaskMutationSource::Migration,
            )
            .with_summary(Some(format!(
                "Bound to the existing {name} worktree by name; inferred from the naming \
                 convention, not observed when the worktree was provisioned"
            )));

            let updated = self
                .update(
                    task_number,
                    UpdateTaskInput {
                        worktree_id: Some(Some(worktree_id.clone())),
                        context,
                        ..Default::default()
                    },
                )
                .await?;

            if updated.is_some() {
                bound += 1;
            }
        }

        Ok(bound)
    }

    pub async fn update(&self, task_number: i64, input: UpdateTaskInput) -> Result<Option<Task>> {
        Ok(self
            .update_with_status_transition(task_number, input)
            .await?
            .map(|result| result.task))
    }

    pub async fn update_with_status_transition(
        &self,
        task_number: i64,
        input: UpdateTaskInput,
    ) -> Result<Option<TaskUpdateResult>> {
        self.update_with_status_policy(task_number, input, StatusTransitionPolicy::Enforce)
            .await
    }

    /// Update a task and optionally replace its dependency edges atomically.
    pub async fn update_with_dependencies_and_status_transition(
        &self,
        task_number: i64,
        input: UpdateTaskInput,
        edges: Option<&[(i64, TaskDependencyKind)]>,
    ) -> Result<Option<TaskUpdateResult>> {
        self.update_with_dependencies_and_status_policy(
            task_number,
            input,
            edges,
            StatusTransitionPolicy::Enforce,
        )
        .await
    }

    /// Update a task from an operator-facing API without enforcing agent
    /// lifecycle transitions. This reconciles work completed outside Spacebot.
    pub async fn update_with_status_override(
        &self,
        task_number: i64,
        input: UpdateTaskInput,
    ) -> Result<Option<TaskUpdateResult>> {
        self.update_with_status_policy(task_number, input, StatusTransitionPolicy::Override)
            .await
    }

    /// Update a task and optionally replace its dependency edges atomically,
    /// without enforcing agent lifecycle status transitions.
    pub async fn update_with_dependencies_and_status_override(
        &self,
        task_number: i64,
        input: UpdateTaskInput,
        edges: Option<&[(i64, TaskDependencyKind)]>,
    ) -> Result<Option<TaskUpdateResult>> {
        self.update_with_dependencies_and_status_policy(
            task_number,
            input,
            edges,
            StatusTransitionPolicy::Override,
        )
        .await
    }

    async fn update_with_status_policy(
        &self,
        task_number: i64,
        input: UpdateTaskInput,
        status_policy: StatusTransitionPolicy,
    ) -> Result<Option<TaskUpdateResult>> {
        self.update_with_dependencies_and_status_policy(task_number, input, None, status_policy)
            .await
    }

    async fn update_with_dependencies_and_status_policy(
        &self,
        task_number: i64,
        input: UpdateTaskInput,
        edges: Option<&[(i64, TaskDependencyKind)]>,
        status_policy: StatusTransitionPolicy,
    ) -> Result<Option<TaskUpdateResult>> {
        self.apply_update(task_number, input, edges, status_policy, None)
            .await
    }

    /// The one transactional task mutation. Every caller — REST, CLI, portal,
    /// tools, automation, restore — reaches storage through here, so the task
    /// row, its revision, and its dependency edges can never disagree.
    ///
    /// `restored_from` marks the revision a restore replayed; ordinary edits
    /// pass `None`.
    async fn apply_update(
        &self,
        task_number: i64,
        input: UpdateTaskInput,
        edges: Option<&[(i64, TaskDependencyKind)]>,
        status_policy: StatusTransitionPolicy,
        restored_from: Option<i64>,
    ) -> Result<Option<TaskUpdateResult>> {
        input.context.validate()?;

        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .context("failed to open task update transaction")?;

        let row = sqlx::query(&format!(
            "{SELECT_COLUMNS} FROM tasks WHERE task_number = ?"
        ))
        .bind(task_number)
        .fetch_optional(&mut *tx)
        .await
        .context("failed to fetch task by number for update")?;

        let Some(row) = row else {
            tx.commit()
                .await
                .context("failed to commit empty task update transaction")?;
            return Ok(None);
        };

        let current = task_from_row(row)?;
        let previous_status = current.status;
        let context = input.context.clone();
        check_expected_revision(task_number, &current, &context)?;

        let before_dependencies = Self::dependency_pairs_in_tx(&mut tx, &current.id).await?;
        let before = TaskRevisionSnapshot::capture(&current, &before_dependencies);
        let task_id = current.id.clone();
        let current_revision = current.revision;

        let mut task =
            Self::update_current_in_tx(&mut tx, task_number, current, input, status_policy).await?;

        if let Some(edges) = edges {
            Self::replace_dependencies_in_tx(&mut tx, &task, edges).await?;
        }

        let after_dependencies = Self::dependency_pairs_in_tx(&mut tx, &task_id).await?;
        let after = TaskRevisionSnapshot::capture(&task, &after_dependencies);

        // An update that changes nothing material leaves no trace in history
        // and emits no edit event.
        let new_revision = if after == before {
            None
        } else {
            let revision = Self::insert_revision_in_tx(
                &mut tx,
                &task_id,
                current_revision,
                &after,
                &context,
                restored_from,
            )
            .await?;
            task.revision = revision;
            Some(revision)
        };

        tx.commit()
            .await
            .context("failed to commit task update transaction")?;

        if edges.is_some() {
            task = self
                .get_by_number(task_number)
                .await?
                .context("task vanished during dependency update")?;
        }

        Ok(Some(TaskUpdateResult {
            previous_status,
            task,
            new_revision,
        }))
    }

    /// Restore a historical revision by appending a new one whose material
    /// state matches it.
    ///
    /// Revision `revision` is never touched and nothing after it is erased. The
    /// restore is an ordinary update underneath, so status transitions,
    /// dependency validation, and optimistic concurrency all still apply.
    pub async fn restore_revision(
        &self,
        task_number: i64,
        revision: i64,
        context: TaskMutationContext,
    ) -> Result<TaskUpdateResult> {
        let target =
            self.get_revision(task_number, revision)
                .await?
                .ok_or(TaskError::RevisionNotFound {
                    task_number,
                    revision,
                })?;
        let snapshot = target.snapshot;
        let edges = snapshot.dependency_edges();

        let input = UpdateTaskInput {
            title: Some(snapshot.title),
            description: Some(snapshot.description),
            status: Some(snapshot.status),
            priority: Some(snapshot.priority),
            subtasks: Some(snapshot.subtasks),
            metadata: Some(snapshot.metadata),
            replace_metadata: true,
            assigned_agent_id: Some(snapshot.assigned_agent_id),
            worker_type: Some(snapshot.worker_type),
            project_id: Some(snapshot.project_id),
            repo_id: Some(snapshot.repo_id),
            worktree_mode: Some(snapshot.worktree_mode),
            worktree_id: Some(snapshot.worktree_id),
            goal_id: Some(snapshot.goal_id),
            required_skills: Some(snapshot.required_skills),
            context,
            ..Default::default()
        };

        self.apply_update(
            task_number,
            input,
            Some(&edges),
            StatusTransitionPolicy::Enforce,
            Some(revision),
        )
        .await?
        .ok_or_else(|| TaskError::NotFound { task_number }.into())
    }

    async fn replace_dependencies_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        task: &Task,
        edges: &[(i64, TaskDependencyKind)],
    ) -> Result<()> {
        let stack_edges = edges
            .iter()
            .filter(|(_, kind)| *kind == TaskDependencyKind::Stack)
            .count();
        if stack_edges > 1 {
            return Err(crate::error::Error::Other(anyhow::anyhow!(
                "a task can stack on at most one parent"
            )));
        }

        let mut resolved: Vec<(String, TaskDependencyKind)> = Vec::with_capacity(edges.len());
        for (dep_number, kind) in edges {
            if *dep_number == task.task_number {
                return Err(crate::error::Error::Other(anyhow::anyhow!(
                    "task #{} cannot depend on itself",
                    task.task_number
                )));
            }
            let dep = sqlx::query(&format!(
                "{SELECT_COLUMNS} FROM tasks WHERE task_number = ?"
            ))
            .bind(dep_number)
            .fetch_optional(&mut **tx)
            .await
            .context("failed to fetch dependency task")?
            .map(task_from_row)
            .transpose()?
            .with_context(|| format!("dependency task #{dep_number} not found"))?;

            // A cycle exists when the dependency can already reach this task
            // through existing edges.
            let reaches: Option<i64> = sqlx::query_scalar(
                "WITH RECURSIVE reachable(id) AS ( \
                     SELECT depends_on_task_id FROM task_dependencies WHERE task_id = ? \
                     UNION \
                     SELECT d.depends_on_task_id FROM task_dependencies d \
                     JOIN reachable r ON d.task_id = r.id \
                  ) SELECT 1 FROM reachable WHERE id = ? LIMIT 1",
            )
            .bind(&dep.id)
            .bind(&task.id)
            .fetch_optional(&mut **tx)
            .await
            .context("failed to run dependency cycle check")?;
            if reaches.is_some() {
                return Err(crate::error::Error::Other(anyhow::anyhow!(
                    "dependency on #{dep_number} would create a cycle"
                )));
            }

            resolved.push((dep.id, *kind));
        }

        sqlx::query("DELETE FROM task_dependencies WHERE task_id = ?")
            .bind(&task.id)
            .execute(&mut **tx)
            .await
            .context("failed to clear task dependencies")?;
        for (dep_id, kind) in resolved {
            sqlx::query(
                "INSERT INTO task_dependencies (task_id, depends_on_task_id, kind) \
                 VALUES (?, ?, ?)",
            )
            .bind(&task.id)
            .bind(dep_id)
            .bind(kind.as_str())
            .execute(&mut **tx)
            .await
            .context("failed to insert task dependency")?;
        }

        Ok(())
    }

    pub async fn update_worker_task(
        &self,
        worker_id: &str,
        task_number: i64,
        input: UpdateTaskInput,
    ) -> Result<WorkerTaskUpdateResult> {
        input.context.validate()?;

        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .context("failed to open worker task update transaction")?;

        let exact_row = sqlx::query(&format!(
            "{SELECT_COLUMNS} FROM tasks WHERE worker_id = ? AND task_number = ?"
        ))
        .bind(worker_id)
        .bind(task_number)
        .fetch_optional(&mut *tx)
        .await
        .context("failed to fetch worker task by id and number for update")?;

        let Some(row) = exact_row else {
            let assigned_task_number = sqlx::query_scalar::<_, i64>(
                "SELECT task_number FROM tasks WHERE worker_id = ? ORDER BY task_number DESC LIMIT 1",
            )
            .bind(worker_id)
            .fetch_optional(&mut *tx)
            .await
            .context("failed to fetch any task by worker id for update")?;

            tx.commit()
                .await
                .context("failed to commit unmatched worker task update transaction")?;
            if let Some(assigned_task_number) = assigned_task_number {
                return Ok(WorkerTaskUpdateResult::WrongTask {
                    assigned_task_number,
                });
            }
            return Ok(WorkerTaskUpdateResult::NotAssigned);
        };

        let current = task_from_row(row)?;
        let previous_status = current.status;
        let context = input.context.clone();
        check_expected_revision(task_number, &current, &context)?;

        let dependencies = Self::dependency_pairs_in_tx(&mut tx, &current.id).await?;
        let before = TaskRevisionSnapshot::capture(&current, &dependencies);
        let task_id = current.id.clone();
        let current_revision = current.revision;

        let mut task = Self::update_current_in_tx(
            &mut tx,
            task_number,
            current,
            input,
            StatusTransitionPolicy::Enforce,
        )
        .await?;

        // Workers cannot touch dependency edges, so the edge set is unchanged.
        let after = TaskRevisionSnapshot::capture(&task, &dependencies);
        let new_revision = if after == before {
            None
        } else {
            let revision = Self::insert_revision_in_tx(
                &mut tx,
                &task_id,
                current_revision,
                &after,
                &context,
                None,
            )
            .await?;
            task.revision = revision;
            Some(revision)
        };

        tx.commit()
            .await
            .context("failed to commit worker task update transaction")?;

        Ok(WorkerTaskUpdateResult::Updated(Box::new(
            TaskUpdateResult {
                previous_status,
                task,
                new_revision,
            },
        )))
    }

    async fn update_current_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        task_number: i64,
        current: Task,
        input: UpdateTaskInput,
        status_policy: StatusTransitionPolicy,
    ) -> Result<Task> {
        if let Some(next_status) = input.status
            && status_policy == StatusTransitionPolicy::Enforce
            && !can_transition(current.status, next_status)
        {
            return Err(TaskError::InvalidTransition {
                from: current.status.to_string(),
                to: next_status.to_string(),
            }
            .into());
        }

        let mut subtasks = input.subtasks.unwrap_or(current.subtasks);
        if let Some(index) = input.complete_subtask
            && let Some(subtask) = subtasks.get_mut(index)
        {
            subtask.completed = true;
        }

        let next_status = input.status.unwrap_or(current.status);
        let next_priority = input.priority.unwrap_or(current.priority);
        let next_metadata = if input.replace_metadata {
            input.metadata.unwrap_or(current.metadata)
        } else {
            merge_json_object(current.metadata, input.metadata)
        };
        let next_assigned = patch(input.assigned_agent_id, current.assigned_agent_id.clone());
        let reassigned = next_assigned != current.assigned_agent_id;

        // If the task is being reassigned to a different agent, clear the worker
        // binding so the old worker cannot keep updating it.
        let clear_worker = input.clear_worker_id || (reassigned && current.worker_id.is_some());
        let next_worker_id = if clear_worker {
            None
        } else if let Some(worker_id) = input.worker_id {
            Some(worker_id)
        } else {
            current.worker_id
        };

        let approved_at = if current.approved_at.is_none() && next_status == TaskStatus::Ready {
            Some("SET")
        } else {
            None
        };

        let completed_at = if current.status != TaskStatus::Done && next_status == TaskStatus::Done
        {
            Some("SET")
        } else if current.status == TaskStatus::Done && next_status != TaskStatus::Done {
            Some("NULL")
        } else {
            None
        };

        let next_worker_type = patch(input.worker_type, current.worker_type);
        let next_project_id = patch(input.project_id, current.project_id);
        let next_repo_id = patch(input.repo_id, current.repo_id);
        let next_worktree_mode = patch(input.worktree_mode, current.worktree_mode);
        let next_worktree_id = patch(input.worktree_id, current.worktree_id);
        let next_goal_id = patch(input.goal_id, current.goal_id);
        let next_required_skills = input.required_skills.unwrap_or(current.required_skills);
        let required_skills_json = serde_json::to_string(&next_required_skills)
            .context("failed to serialize required skills")?;

        let mut query = String::from(
            "UPDATE tasks SET title = ?, description = ?, status = ?, priority = ?, \
             assigned_agent_id = ?, subtasks = ?, metadata = ?, \
             worker_type = ?, project_id = ?, repo_id = ?, worktree_mode = ?, \
             worktree_id = ?, goal_id = ?, required_skills = ?, ",
        );

        if clear_worker {
            query.push_str("worker_id = NULL, ");
        } else {
            query.push_str("worker_id = ?, ");
        }

        query.push_str(
            "approved_by = COALESCE(?, approved_by), \
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        );

        if approved_at.is_some() {
            query.push_str(", approved_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')");
        }
        if let Some(value) = completed_at {
            if value == "SET" {
                query.push_str(", completed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')");
            } else {
                query.push_str(", completed_at = NULL");
            }
        }

        query.push_str(" WHERE task_number = ?");

        let mut sql = sqlx::query(&query)
            .bind(input.title.unwrap_or(current.title))
            .bind(patch(input.description, current.description))
            .bind(next_status.as_str())
            .bind(next_priority.as_str())
            .bind(&next_assigned)
            .bind(serde_json::to_string(&subtasks).context("failed to serialize subtasks")?)
            .bind(next_metadata.to_string())
            .bind(next_worker_type.map(TaskWorkerType::as_str))
            .bind(&next_project_id)
            .bind(&next_repo_id)
            .bind(next_worktree_mode.map(TaskWorktreeMode::as_str))
            .bind(&next_worktree_id)
            .bind(&next_goal_id)
            .bind(&required_skills_json);

        if !clear_worker {
            sql = sql.bind(next_worker_id);
        }

        sql.bind(input.approved_by)
            .bind(task_number)
            .execute(&mut **tx)
            .await
            .context("failed to update task")?;

        // R7-lite: a status change rewrites the active-tasks block inside the
        // cached prefix. Tagged only when the status actually moved.
        if next_status != current.status {
            crate::llm::cache_stats::tag(crate::llm::cache_stats::CacheEvent::TaskTransition);
        }

        let updated = sqlx::query(&format!(
            "{SELECT_COLUMNS} FROM tasks WHERE task_number = ?"
        ))
        .bind(task_number)
        .fetch_one(&mut **tx)
        .await
        .context("failed to fetch updated task")?;

        task_from_row(updated)
    }

    /// Delete a task with its comments, revisions and run history.
    ///
    /// The child rows are removed explicitly rather than through the foreign
    /// key, which only cascades when `PRAGMA foreign_keys` is on.
    pub async fn delete(&self, task_number: i64) -> Result<bool> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .context("failed to open task delete transaction")?;

        let task_id: Option<String> =
            sqlx::query_scalar("SELECT id FROM tasks WHERE task_number = ?")
                .bind(task_number)
                .fetch_optional(&mut *tx)
                .await
                .context("failed to resolve task for deletion")?;

        let Some(task_id) = task_id else {
            tx.rollback()
                .await
                .context("failed to roll back empty task delete transaction")?;
            return Ok(false);
        };

        for table in [
            "task_comments",
            "task_revisions",
            "task_dependencies",
            "task_worker_runs",
        ] {
            sqlx::query(&format!("DELETE FROM {table} WHERE task_id = ?"))
                .bind(&task_id)
                .execute(&mut *tx)
                .await
                .with_context(|| format!("failed to delete {table} for task"))?;
        }
        sqlx::query("DELETE FROM task_dependencies WHERE depends_on_task_id = ?")
            .bind(&task_id)
            .execute(&mut *tx)
            .await
            .context("failed to delete inbound task dependencies")?;

        sqlx::query("DELETE FROM tasks WHERE id = ?")
            .bind(&task_id)
            .execute(&mut *tx)
            .await
            .context("failed to delete task")?;

        tx.commit()
            .await
            .context("failed to commit task delete transaction")?;

        Ok(true)
    }

    pub async fn get_by_worker_id(&self, worker_id: &str) -> Result<Option<Task>> {
        let row = sqlx::query(&format!(
            "{SELECT_COLUMNS} FROM tasks WHERE worker_id = ? ORDER BY updated_at DESC LIMIT 1"
        ))
        .bind(worker_id)
        .fetch_optional(&self.pool)
        .await
        .context("failed to fetch task by worker id")?;

        row.map(task_from_row).transpose()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusTransitionPolicy {
    Enforce,
    Override,
}

/// Fail a mutation whose caller read an older version of the task. The error
/// carries the current revision so the caller can refresh and retry without a
/// second round trip.
fn check_expected_revision(
    task_number: i64,
    current: &Task,
    context: &TaskMutationContext,
) -> Result<()> {
    match context.expected_revision {
        Some(expected) if expected != current.revision => Err(TaskError::RevisionConflict {
            task_number,
            expected,
            current: current.revision,
        }
        .into()),
        _ => Ok(()),
    }
}

/// Column list used by all SELECT queries. Kept in sync with `task_from_row`.
pub(crate) const SELECT_COLUMNS: &str = "SELECT id, task_number, title, description, status, priority, \
     owner_agent_id, assigned_agent_id, subtasks, metadata, goal_id, source_memory_id, worker_id, \
     worker_type, project_id, repo_id, worktree_mode, worktree_id, required_skills, revision, \
     created_by, approved_at, approved_by, created_at, updated_at, completed_at";

pub fn can_transition(current: TaskStatus, next: TaskStatus) -> bool {
    if current == next {
        return true;
    }

    if next == TaskStatus::Backlog {
        return true;
    }

    matches!(
        (current, next),
        (TaskStatus::PendingApproval, TaskStatus::Ready)
            | (TaskStatus::Ready, TaskStatus::InProgress)
            | (TaskStatus::InProgress, TaskStatus::Done)
            | (TaskStatus::InProgress, TaskStatus::Ready)
            | (TaskStatus::InProgress, TaskStatus::Failed)
            | (TaskStatus::Backlog, TaskStatus::Ready)
            | (TaskStatus::Done, TaskStatus::Ready)
            | (TaskStatus::Failed, TaskStatus::Ready)
    )
}

/// Deep-merge an optional object patch into a JSON value. Shared with the
/// goals store so metadata patch semantics stay identical across both.
pub(crate) fn merge_json_object(current: Value, patch: Option<Value>) -> Value {
    let Some(patch) = patch else {
        return current;
    };

    // Only apply object patches — ignore scalars/nulls to preserve the
    // invariant that task metadata is always an object.
    let Value::Object(patch_object) = patch else {
        return current;
    };

    let Value::Object(mut current_object) = current else {
        return Value::Object(patch_object);
    };

    for (key, patch_value) in patch_object {
        let merged_value = match current_object.remove(&key) {
            Some(current_value) => merge_json_value(current_value, patch_value),
            None => patch_value,
        };
        current_object.insert(key, merged_value);
    }

    Value::Object(current_object)
}

fn merge_json_value(current: Value, patch: Value) -> Value {
    match (current, patch) {
        (Value::Object(current_object), Value::Object(patch_object)) => merge_json_object(
            Value::Object(current_object),
            Some(Value::Object(patch_object)),
        ),
        (_, patch_value) => patch_value,
    }
}

fn parse_subtasks(value: &str) -> Vec<TaskSubtask> {
    serde_json::from_str(value).unwrap_or_default()
}

fn parse_metadata(value: &str) -> Value {
    serde_json::from_str(value).unwrap_or_else(|_| Value::Object(serde_json::Map::new()))
}

pub(crate) fn task_from_row(row: sqlx::sqlite::SqliteRow) -> Result<Task> {
    let status_value: String = row
        .try_get("status")
        .context("failed to read task status")?;
    let priority_value: String = row
        .try_get("priority")
        .context("failed to read task priority")?;
    let subtasks_value: String = row.try_get("subtasks").unwrap_or_else(|_| "[]".to_string());
    let metadata_value: String = row.try_get("metadata").unwrap_or_else(|_| "{}".to_string());

    let status = TaskStatus::parse(&status_value)
        .with_context(|| format!("invalid task status in database: {status_value}"))?;
    let priority = TaskPriority::parse(&priority_value)
        .with_context(|| format!("invalid task priority in database: {priority_value}"))?;

    // The global schema uses TEXT columns with ISO 8601 defaults. Read as
    // strings directly; fall back to NaiveDateTime parsing for compatibility
    // with rows that may still use SQLite TIMESTAMP format.
    let created_at = read_timestamp(&row, "created_at")?;
    let updated_at = read_timestamp(&row, "updated_at")?;

    Ok(Task {
        id: row.try_get("id").context("failed to read task id")?,
        task_number: row
            .try_get("task_number")
            .context("failed to read task_number")?,
        title: row.try_get("title").context("failed to read task title")?,
        description: row.try_get("description").ok(),
        status,
        priority,
        owner_agent_id: row
            .try_get("owner_agent_id")
            .context("failed to read owner_agent_id")?,
        assigned_agent_id: row
            .try_get("assigned_agent_id")
            .context("failed to read assigned_agent_id")?,
        subtasks: parse_subtasks(&subtasks_value),
        metadata: parse_metadata(&metadata_value),
        goal_id: row.try_get::<Option<String>, _>("goal_id").ok().flatten(),
        source_memory_id: row.try_get("source_memory_id").ok(),
        worker_id: row
            .try_get::<Option<String>, _>("worker_id")
            .ok()
            .flatten()
            .filter(|value| !value.is_empty()),
        worker_type: row
            .try_get::<Option<String>, _>("worker_type")
            .ok()
            .flatten()
            .as_deref()
            .and_then(TaskWorkerType::parse),
        project_id: row
            .try_get::<Option<String>, _>("project_id")
            .ok()
            .flatten(),
        repo_id: row.try_get::<Option<String>, _>("repo_id").ok().flatten(),
        worktree_mode: row
            .try_get::<Option<String>, _>("worktree_mode")
            .ok()
            .flatten()
            .as_deref()
            .and_then(TaskWorktreeMode::parse),
        worktree_id: row
            .try_get::<Option<String>, _>("worktree_id")
            .ok()
            .flatten(),
        required_skills: row
            .try_get::<Option<String>, _>("required_skills")
            .ok()
            .flatten()
            .and_then(|value| serde_json::from_str(&value).ok())
            .unwrap_or_default(),
        depends_on: Vec::new(),
        revision: row.try_get("revision").unwrap_or(0),
        created_by: row
            .try_get("created_by")
            .context("failed to read task created_by")?,
        approved_at: read_optional_timestamp(&row, "approved_at"),
        approved_by: row.try_get("approved_by").ok(),
        created_at,
        updated_at,
        completed_at: read_optional_timestamp(&row, "completed_at"),
    })
}

/// Read a required timestamp column, trying TEXT first (ISO 8601) then falling
/// back to NaiveDateTime for legacy TIMESTAMP columns.
fn read_timestamp(row: &sqlx::sqlite::SqliteRow, column: &str) -> Result<String> {
    if let Ok(value) = row.try_get::<String, _>(column) {
        return Ok(value);
    }
    row.try_get::<chrono::NaiveDateTime, _>(column)
        .map(|v| v.and_utc().to_rfc3339())
        .with_context(|| format!("failed to read task {column}"))
        .map_err(Into::into)
}

/// Read an optional timestamp column, trying TEXT first then NaiveDateTime.
fn read_optional_timestamp(row: &sqlx::sqlite::SqliteRow, column: &str) -> Option<String> {
    if let Ok(Some(value)) = row.try_get::<Option<String>, _>(column)
        && !value.is_empty()
    {
        return Some(value);
    }
    row.try_get::<Option<chrono::NaiveDateTime>, _>(column)
        .ok()
        .flatten()
        .map(|v| v.and_utc().to_rfc3339())
}

#[cfg(test)]
pub(crate) async fn setup_test_store() -> TaskStore {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite should connect");

    sqlx::query(
        r#"
        CREATE TABLE tasks (
            id TEXT PRIMARY KEY,
            task_number INTEGER NOT NULL UNIQUE,
            title TEXT NOT NULL,
            description TEXT,
            status TEXT NOT NULL DEFAULT 'backlog',
            priority TEXT NOT NULL DEFAULT 'medium',
            owner_agent_id TEXT NOT NULL,
            assigned_agent_id TEXT,
            subtasks TEXT,
            metadata TEXT,
            goal_id TEXT,
            source_memory_id TEXT,
            worker_id TEXT,
            worker_type TEXT,
            project_id TEXT,
            repo_id TEXT,
            worktree_mode TEXT,
            worktree_id TEXT,
            required_skills TEXT NOT NULL DEFAULT '[]',
            revision INTEGER NOT NULL DEFAULT 0,
            created_by TEXT NOT NULL,
            approved_at TEXT,
            approved_by TEXT,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
            updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
            completed_at TEXT
        )
        "#,
    )
    .execute(&pool)
    .await
    .expect("tasks schema should be created");

    sqlx::query(
        "CREATE TABLE task_number_seq (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            next_number INTEGER NOT NULL DEFAULT 1
        )",
    )
    .execute(&pool)
    .await
    .expect("task_number_seq should be created");

    sqlx::query(
        "CREATE TABLE task_dependencies (
            task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
            depends_on_task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
            kind TEXT NOT NULL DEFAULT 'gate',
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
            PRIMARY KEY (task_id, depends_on_task_id)
        )",
    )
    .execute(&pool)
    .await
    .expect("task_dependencies should be created");

    sqlx::query(
        "CREATE TABLE task_comments (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            id TEXT NOT NULL UNIQUE,
            task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
            author_type TEXT NOT NULL,
            author_id TEXT,
            body TEXT NOT NULL,
            worker_id TEXT,
            metadata TEXT NOT NULL DEFAULT '{}',
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
        )",
    )
    .execute(&pool)
    .await
    .expect("task_comments should be created");

    sqlx::query(
        "CREATE TABLE task_revisions (
            id TEXT PRIMARY KEY,
            task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
            revision INTEGER NOT NULL,
            snapshot TEXT NOT NULL,
            author_type TEXT NOT NULL,
            author_id TEXT,
            source TEXT NOT NULL,
            edit_summary TEXT,
            restored_from INTEGER,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
            UNIQUE (task_id, revision)
        )",
    )
    .execute(&pool)
    .await
    .expect("task_revisions should be created");

    sqlx::query(
        "CREATE TABLE task_worker_runs (
            id TEXT PRIMARY KEY,
            task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
            worker_id TEXT NOT NULL,
            attempt INTEGER NOT NULL,
            author_type TEXT NOT NULL DEFAULT 'system',
            author_id TEXT,
            agent_id TEXT,
            channel_id TEXT,
            started_at TIMESTAMP NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
            outcome_kind TEXT,
            outcome_summary TEXT,
            ended_at TIMESTAMP,
            UNIQUE (task_id, worker_id),
            UNIQUE (task_id, attempt)
        )",
    )
    .execute(&pool)
    .await
    .expect("task_worker_runs should be created");

    sqlx::query(
        "CREATE UNIQUE INDEX task_worker_runs_live ON task_worker_runs(task_id) \
         WHERE ended_at IS NULL",
    )
    .execute(&pool)
    .await
    .expect("live attempt index should be created");

    sqlx::query("INSERT INTO task_number_seq (id, next_number) VALUES (1, 1)")
        .execute(&pool)
        .await
        .expect("sequence seed should be inserted");

    TaskStore::new(pool)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup_store() -> TaskStore {
        setup_test_store().await
    }

    fn self_assigned_input(title: &str, status: TaskStatus) -> CreateTaskInput {
        CreateTaskInput {
            owner_agent_id: "agent-test".to_string(),
            assigned_agent_id: Some("agent-test".to_string()),
            title: title.to_string(),
            description: None,
            status,
            priority: TaskPriority::Medium,
            subtasks: Vec::new(),
            metadata: serde_json::json!({}),
            source_memory_id: None,
            created_by: "branch".to_string(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn unassigned_task_persists_and_falls_back_to_owner() {
        let store = setup_store().await;
        let created = store
            .create(CreateTaskInput {
                assigned_agent_id: None,
                ..self_assigned_input("unassigned task", TaskStatus::Backlog)
            })
            .await
            .expect("task should be created");

        let loaded = store
            .get_by_number(created.task_number)
            .await
            .expect("task should load")
            .expect("task should exist");
        assert_eq!(loaded.assigned_agent_id, None);
        assert_eq!(loaded.effective_agent_id(), "agent-test");
    }

    #[tokio::test]
    async fn dependencies_reject_self_references_and_cycles() {
        let store = setup_store().await;
        let first = store
            .create(self_assigned_input("first", TaskStatus::Ready))
            .await
            .expect("first task");
        let second = store
            .create(self_assigned_input("second", TaskStatus::Ready))
            .await
            .expect("second task");

        store
            .set_dependencies(
                second.task_number,
                &[(first.task_number, TaskDependencyKind::Gate)],
            )
            .await
            .expect("dependency should be accepted");

        assert!(
            store
                .set_dependencies(
                    first.task_number,
                    &[(first.task_number, TaskDependencyKind::Gate)],
                )
                .await
                .is_err(),
            "self-dependency must be rejected"
        );
        assert!(
            store
                .set_dependencies(
                    first.task_number,
                    &[(second.task_number, TaskDependencyKind::Gate)],
                )
                .await
                .is_err(),
            "cyclic dependency must be rejected"
        );
    }

    #[tokio::test]
    async fn failed_create_with_dependencies_leaves_no_task() {
        let store = setup_store().await;

        let error = store
            .create_with_dependencies(
                self_assigned_input("invalid dependency", TaskStatus::Ready),
                &[(999, TaskDependencyKind::Gate)],
            )
            .await
            .expect_err("unknown dependency should reject task creation");

        assert!(error.to_string().contains("dependency task #999 not found"));
        assert!(
            store
                .list(TaskListFilter::default())
                .await
                .expect("tasks should list")
                .is_empty(),
            "failed creation must not persist a task row"
        );
    }

    #[tokio::test]
    async fn failed_combined_update_preserves_task_and_dependencies() {
        let store = setup_store().await;
        let dependency = store
            .create(self_assigned_input("dependency", TaskStatus::Ready))
            .await
            .expect("dependency should be created");
        let task = store
            .create_with_dependencies(
                self_assigned_input("original title", TaskStatus::PendingApproval),
                &[(dependency.task_number, TaskDependencyKind::Gate)],
            )
            .await
            .expect("task should be created");

        assert!(
            store
                .update_with_dependencies_and_status_transition(
                    task.task_number,
                    UpdateTaskInput {
                        status: Some(TaskStatus::InProgress),
                        ..Default::default()
                    },
                    Some(&[]),
                )
                .await
                .is_err(),
            "invalid task update should fail"
        );

        let unchanged = store
            .get_by_number(task.task_number)
            .await
            .expect("task should load")
            .expect("task should exist");
        assert_eq!(unchanged.status, TaskStatus::PendingApproval);
        assert_eq!(unchanged.depends_on.len(), 1);
        assert_eq!(
            unchanged.depends_on[0].depends_on_task_number,
            dependency.task_number
        );

        assert!(
            store
                .update_with_dependencies_and_status_transition(
                    task.task_number,
                    UpdateTaskInput {
                        title: Some("updated title".to_string()),
                        ..Default::default()
                    },
                    Some(&[(999, TaskDependencyKind::Gate)]),
                )
                .await
                .is_err(),
            "invalid dependencies should fail"
        );

        let unchanged = store
            .get_by_number(task.task_number)
            .await
            .expect("task should load")
            .expect("task should exist");
        assert_eq!(unchanged.title, "original title");
        assert_eq!(unchanged.depends_on.len(), 1);
        assert_eq!(
            unchanged.depends_on[0].depends_on_task_number,
            dependency.task_number
        );
    }

    #[tokio::test]
    async fn at_most_one_stack_parent() {
        let store = setup_store().await;
        let a = store
            .create(self_assigned_input("a", TaskStatus::Backlog))
            .await
            .expect("a");
        let b = store
            .create(self_assigned_input("b", TaskStatus::Backlog))
            .await
            .expect("b");
        let c = store
            .create(self_assigned_input("c", TaskStatus::Backlog))
            .await
            .expect("c");

        assert!(
            store
                .set_dependencies(
                    c.task_number,
                    &[
                        (a.task_number, TaskDependencyKind::Stack),
                        (b.task_number, TaskDependencyKind::Stack),
                    ],
                )
                .await
                .is_err(),
            "two stack parents must be rejected"
        );

        // One stack parent plus a gate is fine.
        let c = store
            .set_dependencies(
                c.task_number,
                &[
                    (a.task_number, TaskDependencyKind::Stack),
                    (b.task_number, TaskDependencyKind::Gate),
                ],
            )
            .await
            .expect("mixed edges should insert");
        assert_eq!(c.depends_on.len(), 2);
    }

    #[tokio::test]
    async fn execution_plan_fields_round_trip() {
        let store = setup_store().await;
        let created = store
            .create(CreateTaskInput {
                worker_type: Some(TaskWorkerType::Opencode),
                project_id: Some("proj-1".to_string()),
                repo_id: Some("repo-1".to_string()),
                worktree_mode: Some(TaskWorktreeMode::Create),
                required_skills: vec!["spacebot-dev".to_string()],
                ..self_assigned_input("planned task", TaskStatus::Backlog)
            })
            .await
            .expect("task should be created");

        let loaded = store
            .get_by_number(created.task_number)
            .await
            .expect("task should load")
            .expect("task should exist");
        assert_eq!(loaded.worker_type, Some(TaskWorkerType::Opencode));
        assert_eq!(loaded.project_id.as_deref(), Some("proj-1"));
        assert_eq!(loaded.repo_id.as_deref(), Some("repo-1"));
        assert_eq!(loaded.worktree_mode, Some(TaskWorktreeMode::Create));
        assert_eq!(loaded.worktree_id, None);
        assert_eq!(loaded.required_skills, vec!["spacebot-dev".to_string()]);

        // Updates set individual plan fields without disturbing the rest.
        let updated = store
            .update(
                created.task_number,
                UpdateTaskInput {
                    worktree_id: Some(Some("wt-1".to_string())),
                    ..Default::default()
                },
            )
            .await
            .expect("update should succeed")
            .expect("task should exist");
        assert_eq!(updated.worktree_id.as_deref(), Some("wt-1"));
        assert_eq!(updated.worker_type, Some(TaskWorkerType::Opencode));
        assert_eq!(updated.required_skills, vec!["spacebot-dev".to_string()]);
    }

    #[test]
    fn execution_plan_resolution_merges_project_defaults() {
        let task = Task {
            id: "t".to_string(),
            task_number: 1,
            title: "t".to_string(),
            description: None,
            status: TaskStatus::Ready,
            priority: TaskPriority::Medium,
            owner_agent_id: "a".to_string(),
            assigned_agent_id: None,
            subtasks: Vec::new(),
            metadata: serde_json::json!({}),
            goal_id: None,
            source_memory_id: None,
            worker_id: None,
            worker_type: None,
            project_id: Some("proj-1".to_string()),
            repo_id: None,
            worktree_mode: Some(TaskWorktreeMode::Root),
            worktree_id: None,
            required_skills: vec!["Task-Skill".to_string(), "shared".to_string()],
            depends_on: Vec::new(),
            revision: 1,
            created_by: "branch".to_string(),
            approved_at: None,
            approved_by: None,
            created_at: String::new(),
            updated_at: String::new(),
            completed_at: None,
        };
        let defaults = ExecutionDefaults {
            worker_type: Some(TaskWorkerType::Opencode),
            worktree_mode: Some(TaskWorktreeMode::Create),
            required_skills: vec!["shared".to_string(), "project-skill".to_string()],
        };

        let plan = ExecutionPlan::resolve(&task, Some(&defaults));

        // Task fields win; unset fields inherit the defaults.
        assert_eq!(plan.worker_type, Some(TaskWorkerType::Opencode));
        assert_eq!(plan.worktree_mode, Some(TaskWorktreeMode::Root));
        // Union keeps project defaults first and dedups case-insensitively.
        assert_eq!(
            plan.required_skills,
            vec![
                "shared".to_string(),
                "project-skill".to_string(),
                "Task-Skill".to_string(),
            ]
        );

        let bare = ExecutionPlan::resolve(&task, None);
        assert_eq!(bare.worker_type, None);
        assert_eq!(bare.worktree_mode, Some(TaskWorktreeMode::Root));
    }

    #[tokio::test]
    async fn rejects_invalid_status_transition() {
        let store = setup_store().await;
        let created = store
            .create(CreateTaskInput {
                created_by: "cortex".to_string(),
                ..self_assigned_input("pending task", TaskStatus::PendingApproval)
            })
            .await
            .expect("task should be created");

        let error = store
            .update(
                created.task_number,
                UpdateTaskInput {
                    status: Some(TaskStatus::InProgress),
                    ..Default::default()
                },
            )
            .await
            .expect_err("pending_approval -> in_progress must fail");

        assert!(error.to_string().contains("invalid task status transition"));
    }

    #[tokio::test]
    async fn update_with_status_transition_returns_previous_status() {
        let store = setup_store().await;
        let created = store
            .create(self_assigned_input(
                "track status transition",
                TaskStatus::InProgress,
            ))
            .await
            .expect("task should be created");

        let result = store
            .update_with_status_transition(
                created.task_number,
                UpdateTaskInput {
                    status: Some(TaskStatus::Done),
                    ..Default::default()
                },
            )
            .await
            .expect("update should succeed")
            .expect("task should exist");

        assert_eq!(result.previous_status, TaskStatus::InProgress);
        assert_eq!(result.task.status, TaskStatus::Done);
    }

    #[tokio::test]
    async fn operator_override_accepts_status_outside_agent_lifecycle() {
        let store = setup_store().await;
        let created = store
            .create(CreateTaskInput {
                created_by: "human".to_string(),
                ..self_assigned_input("completed elsewhere", TaskStatus::PendingApproval)
            })
            .await
            .expect("task should be created");

        let result = store
            .update_with_status_override(
                created.task_number,
                UpdateTaskInput {
                    status: Some(TaskStatus::Done),
                    ..Default::default()
                },
            )
            .await
            .expect("operator override should succeed")
            .expect("task should exist");

        assert_eq!(result.previous_status, TaskStatus::PendingApproval);
        assert_eq!(result.task.status, TaskStatus::Done);
        assert!(result.task.completed_at.is_some());
        assert!(result.task.approved_at.is_none());
    }

    #[tokio::test]
    async fn repeated_operator_done_override_preserves_completion_time() {
        let store = setup_store().await;
        let created = store
            .create(self_assigned_input("already done", TaskStatus::Backlog))
            .await
            .expect("task should be created");
        let first = store
            .update_with_status_override(
                created.task_number,
                UpdateTaskInput {
                    status: Some(TaskStatus::Done),
                    ..Default::default()
                },
            )
            .await
            .expect("first override should succeed")
            .expect("task should exist");
        let second = store
            .update_with_status_override(
                created.task_number,
                UpdateTaskInput {
                    status: Some(TaskStatus::Done),
                    ..Default::default()
                },
            )
            .await
            .expect("repeated override should succeed")
            .expect("task should exist");

        assert_eq!(first.task.completed_at, second.task.completed_at);
    }

    #[tokio::test]
    async fn update_worker_task_prefers_exact_match_with_duplicate_worker_bindings() {
        let store = setup_store().await;
        let first = store
            .create(self_assigned_input("first task", TaskStatus::InProgress))
            .await
            .expect("first task should be created");
        let second = store
            .create(self_assigned_input("second task", TaskStatus::InProgress))
            .await
            .expect("second task should be created");

        let shared_worker_id = "worker-shared";
        store
            .update(
                first.task_number,
                UpdateTaskInput {
                    worker_id: Some(shared_worker_id.to_string()),
                    ..Default::default()
                },
            )
            .await
            .expect("first worker binding should update");
        store
            .update(
                second.task_number,
                UpdateTaskInput {
                    worker_id: Some(shared_worker_id.to_string()),
                    ..Default::default()
                },
            )
            .await
            .expect("second worker binding should update");

        let result = store
            .update_worker_task(
                shared_worker_id,
                first.task_number,
                UpdateTaskInput {
                    metadata: Some(serde_json::json!({"target": "first"})),
                    ..Default::default()
                },
            )
            .await
            .expect("worker-scoped update should succeed");

        let WorkerTaskUpdateResult::Updated(result) = result else {
            panic!("expected exact task update despite duplicate worker bindings");
        };
        assert_eq!(result.task.task_number, first.task_number);
        assert_eq!(result.task.metadata["target"], "first");
    }

    #[tokio::test]
    async fn can_requeue_in_progress_and_clear_worker_binding() {
        let store = setup_store().await;
        let created = store
            .create(self_assigned_input("ready task", TaskStatus::Ready))
            .await
            .expect("task should be created");

        let in_progress = store
            .update(
                created.task_number,
                UpdateTaskInput {
                    status: Some(TaskStatus::InProgress),
                    worker_id: Some("worker-1".to_string()),
                    ..Default::default()
                },
            )
            .await
            .expect("update should succeed")
            .expect("task should exist");

        assert_eq!(in_progress.worker_id.as_deref(), Some("worker-1"));

        let requeued = store
            .update(
                created.task_number,
                UpdateTaskInput {
                    status: Some(TaskStatus::Ready),
                    clear_worker_id: true,
                    ..Default::default()
                },
            )
            .await
            .expect("requeue should succeed")
            .expect("task should exist");

        assert_eq!(requeued.status, TaskStatus::Ready);
        assert!(
            requeued.worker_id.is_none(),
            "expected worker binding to clear, got {:?}",
            requeued.worker_id
        );
    }

    #[tokio::test]
    async fn metadata_updates_deep_merge_nested_objects() {
        let store = setup_store().await;
        let created = store
            .create(CreateTaskInput {
                metadata: serde_json::json!({
                    "github_issue": {
                        "repo": "spacedriveapp/spacebot",
                        "number": 123,
                        "labels": ["bug"],
                        "state": "open"
                    },
                    "source": "github"
                }),
                ..self_assigned_input("github-linked task", TaskStatus::Backlog)
            })
            .await
            .expect("task should be created");

        let updated = store
            .update(
                created.task_number,
                UpdateTaskInput {
                    metadata: Some(serde_json::json!({
                        "github_issue": {
                            "url": "https://github.com/spacedriveapp/spacebot/issues/123",
                            "labels": ["bug", "tasks"]
                        },
                        "github_pr": {
                            "number": 456
                        }
                    })),
                    ..Default::default()
                },
            )
            .await
            .expect("update should succeed")
            .expect("task should exist");

        assert_eq!(
            updated.metadata,
            serde_json::json!({
                "github_issue": {
                    "repo": "spacedriveapp/spacebot",
                    "number": 123,
                    "url": "https://github.com/spacedriveapp/spacebot/issues/123",
                    "labels": ["bug", "tasks"],
                    "state": "open"
                },
                "github_pr": {
                    "number": 456
                },
                "source": "github"
            })
        );
    }

    #[tokio::test]
    async fn global_task_numbers_are_unique_across_agents() {
        let store = setup_store().await;

        let task_a = store
            .create(self_assigned_input("task for agent A", TaskStatus::Backlog))
            .await
            .expect("task A should be created");

        let task_b = store
            .create(CreateTaskInput {
                owner_agent_id: "agent-other".to_string(),
                assigned_agent_id: Some("agent-other".to_string()),
                ..self_assigned_input("task for agent B", TaskStatus::Backlog)
            })
            .await
            .expect("task B should be created");

        assert_eq!(task_a.task_number, 1);
        assert_eq!(task_b.task_number, 2);

        // Both accessible by global number without agent scoping
        let fetched_a = store
            .get_by_number(1)
            .await
            .expect("fetch should succeed")
            .expect("task 1 should exist");
        assert_eq!(fetched_a.owner_agent_id, "agent-test");

        let fetched_b = store
            .get_by_number(2)
            .await
            .expect("fetch should succeed")
            .expect("task 2 should exist");
        assert_eq!(fetched_b.owner_agent_id, "agent-other");
    }

    #[tokio::test]
    async fn list_filters_by_assigned_agent() {
        let store = setup_store().await;

        store
            .create(self_assigned_input("my task", TaskStatus::Backlog))
            .await
            .expect("should create");

        store
            .create(CreateTaskInput {
                owner_agent_id: "agent-test".to_string(),
                assigned_agent_id: Some("agent-other".to_string()),
                ..self_assigned_input("delegated task", TaskStatus::Ready)
            })
            .await
            .expect("should create");

        let mine = store
            .list(TaskListFilter {
                assigned_agent_id: Some("agent-test".to_string()),
                ..Default::default()
            })
            .await
            .expect("list should succeed");
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].title, "my task");

        let theirs = store
            .list(TaskListFilter {
                assigned_agent_id: Some("agent-other".to_string()),
                ..Default::default()
            })
            .await
            .expect("list should succeed");
        assert_eq!(theirs.len(), 1);
        assert_eq!(theirs[0].title, "delegated task");

        // Unfiltered returns both
        let all = store
            .list(TaskListFilter::default())
            .await
            .expect("list should succeed");
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn reassign_task_via_update() {
        let store = setup_store().await;
        let created = store
            .create(self_assigned_input("reassignable", TaskStatus::Backlog))
            .await
            .expect("should create");

        assert_eq!(created.assigned_agent_id.as_deref(), Some("agent-test"));

        let updated = store
            .update(
                created.task_number,
                UpdateTaskInput {
                    assigned_agent_id: Some(Some("agent-other".to_string())),
                    ..Default::default()
                },
            )
            .await
            .expect("update should succeed")
            .expect("task should exist");

        assert_eq!(updated.assigned_agent_id.as_deref(), Some("agent-other"));
        assert_eq!(updated.owner_agent_id, "agent-test");
    }
}
