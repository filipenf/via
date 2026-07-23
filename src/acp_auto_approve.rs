//! ACP permission auto-approve policy (via:nce2).
//!
//! Built-in allows (always on):
//! - All shell commands whose base executable is `via` (any subcommand/args).
//! - ACP tool kinds `read` and `search` when `tool_kind` is known.
//! - Read-only shell commands: `ls`, `pwd`, `cat`, `head`, `tail`, `rg`, `fd`, and
//!   `git` with subcommands `status`, `diff`, `log`, or `show`.
//!
//! User extensions in `via.conf` `[auto_approve]` add extra command base names and kinds;
//! they never disable built-ins. Miss → modal (never auto-deny).

use std::path::Path;

use crate::config::AutoApproveConfig;
use crate::event::AcpPermissionOption;

/// Resolved allow rules: built-ins plus optional user extensions from config.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AutoApprovePolicy {
    extra_commands: Vec<String>,
    extra_kinds: Vec<String>,
}

/// Inputs for a single `session/request_permission` decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PermissionContext<'a> {
    pub command: Option<&'a str>,
    pub tool_kind: Option<&'a str>,
}

impl AutoApprovePolicy {
    pub fn from_config(config: &AutoApproveConfig) -> Self {
        Self {
            extra_commands: config.commands.clone(),
            extra_kinds: config.kinds.clone(),
        }
    }

    pub fn allows(&self, ctx: PermissionContext<'_>) -> bool {
        if let Some(kind) = ctx.tool_kind {
            if is_blocked_kind(kind) {
                return false;
            }
            if is_builtin_kind(kind) || self.extra_kinds.iter().any(|k| k == kind) {
                return true;
            }
        }
        if let Some(command) = ctx.command {
            if command_allowed(command, &self.extra_commands) {
                return true;
            }
        }
        false
    }
}

fn is_blocked_kind(kind: &str) -> bool {
    matches!(kind, "edit" | "delete" | "move")
}

fn is_builtin_kind(kind: &str) -> bool {
    matches!(kind, "read" | "search")
}

fn command_base_token(command: &str) -> &str {
    command.split_whitespace().next().unwrap_or("")
}

fn executable_name(token: &str) -> &str {
    Path::new(token)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(token)
}

fn command_allowed(command: &str, extra_commands: &[String]) -> bool {
    let base = command_base_token(command);
    if base.is_empty() {
        return false;
    }
    let name = executable_name(base);
    if name == "via" {
        return true;
    }
    if matches!(name, "ls" | "pwd" | "cat" | "head" | "tail" | "rg" | "fd") {
        return true;
    }
    if name == "git" {
        return git_subcommand_allowed(command);
    }
    extra_commands.iter().any(|allowed| allowed == name)
}

fn git_subcommand_allowed(command: &str) -> bool {
    let mut parts = command.split_whitespace();
    let base = parts.next().unwrap_or("");
    if executable_name(base) != "git" {
        return false;
    }
    matches!(
        parts.next(),
        Some("status") | Some("diff") | Some("log") | Some("show")
    )
}

/// Pick an allow option for auto-approve; prefer `allow-once` over `allow-always`.
///
/// Returns `None` when no option looks like an allow (caller must fall through to the modal).
/// Never returns deny/cancel/reject ids — that would auto-deny and contradict miss→modal.
pub fn pick_allow_option(options: &[AcpPermissionOption]) -> Option<String> {
    for opt in options {
        let id = opt.option_id.as_str();
        if id == "allow-once" || id.ends_with("allow-once") || id == "allow_once" {
            return Some(opt.option_id.clone());
        }
    }
    for opt in options {
        let lower = opt.option_id.to_ascii_lowercase();
        if lower.contains("allow") && !lower.contains("always") {
            return Some(opt.option_id.clone());
        }
    }
    for opt in options {
        if opt.option_id.to_ascii_lowercase().contains("allow") {
            return Some(opt.option_id.clone());
        }
    }
    None
}

