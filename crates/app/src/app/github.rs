use super::*;
use agentbench_core::{
    API_GITHUB_APP, API_GITHUB_FLOW, API_GITHUB_WORK, API_PR_REVIEW, AutoMerge, GithubSettings, GithubStatus, IssueSource,
    MergeMethod, MergeMode, OutboxItem, OutboxList, PostMode, PullRequest, Tracker, Trust, WebhookSecret,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GhTab {
    Issues,
    Outbox,
    Pulls,
    Board,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Sent {
    Waiting,
    Posted,
    All,
}

#[derive(Default)]
pub struct GithubView {
    pub status: Option<GithubStatus>,
    pub outbox: Option<OutboxList>,
    pub pulls: Option<Vec<PullRequest>>,
    pub settings: Option<GithubSettings>,
    pub secret: Option<WebhookSecret>,
    edits: HashMap<Id, String>,
    repo: Option<String>,
    poll: Option<String>,
    base: Option<String>,
    asked: Option<Instant>,
    status_asked: Option<Instant>,
    syncing: bool,
}

const LIST_EVERY: Duration = Duration::from_secs(30);
const STATUS_EVERY: Duration = Duration::from_secs(120);

pub(super) fn trust_chip(ui: &mut Ui, trust: Trust) -> egui::Response {
    let (text, tint) = match trust {
        Trust::Untrusted => ("outside contributor", WARN),
        Trust::Trusted => ("trusted", OK),
    };
    let r = chip(ui, text, tint, trust == Trust::Trusted);
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, text));
    r
}

fn status_tint(status: &str) -> egui::Color32 {
    match status {
        "proposed" => READOUT,
        "failed" => FAULT,
        "done" => OK,
        "approved" | "posting" => TRACE,
        _ => LEGEND,
    }
}

fn status_word(status: &str) -> &str {
    match status {
        "proposed" => "waiting for you",
        "approved" => "queued",
        "posting" => "posting",
        "done" => "sent",
        "failed" => "failed",
        "rejected" => "rejected",
        "local" => "kept here",
        other => other,
    }
}

fn checks_look(pr: &PullRequest) -> (&'static str, egui::Color32) {
    if pr.checks.is_empty() || !pr.checks_known() {
        ("no checks yet", LEGEND)
    } else if pr.checks_red() {
        ("checks failed", FAULT)
    } else if pr.checks_green() {
        ("checks pass", OK)
    } else {
        ("checks running", TRACE)
    }
}

fn verdict_look(pr: &PullRequest) -> (&'static str, egui::Color32) {
    match (pr.reviewed_sha.as_deref() == Some(pr.head_sha.as_str()), pr.review_verdict.as_deref()) {
        (_, None) => ("not reviewed", LEGEND),
        (false, Some(_)) => ("review is for an older commit", LEGEND),
        (true, Some("approve")) => ("review: approve", OK),
        (true, Some("request_changes")) => ("review: changes asked", FAULT),
        (true, Some(_)) => ("review: comment", VALUE),
    }
}

fn first_line(text: &str) -> String {
    text.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#')).unwrap_or_default().to_string()
}

fn link_key(ui: &mut Ui, text: &str, url: &str) {
    let r = ui.add(Key::new(text).outline(VALUE));
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Link, true, text));
    if r.clicked() {
        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
    }
}

impl App {
    pub fn github_mode(&self, pid: Id) -> bool {
        self.server_has(pid, API_GITHUB_FLOW)
            && self.overview.as_ref().is_some_and(|o| o.projects.iter().any(|p| p.id == pid && p.tracker.is_github()))
    }

    pub fn github_view(&self, pid: Id) -> Option<&GithubView> {
        self.github.get(&pid)
    }

    pub fn notice(&self) -> Option<(bool, String)> {
        self.notice.clone()
    }

    pub fn show_issues(&mut self) {
        self.tab = Tab::Issues;
        self.scope = Scope::Project;
        self.detail = None;
    }

    pub fn set_github_tab(&mut self, tab: GhTab) {
        self.gh_tab = tab;
    }

    pub fn github_tab(&self) -> GhTab {
        self.gh_tab
    }

