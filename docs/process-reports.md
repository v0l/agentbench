# Process reports

Agentbench keeps one loop for improving how agents work: one queue, one dedup, one daily digest and one rules file.

## The queue

Process problems are issues labelled `process` in the agentbench project (or the project named by the `process_project` setting). They are filed in backlog with no goal, so the autopilot never starts them.

Four sources feed it:

| Source | When |
|---|---|
| `report_process_issue` | An agent calls it when the process, not the code, cost it time. Fields: `category`, `what_happened`, `cost`, `suggestion`. The session, project, issue and model are attached. At most 3 per session per turn. |
| A `process:` line | A reply that ends with `process: <what happened>` is filed when the turn ends. |
| Auto-capture | An undelivered `send_message`, the same harness warning 3 times in one session (bash edits, bash sleeps), the same tool error 3 times in one session, and turns stopped by a server restart. |
| User corrections | A user message that reads as a correction ("why didn't you", "I thought that was clear", "always", "never", "come on") is filed with the category `user correction`. |

## Dedup

Before filing, the open `process` issues are checked for the same problem: the same capture key, or the same category with similar text. A match adds a comment with the new occurrence, adds its cost, and bumps the count in the title (`(x3)`).

## The daily digest

Once a day the supervisor gets a process digest as a new turn: the new and bumped `process` issues ranked by count, then cost, and the supervisor notes that look stale (unchanged for 30 days) or duplicated. An item seen twice or more is marked "make it a guard".

Each item ends in exactly one outcome:

- a note or `AGENTS.md` edit, when the fix is guidance;
- a rule in the guard rules file, when the rule can be checked;
- a todo issue with the concrete fix, when it needs code;
- closed as noise.

## Guard rules

Guards read one file, `guards.toml` in the memory folder (`~/.agents/memory/guards.toml` by default). It is seeded with three rules:

- `worker-no-push`: a brief, message or issue body telling a worker to push to main or master is refused.
- `pr-review-model`: a PR review session not on the top model is refused.
- `pr-reviews`: posting GitHub comments or reviews from bash asks the user first.

A rule matches a tool call when one of its `fields` matches `pattern`, `unless` does not match, and, when set, `require_field` fails to match `require`. `action` is `refuse` or `ask`. The supervisor adds or removes rules with the `guard_rules` tool; new rules need no code.

```toml
[[rule]]
id = "worker-no-push"
tools = ["start_session", "send_message", "file_issue"]
fields = ["prompt", "text", "body"]
pattern = '(?i)\bpush\b[^.\n]{0,40}\b(to|into|onto)\s+(origin[ /])?(main|master)\b'
unless = '(?i)\b(never|not|don.t|do not|no|without)\s+push'
action = "refuse"
message = "Workers never push to main or master."
```