/// JSON-RPC `result` for an approved `session/request_permission` (same shape as the modal).
pub fn permission_result_for_option(option_id: &str) -> serde_json::Value {
    serde_json::json!({
        "outcome": {
            "outcome": "selected",
            "optionId": option_id,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(extra_commands: &[&str], extra_kinds: &[&str]) -> AutoApprovePolicy {
        AutoApprovePolicy {
            extra_commands: extra_commands.iter().map(|s| (*s).to_string()).collect(),
            extra_kinds: extra_kinds.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn allows_via_commands() {
        let p = AutoApprovePolicy::default();
        assert!(p.allows(PermissionContext {
            command: Some("via task list"),
            tool_kind: None,
        }));
        assert!(p.allows(PermissionContext {
            command: Some("/usr/bin/via agent send -m hi"),
            tool_kind: Some("execute"),
        }));
    }

    #[test]
    fn allows_read_and_search_kinds() {
        let p = AutoApprovePolicy::default();
        assert!(p.allows(PermissionContext {
            command: None,
            tool_kind: Some("read"),
        }));
        assert!(p.allows(PermissionContext {
            command: None,
            tool_kind: Some("search"),
        }));
    }

    #[test]
    fn blocks_edit_delete_move_kinds() {
        let p = AutoApprovePolicy::default();
        for kind in ["edit", "delete", "move"] {
            assert!(!p.allows(PermissionContext {
                command: Some("via task list"),
                tool_kind: Some(kind),
            }));
        }
    }

    #[test]
    fn allows_readonly_shell_builtins() {
        let p = AutoApprovePolicy::default();
        for cmd in [
            "ls -la",
            "pwd",
            "cat README.md",
            "head -n 5 file",
            "tail -f log",
            "rg foo",
            "fd via",
            "git status",
            "git diff main",
            "git log -1",
            "git show HEAD",
        ] {
            assert!(
                p.allows(PermissionContext {
                    command: Some(cmd),
                    tool_kind: None,
                }),
                "expected allow for `{cmd}`"
            );
        }
    }

    #[test]
    fn rejects_unlisted_shell_commands() {
        let p = AutoApprovePolicy::default();
        for cmd in ["cargo test", "git push", "rm -rf /", "curl example.com"] {
            assert!(
                !p.allows(PermissionContext {
                    command: Some(cmd),
                    tool_kind: None,
                }),
                "expected deny modal for `{cmd}`"
            );
        }
    }

    #[test]
    fn config_extends_commands_and_kinds() {
        let p = policy(&["echo"], &["fetch"]);
        assert!(p.allows(PermissionContext {
            command: Some("echo hi"),
            tool_kind: None,
        }));
        assert!(p.allows(PermissionContext {
            command: None,
            tool_kind: Some("fetch"),
        }));
    }

    #[test]
    fn allows_via_with_relative_or_absolute_path() {
        let p = AutoApprovePolicy::default();
        for cmd in [
            "./via task list",
            "../bin/via agent inbox",
            "/home/user/.cargo/bin/via task show lr3m",
        ] {
            assert!(
                p.allows(PermissionContext {
                    command: Some(cmd),
                    tool_kind: None,
                }),
                "expected allow for `{cmd}`"
            );
        }
    }

    #[test]
    fn rejects_empty_or_git_write_subcommands() {
        let p = AutoApprovePolicy::default();
        assert!(!p.allows(PermissionContext {
            command: Some("   "),
            tool_kind: None,
        }));
        for cmd in ["git push", "git commit", "git reset --hard"] {
            assert!(
                !p.allows(PermissionContext {
                    command: Some(cmd),
                    tool_kind: None,
                }),
                "expected modal for `{cmd}`"
            );
        }
    }

    #[test]
    fn execute_kind_without_allowed_command_still_prompts() {
        let p = AutoApprovePolicy::default();
        assert!(!p.allows(PermissionContext {
            command: Some("cargo test"),
            tool_kind: Some("execute"),
        }));
    }

    #[test]
    fn permission_result_matches_modal_shape() {
        let result = permission_result_for_option("allow-once");
        assert_eq!(result["outcome"]["outcome"], "selected");
        assert_eq!(result["outcome"]["optionId"], "allow-once");
    }

    #[test]
    fn from_config_preserves_user_extensions() {
        let config = AutoApproveConfig {
            commands: vec!["wc".to_string()],
            kinds: vec!["think".to_string()],
        };
        let p = AutoApprovePolicy::from_config(&config);
        assert!(p.allows(PermissionContext {
            command: Some("wc -l"),
            tool_kind: None,
        }));
        assert!(p.allows(PermissionContext {
            command: None,
            tool_kind: Some("think"),
        }));
        assert!(!p.allows(PermissionContext {
            command: Some("cargo test"),
            tool_kind: None,
        }));
    }

    #[test]
    fn pick_allow_option_prefers_allow_once() {
        let options = vec![
            AcpPermissionOption {
                option_id: "allow-always".to_string(),
                name: "Always".to_string(),
            },
            AcpPermissionOption {
                option_id: "allow-once".to_string(),
                name: "Once".to_string(),
            },
        ];
        assert_eq!(pick_allow_option(&options).as_deref(), Some("allow-once"));
    }

    #[test]
    fn pick_allow_option_returns_none_without_allow() {
        let options = vec![
            AcpPermissionOption {
                option_id: "deny".to_string(),
                name: "Deny".to_string(),
            },
            AcpPermissionOption {
                option_id: "cancel".to_string(),
                name: "Cancel".to_string(),
            },
        ];
        assert_eq!(pick_allow_option(&options), None);
        assert_eq!(pick_allow_option(&[]), None);
    }
}
