use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Subcommand, ValueEnum};

use crate::agent_bus;
use crate::session;
use crate::task_delivery;
use crate::task_store::{
    CreateTask, Task, TaskFilter, TaskStatus, TaskUpdate, claim_task, create_task, done_task,
    get_task, list_tasks, update_task,
};
use crate::workspace::{
    BoardMeta, TasksContext, active_board_id, create_board, list_boards, resolve_tasks_context,
    set_active_board, workspace_for_cwd,
};

#[derive(Subcommand)]
pub enum TaskCommand {
    /// Manage task boards within the current workspace.
    Board {
        #[command(subcommand)]
        command: TaskBoardCommand,
    },
    /// List tasks on the active board.
    List {
        #[arg(long)]
        json: bool,
        /// Filter by status.
        #[arg(long)]
        status: Option<TaskStatusArg>,
        /// Filter by assignee id.
        #[arg(long)]
        assignee: Option<String>,
    },
    /// Create a new task (status `queued`).
    Create {
        /// Task title.
        title: String,
        /// Optional stable id (auto-generated when omitted).
        #[arg(long)]
        id: Option<String>,
        /// Initial assignee.
        #[arg(long)]
        assignee: Option<String>,
        /// Task ids this work is blocked by (comma-separated).
        #[arg(long, value_delimiter = ',')]
        blocked_by: Vec<String>,
        /// Longer description.
        #[arg(long, short = 'm')]
        body: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show one task by id.
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Assign a task to yourself (or `--assignee`) and set status to `in_progress`.
    Claim {
        id: String,
        #[arg(long)]
        assignee: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Update task fields.
    Update {
        id: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        status: Option<TaskStatusArg>,
        /// Set assignee (use `--clear-assignee` to remove).
        #[arg(long)]
        assignee: Option<String>,
        #[arg(long)]
        clear_assignee: bool,
        /// Replace blocked-by list (comma-separated).
        #[arg(long, value_delimiter = ',')]
        blocked_by: Option<Vec<String>>,
        #[arg(long, short = 'm')]
        body: Option<String>,
        #[arg(long)]
        clear_body: bool,
        #[arg(long)]
        json: bool,
    },
    /// Mark a task as `done`.
    Done {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Hand a task to the human for review. Sets status to `review` and assignee
    /// to `human`, then fires the review gate (opens the review surface +
    /// notifies the primary agent pane). Shorthand for
    /// `update <id> --status review --assignee human`.
    Review {
        id: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum TaskBoardCommand {
    /// List task boards for the current workspace.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Create a board and switch to it.
    New {
        /// Board id (auto-generated when omitted).
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Switch the active task board for this workspace.
    Use { id: String },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
#[value(rename_all = "lowercase")]
pub(crate) enum TaskStatusArg {
    Queued,
    #[value(name = "in_progress")]
    InProgress,
    Review,
    Done,
    Blocked,
}

impl From<TaskStatusArg> for TaskStatus {
    fn from(value: TaskStatusArg) -> Self {
        match value {
            TaskStatusArg::Queued => TaskStatus::Queued,
            TaskStatusArg::InProgress => TaskStatus::InProgress,
            TaskStatusArg::Review => TaskStatus::Review,
            TaskStatusArg::Done => TaskStatus::Done,
            TaskStatusArg::Blocked => TaskStatus::Blocked,
        }
    }
}

pub fn run(command: TaskCommand) -> Result<()> {
    match command {
        TaskCommand::Board { command } => run_board(command),
        TaskCommand::List {
            json,
            status,
            assignee,
        } => run_list(json, status, assignee),
        TaskCommand::Create {
            title,
            id,
            assignee,
            blocked_by,
            body,
            json,
        } => run_create(title, id, assignee, blocked_by, body, json),
        TaskCommand::Show { id, json } => run_show(id, json),
        TaskCommand::Claim { id, assignee, json } => run_claim(id, assignee, json),
        TaskCommand::Update {
            id,
            title,
            status,
            assignee,
            clear_assignee,
            blocked_by,
            body,
            clear_body,
            json,
        } => run_update(UpdateArgs {
            id,
            title,
            status,
            assignee,
            clear_assignee,
            blocked_by,
            body,
            clear_body,
            json,
        }),
        TaskCommand::Done { id, json } => run_done(id, json),
        TaskCommand::Review { id, json } => run_review(id, json),
    }
}

fn self_id() -> Option<String> {
    std::env::var(agent_bus::VIA_AGENT_ID_ENV)
        .ok()
        .filter(|s| !s.is_empty())
}

fn workspace_cwd() -> Result<PathBuf> {
    if let Ok(manifest) = session::resolve_session() {
        return Ok(manifest.cwd);
    }
    std::env::current_dir().context("resolve current directory")
}

fn tasks_context() -> Result<TasksContext> {
    resolve_tasks_context(&workspace_cwd()?)
}

fn run_board(command: TaskBoardCommand) -> Result<()> {
    match command {
        TaskBoardCommand::List { json } => run_board_list(json),
        TaskBoardCommand::New { id, title, json } => run_board_new(id, title, json),
        TaskBoardCommand::Use { id } => run_board_use(id),
    }
}

fn run_board_list(json: bool) -> Result<()> {
    let cwd = workspace_cwd()?;
    let workspace = workspace_for_cwd(&cwd)?;
    let active = active_board_id(&workspace)?;
    let boards = list_boards(&workspace)?;

    if json {
        let payload = serde_json::json!({
            "workspace_id": workspace.id,
            "cwd": workspace.cwd,
            "active_board": active,
            "boards": boards,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    println!("workspace={} cwd={}", workspace.id, workspace.cwd.display());
    if boards.is_empty() {
        println!("no boards");
        return Ok(());
    }

    for board in boards {
        print_board_line(&board, active.as_deref());
    }
    Ok(())
}

fn run_board_new(id: Option<String>, title: Option<String>, json: bool) -> Result<()> {
    let cwd = workspace_cwd()?;
    let workspace = workspace_for_cwd(&cwd)?;
    let id =
        id.unwrap_or_else(|| format!("board-{}-{}", crate::util::now_millis(), std::process::id()));
    let meta = create_board(&workspace, &id, title)?;
    set_active_board(&workspace, &meta.id)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&meta)?);
    } else {
        print_board_line(&meta, Some(&meta.id));
        println!("(created and activated)");
    }
    Ok(())
}

fn run_board_use(id: String) -> Result<()> {
    let cwd = workspace_cwd()?;
    let workspace = workspace_for_cwd(&cwd)?;
    set_active_board(&workspace, &id)?;
    println!("active board: {id}");
    Ok(())
}

fn run_list(json: bool, status: Option<TaskStatusArg>, assignee: Option<String>) -> Result<()> {
    let ctx = tasks_context()?;
    let filter = TaskFilter {
        status: status.map(Into::into),
        assignee,
    };
    let tasks = list_tasks(&ctx.tasks_dir, &filter)?;

    if json {
        let payload = serde_json::json!({
            "workspace_id": ctx.workspace_id,
            "board": ctx.board_id,
            "tasks": tasks,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    println!("workspace={} board={}", ctx.workspace_id, ctx.board_id);
    if tasks.is_empty() {
        println!("no tasks");
        return Ok(());
    }

    for task in tasks {
        print_task_line(&task);
    }
    Ok(())
}

fn run_create(
    title: String,
    id: Option<String>,
    assignee: Option<String>,
    blocked_by: Vec<String>,
    body: Option<String>,
    json: bool,
) -> Result<()> {
    let ctx = tasks_context()?;
    let task = create_task(
        &ctx.tasks_dir,
        CreateTask {
            title,
            id,
            assignee,
            blocked_by,
            created_by: self_id(),
            body,
        },
    )?;
    task_delivery::deliver_task_notifications(&task, None, self_id().as_deref());
    print_task_result(&task, json, "created")
}

fn run_show(id: String, json: bool) -> Result<()> {
    let ctx = tasks_context()?;
    let Some(task) = get_task(&ctx.tasks_dir, &id)? else {
        bail!("task not found: {id}");
    };
    print_task_result(&task, json, "show")
}

fn run_claim(id: String, assignee: Option<String>, json: bool) -> Result<()> {
    let assignee = match assignee.or_else(self_id) {
        Some(id) => id,
        None => bail!(
            "assignee required: pass --assignee or run from a via agent pane with {} set",
            agent_bus::VIA_AGENT_ID_ENV
        ),
    };
    let ctx = tasks_context()?;
    let previous = get_task(&ctx.tasks_dir, &id)?;
    let task = claim_task(&ctx.tasks_dir, &id, &assignee)?;
    task_delivery::deliver_task_notifications(&task, previous.as_ref(), self_id().as_deref());
    print_task_result(&task, json, "claimed")
}

struct UpdateArgs {
    id: String,
    title: Option<String>,
    status: Option<TaskStatusArg>,
    assignee: Option<String>,
    clear_assignee: bool,
    blocked_by: Option<Vec<String>>,
    body: Option<String>,
    clear_body: bool,
    json: bool,
}

fn run_update(args: UpdateArgs) -> Result<()> {
    if args.clear_assignee && args.assignee.is_some() {
        bail!("pass only one of --assignee and --clear-assignee");
    }
    if args.clear_body && args.body.is_some() {
        bail!("pass only one of --body and --clear-body");
    }

    let assignee_update = if args.clear_assignee {
        Some(None)
    } else {
        args.assignee.map(Some)
    };
    let body_update = if args.clear_body {
        Some(None)
    } else {
        args.body.map(Some)
    };

    let ctx = tasks_context()?;
    let previous = get_task(&ctx.tasks_dir, &args.id)?;
    let task = update_task(
        &ctx.tasks_dir,
        &args.id,
        TaskUpdate {
            title: args.title,
            status: args.status.map(Into::into),
            assignee: assignee_update,
            blocked_by: args.blocked_by,
            body: body_update,
        },
    )?;
    task_delivery::deliver_task_notifications(&task, previous.as_ref(), self_id().as_deref());
    print_task_result(&task, args.json, "updated")
}

fn run_done(id: String, json: bool) -> Result<()> {
    let ctx = tasks_context()?;
    let previous = get_task(&ctx.tasks_dir, &id)?;
    let task = done_task(&ctx.tasks_dir, &id)?;
    task_delivery::deliver_task_notifications(&task, previous.as_ref(), self_id().as_deref());
    print_task_result(&task, json, "done")
}

/// `via task review <id>`: set status to `review` + assignee to `human`, then
/// fire the review gate. This is the explicit "hand this to the human" move —
/// distinct from `update <id> --status review` which marks ready for review
/// without reassigning. The review-gate fire (open review surface, notify
/// primary agent pane) happens inside `deliver_task_notifications` when it
/// detects the status transition to `review`.
fn run_review(id: String, json: bool) -> Result<()> {
    let ctx = tasks_context()?;
    let previous = get_task(&ctx.tasks_dir, &id)?;
    let task = update_task(
        &ctx.tasks_dir,
        &id,
        TaskUpdate {
            status: Some(TaskStatus::Review),
            assignee: Some(Some(crate::config::HUMAN_ASSIGNEE_ID.to_string())),
            ..TaskUpdate::default()
        },
    )?;
    task_delivery::deliver_task_notifications(&task, previous.as_ref(), self_id().as_deref());
    print_task_result(&task, json, "review")
}

fn print_task_result(task: &Task, json: bool, verb: &str) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(task)?);
    } else {
        print_task_line(task);
        println!("({verb})");
    }
    Ok(())
}

fn print_task_line(task: &Task) {
    let assignee = task.assignee.as_deref().unwrap_or("-");
    let status = task_status_label(task.status);
    println!(
        "{}\tstatus={}\tassignee={}\ttitle={}",
        task.id, status, assignee, task.title
    );
}

fn print_board_line(board: &BoardMeta, active: Option<&str>) {
    let marker = if active == Some(board.id.as_str()) {
        "*"
    } else {
        " "
    };
    let title = board.title.as_deref().unwrap_or("-");
    println!("{marker} {}\ttitle={}", board.id, title);
}

fn task_status_label(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Queued => "queued",
        TaskStatus::InProgress => "in_progress",
        TaskStatus::Review => "review",
        TaskStatus::Done => "done",
        TaskStatus::Blocked => "blocked",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    use crate::cli::{Cli, Command};
    use crate::test_support::{temp_dir, write_session_manifest};
    use crate::workspace::resolve_tasks_context;

    #[test]
    fn parses_task_list_json() {
        let cli = Cli::try_parse_from(["via", "task", "list", "--json"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Task {
                command: TaskCommand::List {
                    json: true,
                    status: None,
                    assignee: None,
                },
            })
        ));
    }

    #[test]
    fn parses_task_board_commands() {
        let cli = Cli::try_parse_from(["via", "task", "board", "list", "--json"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Task {
                command: TaskCommand::Board {
                    command: TaskBoardCommand::List { json: true },
                },
            })
        ));

        let cli = Cli::try_parse_from([
            "via", "task", "board", "new", "--id", "phase2", "--title", "Phase 2",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Task {
                command: TaskCommand::Board {
                    command: TaskBoardCommand::New {
                        id: Some(id),
                        title: Some(title),
                        json: false,
                    },
                },
            }) if id == "phase2" && title == "Phase 2"
        ));

        let cli = Cli::try_parse_from(["via", "task", "board", "use", "phase2"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Task {
                command: TaskCommand::Board {
                    command: TaskBoardCommand::Use { id },
                },
            }) if id == "phase2"
        ));
    }

    #[test]
    fn parses_task_create() {
        let cli = Cli::try_parse_from([
            "via",
            "task",
            "create",
            "Implement CLI",
            "--id",
            "phase2-cli",
            "--assignee",
            "agent",
            "-m",
            "via task subcommands",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Task {
                command: TaskCommand::Create {
                    title,
                    id: Some(task_id),
                    assignee: Some(assignee),
                    blocked_by,
                    body: Some(body),
                    json: false,
                },
            }) if title == "Implement CLI"
                && task_id == "phase2-cli"
                && assignee == "agent"
                && blocked_by.is_empty()
                && body == "via task subcommands"
        ));
    }

    #[test]
    fn parses_task_claim_and_update() {
        let cli =
            Cli::try_parse_from(["via", "task", "claim", "t1", "--assignee", "coder"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Task {
                command: TaskCommand::Claim {
                    id,
                    assignee: Some(assignee),
                    json: false,
                },
            }) if id == "t1" && assignee == "coder"
        ));

        let cli = Cli::try_parse_from([
            "via",
            "task",
            "update",
            "t1",
            "--status",
            "review",
            "--clear-assignee",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Task {
                command: TaskCommand::Update {
                    id,
                    status: Some(TaskStatusArg::Review),
                    clear_assignee: true,
                    ..
                },
            }) if id == "t1"
        ));
    }

