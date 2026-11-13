use agentbench_core::Automation;

pub const PROPOSAL: &str = "proposal:";

pub fn required(tool: &str) -> Option<Automation> {
    Some(match tool {
        "file_issue" | "update_issue" | "comment_on_issue" | "remember" | "rename_session" => {
            Automation::Assist
        }
        "start_session" | "send_message" | "stop_session" | "close_session"
        | "create_worktree" | "cleanup_worktree" => Automation::Delegate,
        "merge_branch" => Automation::Ship,
        _ => return None,
    })
}