    pub(super) fn github_replied(&mut self, reply: Reply) {
        match reply {
            Reply::GithubStatus(pid, s) => {
                let v = self.github.entry(pid).or_default();
                v.status = Some(s);
                v.syncing = false;
            }
            Reply::Outbox(pid, list) => self.github.entry(pid).or_default().outbox = Some(list),
            Reply::Pulls(pid, list) => self.github.entry(pid).or_default().pulls = Some(list),
            Reply::GithubSettings(pid, s) => self.github.entry(pid).or_default().settings = Some(s),
            Reply::WebhookSecret(pid, s) => {
                let v = self.github.entry(pid).or_default();
                v.secret = Some(s);
                v.status_asked = None;
            }
            _ => {}
        }
    }

    pub(super) fn github_stale(&mut self) {
        for v in self.github.values_mut() {
            v.asked = None;
        }
    }

    pub(super) fn github_fetch(&mut self, pid: Id, with_status: bool) {
        if !self.server_has(pid, API_PR_REVIEW) {
            return;
        }
        let settings = self.server_has(pid, API_GITHUB_APP);
        let v = self.github.entry(pid).or_default();
        let Some(c) = &mut self.client else { return };
        if v.asked.is_none_or(|t| t.elapsed() > LIST_EVERY) {
            v.asked = Some(Instant::now());
            c.outbox(pid);
            c.pulls(pid);
        }
        if with_status && v.status_asked.is_none_or(|t| t.elapsed() > STATUS_EVERY) {
            v.status_asked = Some(Instant::now());
            c.github_status(pid);
            if settings {
                c.github_settings(pid);
            }
        }
    }

    fn outbox_items(&self, pid: Id) -> Vec<OutboxItem> {
        self.github.get(&pid).and_then(|v| v.outbox.as_ref()).map(|o| o.items.clone()).unwrap_or_default()
    }

    fn pulls_of(&self, pid: Id) -> Vec<PullRequest> {
        self.github.get(&pid).and_then(|v| v.pulls.clone()).unwrap_or_default()
    }

    fn repo_of(&self, pid: Id) -> String {
        self.overview
            .as_ref()
            .and_then(|o| o.projects.iter().find(|p| p.id == pid))
            .and_then(Project::github_repo)
            .unwrap_or_default()
    }

