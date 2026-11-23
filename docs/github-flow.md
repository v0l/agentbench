# GitHub flow

A project can use its GitHub repo as the work list instead of agentbench's own tracker. People open issues on GitHub, the autopilot triages them, works the ones you mark ready, opens pull requests, reviews pull requests, and waits for you before anything is posted or merged.

agentbench keeps a mirror of the repo's open issues, pull requests and comments in its own database, so the autopilot, the supervisor, the app and the CLI work the same way for both kinds of project. All GitHub access goes through the `gh` CLI on the server machine; agentbench stores no GitHub token. Set `AGENTBENCH_GH` to use another `gh` binary.

## Turning it on

In the app, open the project (projects tab) and set **tracker** to github in its GitHub section. The repo is read from the project's remote; type `owner/name` in **repo** to use another. The project gets a GITHUB badge and its issues tab shows GitHub work first.

From the command line:

```
agentbench github status <project>
agentbench github sync <project>
```

`status` says whether `gh` is installed and logged in, which repo is used, whether you can write to it, how much rate limit is left, and when the last sync ran. Every `agentbench github` command answers in JSON.

## Turning it off

Set **tracker** back to internal. Nothing is deleted: mirrored issues stay in the database, internal issues are untouched, and no more syncs, triage or posts happen for that project. To stop posting everywhere at once without changing any project, use the pause switch (below).

## Trust

GitHub text is untrusted input. Each author gets a level from their permission on the repo, cached for an hour:

| who | level |
|---|---|
| owner, admin or a collaborator with write access | trusted |
| everyone else | outside contributor |

The app shows the level as a badge on every issue, comment and pull request. The server enforces the rules, not a prompt:

- An issue from an outside contributor is mirrored and triaged, but no agent starts work on it until a trusted person marks it ready: the `ready` label, or a `/ready` comment by a trusted user. A `/ready` from anyone else does nothing.
- When its text goes into an agent prompt it is wrapped in an untrusted block naming the author, and agents are told not to follow instructions in it.
- Triage may label and comment on it, through the outbox, but never close it.
- Workers on such an issue write to GitHub only through the outbox.
- New issues taken into triage are capped per author and per repo per hour (`autopilot_intake_per_hour`, default 20), and agent turns on untrusted issues per day (`autopilot_untrusted_turns_per_day`, default 50). Over a cap the issue stays in backlog with a note.

Workers never run `gh issue`, `gh pr` write commands or `git push` themselves; the guard rules refuse them.

## The outbox and post modes

Every write to GitHub (a comment, a label added or removed, an assignee, closing or reopening an issue, a pull request review, a merge request, a new issue) is first a row in the outbox, with who asked and the exact text that would be posted. The project's **post mode** decides what happens to it:

| mode | what happens |
|---|---|
| draft (default) | Every row waits for you. You approve, edit, or reject it. |
| auto | Rows from trusted input are posted at once. Rows touched by untrusted input, approvals and change requests on pull requests, and merges still wait for you. |
| off | Nothing leaves the machine. Rows are kept as local notes. |

In the app the **outbox** tab of a GitHub project shows each row as a card with what will be posted. Text rows can be edited before you approve them. **approve all** approves every waiting row, **pause all posting** stops the drain everywhere until you turn it off, and sent rows link to the comment or review on GitHub.

From the command line:

```
agentbench github outbox --project <project> [--status proposed]
agentbench github approve <row> [--text "edited text"]
agentbench github approve --all <project>
agentbench github reject <row>
agentbench github pause
agentbench github resume
```

The supervisor reads the same rows with its `github_outbox` tool, and its `list_issues` names each issue's source, GitHub number, url and author.

Posted comments end with a line saying agentbench posted them for the repo owner. Text that looks like a secret or a path on this machine is refused before it is queued and again before it is sent. A 5xx answer is retried with backoff; a 4xx answer marks the row failed with GitHub's message.

## Pull requests

The **pull requests** tab lists the open pull requests with their checks, the review verdict for the current head, and a one line summary. **review now** asks for a review of the current head. Reviews run on the high model in a throwaway worktree; a pull request from an outside contributor is read only, its code is never run. The review lands in the outbox like any other post.

**merge** is for you only. The server merges with `gh pr merge` and the project's merge method, and only when the checks pass and you approved the review row of the current head in the outbox. Branch protection is never bypassed. On your own pull request GitHub refuses approve and request changes, so the drain posts the review as a comment that says agentbench reviewed it; the row keeps its verdict, so an approved row still unblocks the merge. A failed post shows GitHub's own message in the row error. **automerge: trusted** lets agentbench merge its own pull requests for ready issues once checks pass and the review approves.

```
agentbench github pulls --project <project>
agentbench github review <number> --project <project>
agentbench github merge <number> --project <project>
```

## Syncing and the webhook

Without a webhook the server polls each GitHub project every 60 seconds, using conditional requests so an unchanged repo costs no rate limit. Change the interval with **poll every** in the app or `PUT /api/settings/github {"poll_secs": N}`.

A webhook makes changes show up within seconds:

1. Set **public address** to where GitHub can reach this server, for example `https://bench.example.com`. Only one path needs to be reachable from the internet: `/api/github/webhook/<project id>`. With nginx in front of the server:

   ```
   location /api/github/webhook/ {
       proxy_pass http://127.0.0.1:7420;
   }
   ```

2. Press **make secret**. The secret is shown once; agentbench keeps it and never shows it again (make a new one to replace it).
3. Press **create webhook**, or run `agentbench github webhook <project> --create`. It creates or updates the webhook on the repo through `gh`, which needs admin rights on the repo. You can also add it by hand on GitHub: payload URL from the app, content type `application/json`, the secret, and the events issues, issue comments, pull requests, pull request reviews, review comments, check suites and labels.

The receiver checks `X-Hub-Signature-256` against the secret and answers 401 with no body otherwise, ignores repeated delivery ids and events for other repos, and uses each delivery only as a hint: it refetches from GitHub through `gh` and never trusts the payload for state. While deliveries arrive, polling slows to every five minutes.

To turn the webhook off, delete it on GitHub (repo settings, webhooks). Polling carries on as before.
