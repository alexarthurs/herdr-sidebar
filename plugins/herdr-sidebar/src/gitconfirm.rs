//! Opt-in y/N confirmations for Git actions that rewrite history or touch the
//! remote (issue #118). Both views share this so the Explorer's footer sync
//! and Source Control's many sync entry points cannot drift apart.
//!
//! The decision and the prompt wording are pure so they can be tested without
//! constructing either app. Destructive actions (discard, reset, delete,
//! drop, remove worktree) always confirm and do not route through here.

/// Settings row label, shared by both views' ⚙ modals. Kept short enough
/// that `label + "off"` fits the default 32-column sidebar (the modal is
/// `label + value + 5` wide).
pub const SETTING_LABEL: &str = "Confirm risky Git ops";

/// A one-click Git action that `confirm_git_ops` may guard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitOp<'a> {
    /// `pull --rebase --autostash`, then `push`.
    Sync {
        branch: &'a str,
        ahead: usize,
        behind: usize,
    },
    Merge {
        source: &'a str,
        into: &'a str,
    },
    CherryPick {
        commit: &'a str,
        onto: &'a str,
    },
    Revert {
        commit: &'a str,
    },
    StashPop {
        stash: &'a str,
    },
    /// Drawer checkout of a branch, or a detached switch to a commit/tag.
    Checkout {
        target: &'a str,
        detached: bool,
    },
    /// Only moves remote-tracking refs; nothing local can be lost.
    Fetch,
    /// Keeps the stash entry, so a bad apply is recoverable.
    StashApply,
}

impl GitOp<'_> {
    /// Whether this action asks first. Off (the default) never asks, so
    /// existing users keep one-click behavior.
    pub fn needs_confirm(&self, enabled: bool) -> bool {
        enabled && !matches!(self, GitOp::Fetch | GitOp::StashApply)
    }

    /// The y/N question, naming the effect. Callers wrap it to the pane.
    pub fn prompt(&self) -> String {
        match *self {
            GitOp::Sync {
                branch,
                ahead,
                behind,
            } => {
                let mut counts = Vec::new();
                if ahead > 0 {
                    counts.push(format!("{ahead}↑"));
                }
                if behind > 0 {
                    counts.push(format!("{behind}↓"));
                }
                let counts = if counts.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", counts.join(" "))
                };
                format!(
                    "Sync {}: pull --rebase, then push{counts}? (y/N)",
                    or_head(branch)
                )
            }
            GitOp::Merge { source, into } => {
                format!("Merge {source} into {}? (y/N)", or_head(into))
            }
            GitOp::CherryPick { commit, onto } => {
                format!("Cherry-pick {commit} onto {}? (y/N)", or_head(onto))
            }
            GitOp::Revert { commit } => format!("Revert {commit} with a new commit? (y/N)"),
            GitOp::StashPop { stash } => format!("Pop {stash} (apply, then drop)? (y/N)"),
            GitOp::Checkout {
                target,
                detached: false,
            } => format!("Check out {target}? (y/N)"),
            GitOp::Checkout {
                target,
                detached: true,
            } => format!("Check out {target} (detached HEAD)? (y/N)"),
            GitOp::Fetch => "Fetch? (y/N)".to_string(),
            GitOp::StashApply => "Apply stash? (y/N)".to_string(),
        }
    }
}

impl crate::git::SyncTarget {
    pub fn prompt(&self, status: &crate::git::Status) -> String {
        // The last status can belong to a branch that was just checked out.
        let (ahead, behind) = if self.branch == status.branch {
            (status.ahead, status.behind)
        } else {
            (0, 0)
        };
        GitOp::Sync {
            branch: &self.branch,
            ahead,
            behind,
        }
        .prompt()
    }
}

/// The free-function form of [`GitOp::needs_confirm`].
pub fn needs_confirm(op: &GitOp<'_>, enabled: bool) -> bool {
    op.needs_confirm(enabled)
}