    pub(super) fn github_issues_page(&mut self, ui: &mut Ui, pid: Id) -> bool {
        self.github_fetch(pid, false);
        let items = self.outbox_items(pid);
        let pulls = self.pulls_of(pid);
        let waiting = items.iter().filter(|i| i.waiting()).count();
        let open_prs = pulls.iter().filter(|p| p.state == "open").count();
        let issues_n = self
            .overview
            .as_ref()
            .map_or(0, |o| o.issues.iter().filter(|i| i.project_id == pid && i.source == IssueSource::Github && i.state.is_open()).count());
        ui.horizontal_wrapped(|ui| {
            chip(ui, "github", VALUE, true).widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, "github project"));
            let repo = self.repo_of(pid);
            if !repo.is_empty() {
                Line::new().value(&repo).size(11.5).show(ui);
            }
            ui.push_id("scope", |ui| {
                segments(ui, &mut self.scope, &[(Scope::Project, "this project", None), (Scope::Everything, "all projects", None)]);
            });
        });
        ui.add_space(6.0);
        let pulls_known = self.server_has(pid, API_PR_REVIEW);
        let mut tabs: Vec<(GhTab, String)> = vec![(GhTab::Issues, format!("issues {issues_n}"))];
        if pulls_known {
            tabs.push((GhTab::Outbox, format!("outbox {waiting}")));
            tabs.push((GhTab::Pulls, format!("pull requests {open_prs}")));
        }
        tabs.push((GhTab::Board, "board".into()));
        if !pulls_known && matches!(self.gh_tab, GhTab::Outbox | GhTab::Pulls) {
            self.gh_tab = GhTab::Issues;
        }
        ui.horizontal_wrapped(|ui| {
            for (tab, text) in &tabs {
                if tab_chip(ui, text, READOUT, self.gh_tab == *tab).clicked() {
                    self.gh_tab = *tab;
                }
            }
        });
        ui.add_space(8.0);
        if self.scope != Scope::Project {
            return false;
        }
        match self.gh_tab {
            GhTab::Board => false,
            GhTab::Issues => {
                egui::ScrollArea::vertical().id_salt("gh-issues").auto_shrink([false, false]).show(ui, |ui| {
                    ui.set_max_width(ui.available_width());
                    self.github_issue_list(ui, pid, &items, &pulls);
                });
                true
            }
            GhTab::Outbox => {
                self.outbox_page(ui, pid, &items);
                true
            }
            GhTab::Pulls => {
                egui::ScrollArea::vertical().id_salt("gh-pulls").auto_shrink([false, false]).show(ui, |ui| {
                    ui.set_max_width(ui.available_width());
                    self.pulls_page(ui, pid, &pulls, &items);
                });
                true
            }
        }
    }

    fn github_issue_list(&mut self, ui: &mut Ui, pid: Id, items: &[OutboxItem], pulls: &[PullRequest]) {
        let Some(o) = self.overview.clone() else { return };
        let everything = self.filter == Filter::All;
        let mine: Vec<&Issue> = o.issues.iter().filter(|i| i.project_id == pid).filter(|i| everything || i.state.is_open()).collect();
        let (mut gh, mut internal): (Vec<&Issue>, Vec<&Issue>) = mine.into_iter().partition(|i| i.source == IssueSource::Github);
        gh.sort_by_key(|i| std::cmp::Reverse(i.external_number.unwrap_or(0)));
        sort_issues(&mut internal, self.sort);
        let all_n = o.issues.iter().filter(|i| i.project_id == pid).count();
        let open_n = o.issues.iter().filter(|i| i.project_id == pid && i.state.is_open()).count();
        segments(ui, &mut self.filter, &[(Filter::Open, "open", Some(open_n)), (Filter::All, "all", Some(all_n))]);
        if self.filter == Filter::Process {
            self.filter = Filter::Open;
        }
        ui.add_space(6.0);
        let mut open = None;
        heading(ui, "on github");
        if gh.is_empty() {
            let synced = self.github.get(&pid).and_then(|v| v.status.as_ref()).and_then(|s| s.last_sync);
            note(
                ui,
                if synced.is_some() { "No open GitHub issues." } else { "Nothing mirrored yet. The first sync brings the open issues and pull requests in." },
                LEGEND,
            );
        }
        for i in gh {
            let n = i.external_number.unwrap_or_default();
            let waiting = items.iter().filter(|x| x.number == n && x.waiting()).count();
            let pr = pulls.iter().find(|p| p.issue_id == Some(i.id) && p.state == "open");
            let r = super::projects::clickable_row(ui, ("gh-issue", i.id), |ui| {
                Line::new().value(format!("#{n}")).tint(LEGEND).size(11.5).gap(8.0).value(&i.title).size(12.5).tint(VALUE).wrapped(ui);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    chip(ui, i.state.as_str(), super::projects::state_tint(i.state), i.state != IssueState::Backlog);
                    trust_chip(ui, i.trust);
                    if let Some(pr) = pr {
                        let text = format!("pr #{}", pr.number);
                        chip(ui, &text, TRACE, true).widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, &text));
                    }
                    if waiting > 0 {
                        let text = format!("{waiting} to approve");
                        chip(ui, &text, READOUT, true).widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, &text));
                    }
                    for l in i.labels.iter().take(6) {
                        chip(ui, l, LEGEND, false);
                    }
                });
                let by = i.external_author.clone().unwrap_or_else(|| "unknown".into());
                Line::new().note(format!("by {by}  updated {}", ago(i.updated_at))).size(10.5).elided(ui);
            });
            r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("github issue #{n} {}", i.title)));
            if r.clicked() {
                open = Some(i.id);
            }
        }
        ui.add_space(10.0);
        heading(ui, "internal");
        hint(ui, "kept in agentbench only, never posted to GitHub");
        if internal.is_empty() {
            note(ui, "No internal issues.", LEGEND);
        }
        for i in internal {
            let r = super::projects::clickable_row(ui, ("int-issue", i.id), |ui| {
                Line::new().value(format!("#{}", crate::hub::untag(i.id))).tint(LEGEND).size(11.0).gap(8.0).value(&i.title).size(12.0).tint(VALUE).wrapped(ui);
                ui.horizontal_wrapped(|ui| {
                    chip(ui, i.state.as_str(), super::projects::state_tint(i.state), i.state != IssueState::Backlog);
                    for l in i.labels.iter().take(4) {
                        chip(ui, l, LEGEND, false);
                    }
                });
            });
            r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("internal issue #{} {}", crate::hub::untag(i.id), i.title)));
            if r.clicked() {
                open = Some(i.id);
            }
        }
        if let Some(id) = open {
            self.detail = Some(id);
            self.comment.clear();
            if let Some(c) = &mut self.client {
                c.comments(id);
            }
        }
    }

    fn outbox_page(&mut self, ui: &mut Ui, pid: Id, items: &[OutboxItem]) {
        let paused = self.github.get(&pid).and_then(|v| v.outbox.as_ref()).is_some_and(|o| o.paused);
        let waiting: Vec<&OutboxItem> = items.iter().filter(|i| i.waiting()).collect();
        let sent = items.iter().filter(|i| i.sent()).count();
        let mut all = false;
        let mut flip = false;
        ui.horizontal_wrapped(|ui| {
            let t = toggle(ui, "pause all posting", paused);
            t.widget_info(|| egui::WidgetInfo::selected(egui::Role::CheckBox, true, paused, "pause all posting"));
            flip = t.clicked();
            let key = ui.add(Key::new(format!("approve all {}", waiting.iter().filter(|i| i.status == "proposed").count())).solid(READOUT).enabled(!waiting.is_empty()));
            key.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, !waiting.is_empty(), "approve all"));
            all = key.clicked();
        });
        if paused {
            note(ui, "Posting is paused everywhere. Approved rows wait until you turn it back on.", WARN);
        }
        ui.add_space(4.0);
        segments(ui, &mut self.outbox_shown, &[(Sent::Waiting, "waiting", Some(waiting.len())), (Sent::Posted, "sent", Some(sent)), (Sent::All, "all", Some(items.len()))]);
        ui.add_space(6.0);
        let shown: Vec<&OutboxItem> = items
            .iter()
            .filter(|i| match self.outbox_shown {
                Sent::Waiting => i.waiting(),
                Sent::Posted => i.sent(),
                Sent::All => true,
            })
            .rev()
            .collect();
        egui::ScrollArea::vertical().id_salt("gh-outbox").auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(ui.available_width());
            if shown.is_empty() {
                note(
                    ui,
                    match self.outbox_shown {
                        Sent::Waiting => "Nothing waits for you. Agents put comments, labels, closes, reviews and merges here before anything reaches GitHub.",
                        _ => "Nothing here yet.",
                    },
                    LEGEND,
                );
            }
            for item in shown {
                self.outbox_card(ui, pid, item);
                ui.add_space(6.0);
            }
        });
        let bulk = self.server_has(pid, API_GITHUB_APP);
        if let Some(c) = &mut self.client {
            if flip {
                c.github_pause(pid, !paused);
            }
            if all {
                if bulk {
                    c.outbox_approve_all(pid);
                } else {
                    for i in waiting.iter().filter(|i| i.status == "proposed") {
                        c.outbox_approve(i.id, None);
                    }
                }
            }
        }
    }

    pub(super) fn outbox_card(&mut self, ui: &mut Ui, pid: Id, item: &OutboxItem) {
        let repo = self.repo_of(pid);
        let what = item.describe();
        let mut approve = false;
        let mut reject = false;
        let editable = item.waiting() && item.carries_text();
        let draft = self.github.entry(pid).or_default().edits.entry(item.id).or_insert_with(|| item.text.clone()).clone();
        let mut text = draft.clone();
        let r = card(
            ui,
            Some(status_tint(&item.status)),
            |ui| {
                Line::new().value(&what).size(12.0).tint(VALUE).elided(ui);
            },
            |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let s = status_word(&item.status);
                    chip(ui, s, status_tint(&item.status), item.waiting());
                    chip(ui, &format!("asked by {}", item.asked_by), LEGEND, false);
                    if !item.trusted {
                        chip(ui, "from untrusted input", WARN, true);
                    }
                    if !item.approved_by.is_empty() {
                        chip(ui, &format!("approved by {}", item.approved_by), LEGEND, false);
                    }
                });
                let params = item.params();
                if item.carries_text() {
                    Line::new().legend("posted as").size(10.0).show(ui);
                    if editable {
                        let r = ui.push_id(("outbox-edit-scope", item.id), |ui| agent_ui::form::prose_id(ui, ("outbox-edit", item.id), &mut text, "the text to post", 4, 220.0)).inner;
                        r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, format!("text of row {}", item.id)));
                    } else if !item.text.trim().is_empty() {
                        code_block(ui, &item.text, VALUE, ("outbox-text", item.id));
                    }
                    if item.kind == "issue_create" {
                        let title = params.get("title").and_then(serde_json::Value::as_str).unwrap_or_default();
                        Line::new().legend("title").gap(8.0).value(title).size(11.0).wrapped(ui);
                    }
                    if let Some(list) = params.get("comments").and_then(serde_json::Value::as_array).filter(|l| !l.is_empty()) {
                        Line::new().legend(&format!("{} line comments", list.len())).size(10.0).show(ui);
                        for c in list {
                            let at = format!(
                                "{}:{}",
                                c.get("path").and_then(serde_json::Value::as_str).unwrap_or("?"),
                                c.get("line").map(|l| l.to_string()).unwrap_or_default()
                            );
                            Line::new().measured(at).size(10.5).gap(8.0).value(c.get("body").and_then(serde_json::Value::as_str).unwrap_or_default()).size(11.0).wrapped(ui);
                        }
                    }
                    if !repo.is_empty() {
                        hint(ui, &format!("with the footer: {}", OutboxItem::footer(&repo).trim().trim_start_matches('-').trim()));
                    }
                }
                if let Some(e) = &item.error {
                    note(ui, e, if item.status == "failed" { FAULT } else { WARN });
                }
                ui.horizontal_wrapped(|ui| {
                    if item.waiting() {
                        let edited = editable && text != item.text;
                        let label = if edited { "approve edited" } else { "approve" };
                        let k = ui.add(Key::new(label).solid(READOUT).id_salt(("approve", item.id)));
                        k.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("approve row {}", item.id)));
                        approve = k.clicked();
                    }
                    if matches!(item.status.as_str(), "proposed" | "approved" | "failed" | "local") {
                        let k = ui.add(Key::new("reject").outline(FAULT).id_salt(("reject", item.id)));
                        k.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("reject row {}", item.id)));
                        reject = k.clicked();
                    }
                    if let Some(url) = &item.url {
                        link_key(ui, "open on github", url);
                    }
                    Line::new().note(format!("row {}  {}", crate::hub::untag(item.id), ago(item.updated_at))).size(10.5).show(ui);
                });
            },
        );
        r.response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Group, true, format!("outbox {what}")));
        if text != draft {
            self.github.entry(pid).or_default().edits.insert(item.id, text.clone());
        }
        if let Some(c) = &mut self.client {
            if approve {
                let edited = (editable && text != item.text).then_some(text.as_str());
                c.outbox_approve(item.id, edited);
            }
            if reject {
                c.outbox_reject(item.id);
            }
        }
        if (approve || reject)
            && let Some(v) = self.github.get_mut(&pid)
        {
            v.edits.remove(&item.id);
            v.asked = None;
        }
    }

    fn pulls_page(&mut self, ui: &mut Ui, pid: Id, pulls: &[PullRequest], items: &[OutboxItem]) {
        let mut open: Vec<&PullRequest> = pulls.iter().filter(|p| p.state == "open").collect();
        open.sort_by_key(|p| std::cmp::Reverse(p.number));
        if open.is_empty() {
            note(ui, "No open pull requests.", LEGEND);
        }
        let mut review = None;
        let mut merge = None;
        for pr in open {
            let (checks, ctint) = checks_look(pr);
            let (verdict, vtint) = verdict_look(pr);
            let summary = pr
                .review_row
                .and_then(|r| items.iter().find(|i| i.id == r))
                .map(|i| first_line(&i.text))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| first_line(&pr.body));
            let rail = if pr.checks_red() { FAULT } else if pr.checks_green() { OK } else { TRACE };
            let r = card(
                ui,
                Some(rail),
                |ui| {
                    Line::new().value(format!("#{}", pr.number)).tint(LEGEND).size(11.5).gap(8.0).value(&pr.title).size(12.0).tint(VALUE).elided(ui);
                },
                |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        chip(ui, checks, ctint, true).widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, checks));
                        chip(ui, verdict, vtint, pr.review_verdict.is_some()).widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, verdict));
                        trust_chip(ui, pr.trust);
                        if pr.by_agentbench {
                            chip(ui, "by agentbench", TRACE, false);
                        }
                        if pr.draft {
                            chip(ui, "draft", LEGEND, false);
                        }
                    });
                    if !summary.is_empty() {
                        Line::new().value(&summary).size(11.5).tint(VALUE).wrapped(ui);
                    }
                    let failed: Vec<&str> = pr.checks.iter().filter(|c| c.failed()).map(|c| c.name.as_str()).collect();
                    if !failed.is_empty() {
                        note(ui, format!("failing: {}", failed.join(", ")), FAULT);
                    }
                    Line::new().note(format!("by {}  {} into {}  updated {}", pr.author, pr.head_ref, pr.base_ref, ago(pr.updated_at))).size(10.5).wrapped(ui);
                    ui.horizontal_wrapped(|ui| {
                        let k = ui.add(Key::new("review now").outline(VALUE).id_salt(("review", pr.number)));
                        k.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("review pull request #{}", pr.number)));
                        if k.clicked() {
                            review = Some(pr.number);
                        }
                        let k = ui.add(Key::new("merge").solid(READOUT).id_salt(("merge", pr.number)));
                        k.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("merge pull request #{}", pr.number)));
                        if k.clicked() {
                            merge = Some((pr.number, pr.title.clone()));
                        }
                        if !pr.url.is_empty() {
                            link_key(ui, "diff on github", &format!("{}/files", pr.url.trim_end_matches('/')));
                        }
                    });
                },
            );
            r.response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Group, true, format!("pull request #{}", pr.number)));
            ui.add_space(6.0);
        }
        hint(ui, "Merging asks GitHub to merge with the project's method. It needs green checks and a review row you approved in the outbox; branch protection is never bypassed.");
        if let Some(n) = review
            && let Some(c) = &mut self.client
        {
            c.pull_review(pid, n);
        }
        if let Some((n, title)) = merge {
            self.confirm = Some(Confirm::Merge(pid, n, title));
        }
    }

    pub(super) fn github_issue_block(&mut self, ui: &mut Ui, issue: &Issue) {
        if issue.source != IssueSource::Github {
            return;
        }
        let pid = issue.project_id;
        let n = issue.external_number.unwrap_or_default();
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            chip(ui, &format!("github #{n}"), VALUE, true);
            trust_chip(ui, issue.trust);
            if let Some(by) = &issue.external_author {
                Line::new().note(format!("by {by}")).size(10.5).show(ui);
            }
            if let Some(url) = &issue.external_url {
                link_key(ui, "open on github", url);
            }
        });
        if issue.trust.is_untrusted() {
            hint(ui, "Written by someone outside the project. Agents read it as untrusted text and need a trusted ready before working on it.");
        }
        if !self.server_has(pid, API_PR_REVIEW) {
            return;
        }
        self.github_fetch(pid, false);
        let mine: Vec<OutboxItem> = self.outbox_items(pid).into_iter().filter(|i| i.number == n && i.kind != "issue_create").collect();
        if mine.is_empty() {
            return;
        }
        ui.add_space(4.0);
        Line::new().legend("proposals and drafts").size(10.0).show(ui);
        for item in mine.iter().rev() {
            self.outbox_card(ui, pid, item);
            ui.add_space(4.0);
        }
    }

    pub(super) fn github_section(&mut self, ui: &mut Ui, p: &Project) {
        let id = p.id;
        if !self.server_has(id, API_GITHUB_FLOW) {
            return;
        }
        if p.tracker.is_github() {
            self.github_fetch(id, true);
        }
        let resolved = agentbench_core::repo_from_remote(p.remote.as_deref(), None);
        let phone = ui.available_width() < 560.0;
        let mut patch = ProjectPatch::default();
        let mut settings = GithubSettings::default();
        let (mut secret, mut create, mut sync) = (false, false, false);
        let note_text = if p.tracker.is_github() { "issues and pull requests come from GitHub" } else { "issues live in agentbench" };
        section(ui, "github", note_text, |ui| {
            let mut tracker = p.tracker.is_github();
            row_help(ui, "tracker", "GitHub makes the repo's issues and pull requests the main work list. Internal issues stay, in their own group, and switching back loses nothing.", |ui| {
                ui.allocate_ui(Vec2::new(if phone { ui.available_width() } else { 200.0 }, KEY_H), |ui| {
                    if choice(ui, "tracker", &mut tracker, [(false, "internal".to_string()), (true, "github".to_string())]) {
                        patch.tracker = Some(if tracker { Tracker::Github { repo: String::new() } } else { Tracker::Internal });
                    }
                });
            });
            if !p.tracker.is_github() {
                if let Some(r) = &resolved {
                    hint(ui, &format!("the remote points at {r}"));
                }
                return;
            }
            let v = self.github.entry(id).or_default();
            let own = match &p.tracker {
                Tracker::Github { repo } => repo.clone(),
                Tracker::Internal => String::new(),
            };
            let repo = v.repo.get_or_insert_with(|| own.clone());
            row_help(ui, "repo", "owner/name. Leave empty to use the one in the project's remote.", |ui| {
                let hint_text = resolved.clone().map_or("owner/name".to_string(), |r| format!("{r} (from the remote)"));
                let r = field(ui, repo, &hint_text);
                r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "github repo"));
                if r.lost_focus() && repo.trim() != own {
                    patch.tracker = Some(Tracker::Github { repo: repo.trim().to_string() });
                }
            });
            let mut post = p.post_mode;
            row_help(ui, "post mode", "draft: every post waits in the outbox for you. auto: posts from trusted input go at once, merges and approvals still wait. off: nothing leaves this machine.", |ui| {
                ui.allocate_ui(Vec2::new(if phone { ui.available_width() } else { 200.0 }, KEY_H), |ui| {
                    if choice(ui, "post-mode", &mut post, [PostMode::Draft, PostMode::Auto, PostMode::Off].map(|m| (m, m.as_str().to_string()))) {
                        patch.post_mode = Some(post);
                    }
                });
            });
            let gh = p.tracker.is_github();
            if gh && self.server_has(id, API_GITHUB_WORK) {
                let mut mode = p.merge_mode;
                row_help(ui, "merge", "pr: finished work becomes a pull request. direct: it is merged into the default branch.", |ui| {
                    ui.allocate_ui(Vec2::new(if phone { ui.available_width() } else { 200.0 }, KEY_H), |ui| {
                        if choice(ui, "merge-mode", &mut mode, [MergeMode::Pr, MergeMode::Direct].map(|m| (m, m.as_str().to_string()))) {
                            patch.merge_mode = Some(mode);
                        }
                    });
                });
            }
            if gh && self.server_has(id, API_PR_REVIEW) {
                let mut auto = p.automerge;
                row_help(ui, "automerge", "trusted: agentbench merges its own pull requests for ready issues once checks pass and the review approves.", |ui| {
                    ui.allocate_ui(Vec2::new(if phone { ui.available_width() } else { 200.0 }, KEY_H), |ui| {
                        if choice(ui, "automerge", &mut auto, [AutoMerge::Off, AutoMerge::Trusted].map(|m| (m, m.as_str().to_string()))) {
                            patch.automerge = Some(auto);
                        }
                    });
                });
                let mut method = p.merge_method;
                row(ui, "merge method", |ui| {
                    ui.allocate_ui(Vec2::new(if phone { ui.available_width() } else { 200.0 }, KEY_H), |ui| {
                        if choice(ui, "merge-method", &mut method, [MergeMethod::Merge, MergeMethod::Squash, MergeMethod::Rebase].map(|m| (m, m.as_str().to_string()))) {
                            patch.merge_method = Some(method);
                        }
                    });
                });
            }
            let app_api = self.server_has(id, API_GITHUB_APP);
            let v = self.github.entry(id).or_default();
            if app_api {
                let saved_poll = v.settings.as_ref().and_then(|s| s.poll_secs).or(v.status.as_ref().and_then(|s| s.poll_secs)).unwrap_or(60);
                let poll = v.poll.get_or_insert_with(|| saved_poll.to_string());
                row_help(ui, "poll every", "Seconds between syncs, for every GitHub project. While webhooks arrive it slows to five minutes.", |ui| {
                    let r = field(ui, poll, "60");
                    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "poll seconds"));
                    if r.lost_focus()
                        && let Ok(n) = poll.trim().parse::<i64>()
                        && n != saved_poll
                    {
                        settings.poll_secs = Some(n.max(5));
                    }
                });
                let saved_base = v.settings.as_ref().and_then(|s| s.webhook_base.clone()).unwrap_or_default();
                let base = v.base.get_or_insert_with(|| saved_base.clone());
                row_help(ui, "public address", "Where GitHub reaches this server, through your reverse proxy, such as https://bench.example.com. Only the webhook path needs to be open.", |ui| {
                    let r = field(ui, base, "https://bench.example.com");
                    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "public address"));
                    if r.lost_focus() && base.trim() != saved_base {
                        settings.webhook_base = Some(base.trim().to_string());
                    }
                });
                let status = v.status.clone().unwrap_or_default();
                let url = status.webhook_url.clone().or_else(|| v.secret.as_ref().and_then(|s| s.url.clone()));
                Line::new().legend("webhook").size(10.0).show(ui);
                match &url {
                    Some(u) => Line::new().value(u).size(11.0).wrapped(ui),
                    None => Line::new().note("set the public address first").size(10.5).wrapped(ui),
                };
                if let Some(s) = &v.secret {
                    note(ui, "The secret is shown once. Copy it into the webhook on GitHub, or press create webhook.", WARN);
                    code_block(ui, &s.secret, READOUT, ("webhook-secret", id));
                }
                ui.horizontal_wrapped(|ui| {
                    let label = if status.webhook_secret_set { "new secret" } else { "make secret" };
                    secret = ui.add(Key::new(label).outline(READOUT)).clicked();
                    if let Some(s) = &v.secret
                        && ui.add(Key::new("copy secret").outline(VALUE)).clicked()
                    {
                        ui.ctx().copy_text(s.secret.clone());
                    }
                    let ready = url.is_some() && (status.webhook_secret_set || v.secret.is_some());
                    let k = ui.add(Key::new("create webhook").solid(READOUT).enabled(ready));
                    k.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ready, "create webhook"));
                    create = k.clicked();
                });
                match status.webhook_last {
                    Some(t) => status_line(ui, true, &format!("last delivery {}", ago(t))),
                    None if status.webhook_secret_set => status_line(ui, false, "no delivery yet, polling instead"),
                    None => {}
                }
            }
            ui.add_space(4.0);
            Line::new().legend("sync").size(10.0).show(ui);
            match &v.status {
                None => hint(ui, "asking the server"),
                Some(s) => {
                    match &s.problem {
                        Some(p) => status_line(ui, false, p),
                        None => status_line(ui, true, &format!("gh ready{}", s.repo.as_ref().map(|r| format!(" for {r}")).unwrap_or_default())),
                    }
                    if let Some(w) = s.write_access {
                        status_line(ui, w, if w { "you can write to the repo" } else { "read only access to the repo" });
                    }
                    if let Some(n) = s.rate_limit_remaining {
                        Line::new().legend("rate limit left").gap(8.0).measured(n.to_string()).size(11.0).show(ui);
                    }
                    match s.last_sync {
                        Some(t) => Line::new().legend("last sync").gap(8.0).measured(ago(t)).size(11.0).show(ui),
                        None => Line::new().legend("last sync").gap(8.0).note("never").size(11.0).show(ui),
                    };
                    if let Some(e) = &s.sync_error {
                        status_line(ui, false, e);
                    }
                    if s.paused {
                        status_line(ui, false, "posting is paused");
                    }
                }
            }
            let busy = v.syncing;
            let k = ui.add(Key::new(if busy { "syncing" } else { "sync now" }).outline(VALUE).enabled(!busy));
            sync = k.clicked();
        });
        if patch != ProjectPatch::default() {
            if let Some(Tracker::Github { repo }) = &patch.tracker
                && let Some(v) = self.github.get_mut(&id)
            {
                v.repo = Some(repo.clone());
            }
            if let Some(t) = &patch.tracker
                && let Some(x) = self.overview.as_mut().and_then(|o| o.projects.iter_mut().find(|x| x.id == id))
            {
                x.tracker = t.clone();
            }
            if let Some(x) = self.overview.as_mut().and_then(|o| o.projects.iter_mut().find(|x| x.id == id)) {
                if let Some(m) = patch.post_mode {
                    x.post_mode = m;
                }
                if let Some(m) = patch.merge_mode {
                    x.merge_mode = m;
                }
                if let Some(m) = patch.automerge {
                    x.automerge = m;
                }
                if let Some(m) = patch.merge_method {
                    x.merge_method = m;
                }
            }
            self.patch_project(id, patch);
            if let Some(v) = self.github.get_mut(&id) {
                v.status_asked = None;
            }
        }
        let Some(c) = &mut self.client else { return };
        if settings != GithubSettings::default() {
            c.set_github_settings(id, &settings);
            if let Some(v) = self.github.get_mut(&id) {
                v.status_asked = None;
            }
        }
        if secret {
            c.webhook_secret(id);
        }
        if create {
            c.webhook_create(id);
        }
        if sync {
            c.github_sync(id);
            self.github.entry(id).or_default().syncing = true;
        }
    }
}

fn status_line(ui: &mut Ui, ok: bool, text: &str) {
    status(ui, ok, text);
}