    #[test]
    fn parses_task_show_and_done() {
        let cli = Cli::try_parse_from(["via", "task", "show", "phase2-store", "--json"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Task {
                command: TaskCommand::Show { id, json: true },
            }) if id == "phase2-store"
        ));

        let cli = Cli::try_parse_from(["via", "task", "done", "phase2-store"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Task {
                command: TaskCommand::Done { id, json: false },
            }) if id == "phase2-store"
        ));
    }

    #[test]
    fn parses_task_review() {
        let cli =
            Cli::try_parse_from(["via", "task", "review", "p4-task-review", "--json"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Task {
                command: TaskCommand::Review { id, json: true },
            }) if id == "p4-task-review"
        ));
    }

    #[test]
    fn task_status_arg_maps_to_store() {
        assert_eq!(TaskStatus::InProgress, TaskStatusArg::InProgress.into());
    }

    /// `via task review <id>` sets status to `review` and assignee to `human`.
    #[test]
    fn run_review_sets_status_and_assignee() {
        let dir = temp_dir("task-review");
        let manifest_path = write_session_manifest(&dir);

        // Create a task on the board for this workspace.
        let ctx = resolve_tasks_context(&dir).unwrap();
        create_task(
            &ctx.tasks_dir,
            CreateTask {
                title: "Review me".to_string(),
                id: Some("review-test".to_string()),
                assignee: Some("coder".to_string()),
                blocked_by: vec![],
                created_by: None,
                body: None,
            },
        )
        .unwrap();

        // Serialize with the global env lock because `cargo test` runs these
        // tests in parallel in the same process.
        let _env_guard = crate::test_support::env_lock();
        unsafe {
            std::env::set_var("VIA_SESSION", &manifest_path);
        }

        let result = run_review("review-test".to_string(), false);

        unsafe {
            std::env::remove_var("VIA_SESSION");
        }

        result.unwrap();

        let task = get_task(&ctx.tasks_dir, "review-test").unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::Review);
        assert_eq!(
            task.assignee.as_deref(),
            Some(crate::config::HUMAN_ASSIGNEE_ID)
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `via task review <id>` on a missing task bails with a clear error.
    #[test]
    fn run_review_missing_task_errors() {
        let dir = temp_dir("task-review-missing");
        let manifest_path = write_session_manifest(&dir);

        // Serialize with the global env lock because `cargo test` runs these
        // tests in parallel in the same process.
        let _env_guard = crate::test_support::env_lock();
        unsafe {
            std::env::set_var("VIA_SESSION", &manifest_path);
        }

        let result = run_review("nonexistent".to_string(), false);

        unsafe {
            std::env::remove_var("VIA_SESSION");
        }

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("task not found"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