fn or_head(branch: &str) -> &str {
    if branch.is_empty() { "HEAD" } else { branch }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUARDED: [GitOp<'static>; 7] = [
        GitOp::Sync {
            branch: "main",
            ahead: 1,
            behind: 0,
        },
        GitOp::Merge {
            source: "feature",
            into: "main",
        },
        GitOp::CherryPick {
            commit: "abc1234",
            onto: "main",
        },
        GitOp::Revert { commit: "abc1234" },
        GitOp::StashPop { stash: "stash@{0}" },
        GitOp::Checkout {
            target: "feature",
            detached: false,
        },
        GitOp::Checkout {
            target: "v1.0",
            detached: true,
        },
    ];

    #[test]
    fn off_never_asks() {
        for op in GUARDED.iter().chain(&[GitOp::Fetch, GitOp::StashApply]) {
            assert!(!needs_confirm(op, false), "{op:?}");
        }
    }

    #[test]
    fn on_guards_history_and_remote_ops_but_not_recoverable_ones() {
        for op in &GUARDED {
            assert!(needs_confirm(op, true), "{op:?}");
        }
        assert!(!needs_confirm(&GitOp::Fetch, true));
        assert!(!needs_confirm(&GitOp::StashApply, true));
    }

    #[test]
    fn prompts_name_the_effect() {
        assert_eq!(
            GitOp::Sync {
                branch: "main",
                ahead: 2,
                behind: 1
            }
            .prompt(),
            "Sync main: pull --rebase, then push (2↑ 1↓)? (y/N)"
        );
        assert_eq!(
            GitOp::Sync {
                branch: "main",
                ahead: 0,
                behind: 3
            }
            .prompt(),
            "Sync main: pull --rebase, then push (3↓)? (y/N)"
        );
        assert_eq!(
            GitOp::Sync {
                branch: "",
                ahead: 0,
                behind: 0
            }
            .prompt(),
            "Sync HEAD: pull --rebase, then push? (y/N)"
        );
        assert_eq!(
            GitOp::Merge {
                source: "feature",
                into: "main"
            }
            .prompt(),
            "Merge feature into main? (y/N)"
        );
        assert_eq!(
            GitOp::CherryPick {
                commit: "abc1234",
                onto: "main"
            }
            .prompt(),
            "Cherry-pick abc1234 onto main? (y/N)"
        );
        assert_eq!(
            GitOp::Revert { commit: "abc1234" }.prompt(),
            "Revert abc1234 with a new commit? (y/N)"
        );
        assert_eq!(
            GitOp::StashPop { stash: "stash@{0}" }.prompt(),
            "Pop stash@{0} (apply, then drop)? (y/N)"
        );
        assert_eq!(
            GitOp::Checkout {
                target: "feature",
                detached: false
            }
            .prompt(),
            "Check out feature? (y/N)"
        );
        assert_eq!(
            GitOp::Checkout {
                target: "v1.0",
                detached: true
            }
            .prompt(),
            "Check out v1.0 (detached HEAD)? (y/N)"
        );
    }

    #[test]
    fn every_prompt_ends_with_the_default_no_hint() {
        for op in &GUARDED {
            assert!(op.prompt().ends_with("? (y/N)"), "{op:?}");
        }
    }

    #[test]
    fn prompts_wrap_whole_in_a_narrow_pane() {
        // Both views wrap confirm prompts with the « zone (4 cols) reserved.
        for op in &GUARDED {
            let lines = crate::ui::wrap_footer_message(&op.prompt(), 32, 4);
            assert!(lines.len() <= 3, "{op:?}: {lines:?}");
            assert!(
                lines.last().unwrap().ends_with("(y/N)"),
                "{op:?}: {lines:?}"
            );
        }
    }

    #[test]
    fn setting_row_fits_the_default_sidebar_width() {
        // draw_settings sizes the modal to label + value + 5 columns.
        let widest = SETTING_LABEL.chars().count() + "off".len() + 5;
        assert!(
            widest <= crate::state::DEFAULT_SIDEBAR_WIDTH as usize,
            "{widest}"
        );
    }
}
