use crate::client::{Client, Reply, Server};
use agentbench_core::{
    Host, Id, IssuePatch, NewComment, NewIssue, NewProject, NewSession, NewWorktree, Overview,
    Place, ProjectPatch, SessionId,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use web_time::{Duration, Instant};

const SLOT_SHIFT: u32 = 40;
const RETRY: Duration = Duration::from_secs(5);

pub fn tag(slot: u32, id: Id) -> Id {
    if slot == 0 { id } else { (slot as i64) << SLOT_SHIFT | id }
}

pub fn slot_of(id: Id) -> u32 {
    (id >> SLOT_SHIFT) as u32
}

pub fn untag(id: Id) -> Id {
    id & ((1 << SLOT_SHIFT) - 1)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Remote {
    pub slot: u32,
    pub name: String,
    pub url: String,
    pub token: String,
    #[serde(default)]
    pub icon: String,
}

struct Link {
    slot: u32,
    name: String,
    client: Client,
    overview: Option<Overview>,
    cursor: Option<Id>,
    watching: bool,
    offline: Option<(String, Instant)>,
    compat: agentbench_core::Compat,
    api: u32,
}

impl Link {
    fn new(slot: u32, name: String, server: Server, wake: &Arc<dyn Fn() + Send + Sync>) -> Link {
        let wake = wake.clone();
        let mut client = Client::new(server, move || wake());
        client.overview();
        Link {
            slot,
            name,
            client,
            overview: None,
            cursor: None,
            watching: false,
            offline: None,
            compat: agentbench_core::Compat::Ok,
            api: 0,
        }
    }

    fn watch(&mut self) {
        if let (Some(cursor), false, None) = (self.cursor, self.watching, &self.offline) {
            self.watching = true;
            self.client.watch(cursor);
        }
    }
}

pub const MIN_SERVER_API: u32 = agentbench_core::API_ISSUE_DEPS;

pub struct Hub {
    links: Vec<Link>,
    sessions: HashMap<SessionId, u32>,
    dirty: bool,
    focus: Option<u32>,
    supervisor: u32,
    wake: Arc<dyn Fn() + Send + Sync>,
    min_server_api: u32,
}

impl std::ops::Deref for Hub {
    type Target = Client;
    fn deref(&self) -> &Client {
        let slot = self.focus.unwrap_or(0);
        &self.links.iter().find(|l| l.slot == slot).unwrap_or(&self.links[0]).client
    }
}

impl std::ops::DerefMut for Hub {
    fn deref_mut(&mut self) -> &mut Client {
        let at = self.focus.and_then(|f| self.links.iter().position(|l| l.slot == f)).unwrap_or(0);
        &mut self.links[at].client
    }
}

fn tag_proposal(p: &mut agentbench_core::Proposal, slot: u32) {
    use agentbench_core::Change;
    p.id = tag(slot, p.id);
    p.issues.iter_mut().for_each(|i| *i = tag(slot, *i));
    p.projects.iter_mut().for_each(|i| *i = tag(slot, *i));
    for c in &mut p.changes {
        match c {
            Change::Update { issue_id, .. } | Change::Comment { issue_id, .. } => {
                *issue_id = tag(slot, *issue_id)
            }
            Change::Create { issue } => issue.project_id = tag(slot, issue.project_id),
            Change::AddProject { .. } => {}
        }
    }
}

fn merge(links: &[Link], focus: Option<u32>, supervisor: u32) -> Option<Overview> {
    let wanted = focus.unwrap_or(supervisor);
    let base = links
        .iter()
        .find(|l| l.slot == wanted && l.overview.is_some())
        .or_else(|| links.iter().find(|l| l.overview.is_some()))?;
    let bs = base.slot;
    let mut out = base.overview.clone()?;
    out.projects.clear();
    out.worktrees.clear();
    out.issues.clear();
    out.sessions.clear();
    out.visions.clear();
    out.host = None;
    out.cursor = links.iter().filter_map(|l| l.cursor).sum();
    if let Some(t) = &mut out.triage {
        t.proposals.iter_mut().for_each(|p| tag_proposal(p, bs));
        t.notes.iter_mut().for_each(|(id, _)| *id = tag(bs, *id));
    }
    if let Some(u) = &mut out.usage {
        u.spend.projects.iter_mut().for_each(|p| p.project_id = tag(bs, p.project_id));
    }
    if let Some(a) = &mut out.autopilot {
        for x in &mut a.activity {
            x.project_id = x.project_id.map(|i| tag(bs, i));
            x.issue_id = x.issue_id.map(|i| tag(bs, i));
        }
    }
    out.host = base.overview.as_ref().and_then(|o| o.host.clone());
    let mut first = true;
    for l in links.iter().filter(|l| focus.is_none_or(|f| f == l.slot)) {
        let Some(o) = &l.overview else {
            continue;
        };
        let s = l.slot;
        out.projects.extend(o.projects.iter().filter(|p| !p.supervisor).cloned().map(|mut p| {
            p.id = tag(s, p.id);
            p
        }));
        out.worktrees.extend(o.worktrees.iter().cloned().map(|mut w| {
            w.id = tag(s, w.id);
            w.project_id = tag(s, w.project_id);
            w
        }));
        out.issues.extend(o.issues.iter().cloned().map(|mut i| {
            i.id = tag(s, i.id);
            i.project_id = tag(s, i.project_id);
            i.depends_on.iter_mut().for_each(|d| *d = tag(s, *d));
            i.waiting_on.iter_mut().for_each(|d| *d = tag(s, *d));
            i.blocks.iter_mut().for_each(|d| *d = tag(s, *d));
            i
        }));
        out.sessions.extend(
            o.sessions.iter().filter(|x| Some(x.id) != o.supervisor || s == bs).cloned().map(
                |mut x| {
                    retag_session(&mut x, s);
                    x
                },
            ),
        );
        out.visions.extend(o.visions.iter().cloned().map(|mut v| {
            v.id = tag(s, v.id);
            v.project_id = tag(s, v.project_id);
            v
        }));
        if !std::mem::take(&mut first) {
            for a in &o.agents {
                match out.agents.iter_mut().find(|x| x.name == a.name) {
                    Some(x) => x.available |= a.available,
                    None => out.agents.push(a.clone()),
                }
            }
        }
    }
    Some(out)
}

fn retag_session(s: &mut agentbench_core::Session, slot: u32) {
    s.project_id = tag(slot, s.project_id);
    s.worktree_id = tag(slot, s.worktree_id);
    s.issue_id = s.issue_id.map(|i| tag(slot, i));
}

impl Hub {
    pub fn new(
        primary_name: &str,
        primary: Server,
        extras: &[Remote],
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Hub {
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
        let mut links = vec![Link::new(0, primary_name.to_string(), primary, &wake)];
        links.extend(extras.iter().map(|e| {
            Link::new(
                e.slot,
                e.name.clone(),
                Server { url: e.url.clone(), token: e.token.clone() },
                &wake,
            )
        }));
        Hub {
            links,
            sessions: HashMap::new(),
            dirty: false,
            focus: None,
            supervisor: 0,
            wake,
            min_server_api: MIN_SERVER_API,
        }
    }

    pub fn sync(&mut self, primary_name: &str, extras: &[Remote]) {
        self.links.retain(|l| l.slot == 0 || extras.iter().any(|e| e.slot == l.slot));
        self.links[0].name = primary_name.to_string();
        for e in extras {
            let server = Server { url: e.url.clone(), token: e.token.clone() };
            match self.links.iter_mut().find(|l| l.slot == e.slot) {
                Some(l) if l.client.server == server => l.name = e.name.clone(),
                Some(l) => *l = Link::new(e.slot, e.name.clone(), server, &self.wake),
                None => self.links.push(Link::new(e.slot, e.name.clone(), server, &self.wake)),
            }
        }
        self.sessions.retain(|_, slot| self.links.iter().any(|l| l.slot == *slot));
        self.dirty = true;
    }

    pub fn set_focus(&mut self, focus: Option<u32>) {
        let focus = focus.filter(|f| self.links.iter().any(|l| l.slot == *f));
        if focus != self.focus {
            self.focus = focus;
            self.dirty = true;
        }
    }

    pub fn focus(&self) -> Option<u32> {
        self.focus
    }

    pub fn set_supervisor(&mut self, slot: u32) {
        if self.links.iter().any(|l| l.slot == slot) && slot != self.supervisor {
            self.supervisor = slot;
            self.dirty = true;
        }
    }

    pub fn supervisor_slot(&self) -> u32 {
        self.focus.unwrap_or(self.supervisor)
    }

    pub fn url(&self, slot: u32) -> String {
        self.links
            .iter()
            .find(|l| l.slot == slot)
            .map(|l| l.client.server.url.clone())
            .unwrap_or_default()
    }

    pub fn activity(&self, slot: u32) -> (usize, usize, usize) {
        let Some(o) = self.links.iter().find(|l| l.slot == slot).and_then(|l| l.overview.as_ref())
        else {
            return (0, 0, 0);
        };
        let running = o
            .sessions
            .iter()
            .filter(|s| s.status == agentbench_core::SessionStatus::Running)
            .count();
        let waiting = o
            .sessions
            .iter()
            .filter(|s| s.status == agentbench_core::SessionStatus::Waiting)
            .count();
        let pending = o.triage.as_ref().map_or(0, |t| t.proposals.len());
        (running, waiting, pending)
    }

    pub fn waiting(&self) -> Vec<(u32, SessionId, String, bool)> {
        let mut out = Vec::new();
        for l in &self.links {
            let Some(o) = &l.overview else {
                continue;
            };
            for s in
                o.sessions.iter().filter(|s| s.status == agentbench_core::SessionStatus::Waiting)
            {
                out.push((l.slot, s.id, s.title.clone(), Some(s.id) == o.supervisor));
            }
        }
        out
    }

    pub fn waiting_total(&self) -> usize {
        self.links.iter().map(|l| self.activity(l.slot).1).sum()
    }

    pub fn proposals(&self) -> Vec<(u32, agentbench_core::Proposal)> {
        let mut out = Vec::new();
        for l in &self.links {
            let Some(t) = l.overview.as_ref().and_then(|o| o.triage.as_ref()) else {
                continue;
            };
            for p in &t.proposals {
                let mut p = p.clone();
                tag_proposal(&mut p, l.slot);
                out.push((l.slot, p));
            }
        }
        out
    }

    pub fn multi(&self) -> bool {
        self.links.len() > 1
    }

    pub fn name(&self, slot: u32) -> &str {
        self.links.iter().find(|l| l.slot == slot).map(|l| l.name.as_str()).unwrap_or("")
    }

    pub fn offline(&self, slot: u32) -> Option<&str> {
        let l = self.links.iter().find(|l| l.slot == slot)?;
        l.offline.as_ref().map(|(e, _)| e.as_str())
    }

    pub fn compat(&self, slot: u32) -> agentbench_core::Compat {
        self.links.iter().find(|l| l.slot == slot).map_or(agentbench_core::Compat::Ok, |l| l.compat)
    }

    pub fn server_has(&self, slot: u32, needs: u32) -> bool {
        self.links
            .iter()
            .find(|l| l.slot == slot)
            .is_none_or(|l| l.overview.is_none() || agentbench_core::server_has(l.api, needs))
    }

    pub fn compat_notices(&self) -> Vec<(u32, String)> {
        self.links.iter().filter_map(|l| Some((l.slot, l.compat.notice()?))).collect()
    }

    pub fn slots(&self) -> Vec<u32> {
        self.links.iter().map(|l| l.slot).collect()
    }

    pub fn host(&self, slot: u32) -> Option<&Host> {
        self.links.iter().find(|l| l.slot == slot)?.overview.as_ref()?.host.as_ref()
    }

    pub fn session_slot(&self, id: SessionId) -> u32 {
        self.sessions.get(&id).copied().unwrap_or(0)
    }

    fn link(&mut self, slot: u32) -> &mut Link {
        let at = self.links.iter().position(|l| l.slot == slot).unwrap_or(0);
        &mut self.links[at]
    }

    fn by_id(&mut self, id: Id) -> (&mut Client, Id) {
        let l = self.link(slot_of(id));
        (&mut l.client, untag(id))
    }

    fn by_session(&mut self, id: SessionId) -> &mut Client {
        let slot = self.session_slot(id);
        &mut self.link(slot).client
    }

    pub fn reset(&mut self) {
        for l in &mut self.links {
            l.client.reset();
            l.watching = false;
        }
    }

    pub fn poll(&mut self) -> Vec<Reply> {
        let mut out = Vec::new();
        let mut offline = Vec::new();
        for i in 0..self.links.len() {
            let slot = self.links[i].slot;
            for r in self.links[i].client.poll() {
                match r {
                    Reply::Overview(o) => {
                        let l = &mut self.links[i];
                        l.cursor = Some(o.cursor);
                        l.offline = None;
                        l.api = o.api_version;
                        l.compat = agentbench_core::compat(
                            agentbench_core::API_VERSION,
                            self.min_server_api,
                            o.api_version,
                            o.min_client_api,
                        );
                        for s in &o.sessions {
                            self.sessions.insert(s.id, slot);
                        }
                        l.overview = Some(o);
                        l.watch();
                        self.dirty = true;
                    }
                    Reply::Changed(c) => {
                        let l = &mut self.links[i];
                        l.watching = false;
                        if Some(c) == l.cursor {
                            l.watch();
                        } else {
                            let merged: Id = self
                                .links
                                .iter()
                                .map(|x| if x.slot == slot { c } else { x.cursor.unwrap_or(0) })
                                .sum();
                            out.push(Reply::Changed(merged));
                        }
                    }
                    Reply::Offline(e) => {
                        let l = &mut self.links[i];
                        l.watching = false;
                        l.offline = Some((e.clone(), Instant::now()));
                        offline.push(e);
                        self.dirty = true;
                    }
                    Reply::Started(mut s) => {
                        self.sessions.insert(s.id, slot);
                        retag_session(&mut s, slot);
                        out.push(Reply::Started(s));
                    }
                    Reply::Supervisor(mut s) => {
                        self.sessions.insert(s.id, slot);
                        retag_session(&mut s, slot);
                        out.push(Reply::Supervisor(s));
                    }
                    Reply::Comments(id, mut list) => {
                        for c in &mut list {
                            c.issue_id = tag(slot, c.issue_id);
                        }
                        out.push(Reply::Comments(tag(slot, id), list));
                    }
                    Reply::Notes(id, n) => out.push(Reply::Notes(tag(slot, id), n)),
                    Reply::VisionVersions(id, mut list) => {
                        for v in &mut list {
                            v.id = tag(slot, v.id);
                            v.project_id = tag(slot, v.project_id);
                        }
                        out.push(Reply::VisionVersions(tag(slot, id), list));
                    }
                    other => out.push(other),
                }
            }
        }
        let everyone_down = self.links.iter().all(|l| l.offline.is_some());
        if let Some(why) = offline.first().filter(|_| everyone_down) {
            out.push(Reply::Offline(why.clone()));
        } else if !everyone_down {
            for l in &mut self.links {
                if l.offline.as_ref().is_some_and(|(_, at)| at.elapsed() >= RETRY) {
                    l.offline = l.offline.take().map(|(e, _)| (e, Instant::now()));
                    l.client.overview();
                }
            }
        }
        if std::mem::take(&mut self.dirty)
            && !everyone_down
            && let Some(o) = merge(&self.links, self.focus, self.supervisor)
        {
            out.push(Reply::Overview(o));
        }
        out
    }

    pub fn overview(&mut self) {
        for l in &mut self.links {
            l.client.overview();
        }
    }

    pub fn watch(&mut self, _since: Id) {
        for l in &mut self.links {
            l.watch();
        }
    }

    pub fn messages(&mut self, session: SessionId, after: Id, limit: usize) {
        self.by_session(session).messages(session, after, limit);
    }

    pub fn older_messages(&mut self, session: SessionId, before: Id, limit: usize) {
        self.by_session(session).older_messages(session, before, limit);
    }

    pub fn autopilot(&mut self, patch: &agentbench_core::AutopilotPatch) {
        let slot = self.supervisor_slot();
        self.link(slot).client.autopilot(patch);
    }

    pub fn triage_run(&mut self) {
        let slot = self.supervisor_slot();
        self.link(slot).client.triage_run();
    }

    pub fn triage_patch(&mut self, patch: &agentbench_core::TriagePatch) {
        let slot = self.supervisor_slot();
        let mut patch = patch.clone();
        patch.note_project = patch.note_project.map(untag);
        self.link(slot).client.triage_patch(&patch);
    }

    pub fn triage_decide(&mut self, id: Id, accept: bool) {
        let (c, id) = self.by_id(id);
        c.triage_decide(id, accept);
    }

    pub fn open_supervisor(&mut self, agent: &str) {
        let slot = self.supervisor_slot();
        self.link(slot).client.open_supervisor(agent);
    }

    pub fn comments(&mut self, issue: Id) {
        let (c, id) = self.by_id(issue);
        c.comments(id);
    }

    pub fn add_project_on(&mut self, slot: u32, new: &NewProject) {
        self.link(slot).client.add_project(new);
    }

    pub fn remove_project(&mut self, id: Id) {
        let (c, id) = self.by_id(id);
        c.remove_project(id);
    }

    pub fn update_project(&mut self, id: Id, patch: &ProjectPatch) {
        let (c, id) = self.by_id(id);
        c.update_project(id, patch);
    }

    pub fn notes(&mut self, project: Id) {
        let (c, id) = self.by_id(project);
        c.notes(id);
    }

    pub fn restore_note(&mut self, project: Id, version: Id) {
        let (c, id) = self.by_id(project);
        c.restore_note(id, untag(version));
    }

    pub fn vision_versions(&mut self, project: Id) {
        let (c, id) = self.by_id(project);
        c.vision_versions(id);
    }

    pub fn put_vision(&mut self, project: Id, body: &agentbench_core::VisionBody) {
        let (c, id) = self.by_id(project);
        c.put_vision(id, body);
    }

    pub fn restore_vision(&mut self, project: Id, version: Id) {
        let (c, id) = self.by_id(project);
        c.restore_vision(id, untag(version));
    }

    pub fn sync_project(&mut self, id: Id) {
        let (c, id) = self.by_id(id);
        c.sync_project(id);
    }

    pub fn add_worktree(&mut self, project: Id, new: &NewWorktree) {
        let (c, id) = self.by_id(project);
        c.add_worktree(id, new);
    }

    pub fn remove_worktree(&mut self, id: Id, force: bool) {
        let (c, id) = self.by_id(id);
        c.remove_worktree(id, force);
    }

    pub fn add_issue(&mut self, new: &NewIssue) {
        let (c, project_id) = self.by_id(new.project_id);
        c.add_issue(&NewIssue { project_id, ..new.clone() });
    }

    pub fn update_issue(&mut self, id: Id, patch: &IssuePatch) {
        let (c, id) = self.by_id(id);
        let mut patch = patch.clone();
        if let Some(d) = &mut patch.depends_on {
            d.iter_mut().for_each(|i| *i = untag(*i));
        }
        c.update_issue(id, &patch);
    }

    pub fn delete_issue(&mut self, id: Id) {
        let (c, id) = self.by_id(id);
        c.delete_issue(id);
    }

    pub fn add_comment(&mut self, issue: Id, new: &NewComment) {
        let (c, id) = self.by_id(issue);
        c.add_comment(id, new);
    }

    fn untagged(new: &NewSession) -> NewSession {
        NewSession {
            project_id: untag(new.project_id),
            issue_id: new.issue_id.map(untag),
            place: match &new.place {
                Place::Existing { worktree_id } => {
                    Place::Existing { worktree_id: untag(*worktree_id) }
                }
                other => other.clone(),
            },
            ..new.clone()
        }
    }

    pub fn start_issue(&mut self, issue: Id, new: &NewSession) {
        let (c, id) = self.by_id(issue);
        c.start_issue(id, &Hub::untagged(new));
    }

    pub fn add_session(&mut self, new: &NewSession) {
        let (c, _) = self.by_id(new.project_id);
        c.add_session(&Hub::untagged(new));
    }

    pub fn send_message(&mut self, session: SessionId, text: &str) {
        self.by_session(session).send_message(session, text);
    }

    pub fn send_with(
        &mut self,
        session: SessionId,
        text: &str,
        attachments: Vec<agentbench_core::Upload>,
    ) {
        self.by_session(session).send_with(session, text, attachments);
    }

    pub fn answer(&mut self, session: SessionId, question: &str, answers: &[String]) {
        self.by_session(session).answer(session, question, answers);
    }

    pub fn attachment(
        &self,
        session: SessionId,
        id: Id,
        done: impl FnOnce(Option<Vec<u8>>) + Send + 'static,
    ) {
        let slot = self.session_slot(session);
        let l = self.links.iter().find(|l| l.slot == slot).unwrap_or(&self.links[0]);
        l.client.attachment(session, id, done);
    }

    pub fn project_icon(&self, project: Id, done: impl FnOnce(Option<Vec<u8>>) + Send + 'static) {
        let l = self.links.iter().find(|l| l.slot == slot_of(project)).unwrap_or(&self.links[0]);
        l.client.project_icon(untag(project), done);
    }

    pub fn interrupt(&mut self, session: SessionId) {
        self.by_session(session).interrupt(session);
    }

    pub fn jobs(&mut self, session: SessionId) {
        self.by_session(session).jobs(session);
    }

    pub fn terminal(&mut self, session: SessionId) {
        self.by_session(session).terminal(session);
    }

    pub fn terminal_send(&mut self, session: SessionId, data: &str, rows: u16, cols: u16) {
        self.by_session(session).terminal_send(session, data, rows, cols);
    }

    pub fn terminal_close(&mut self, session: SessionId) {
        self.by_session(session).terminal_close(session);
    }

    pub fn kill_job(&mut self, session: SessionId, handle: &str) {
        self.by_session(session).kill_job(session, handle);
    }

    pub fn rename_session(&mut self, session: SessionId, title: &str) {
        self.by_session(session).rename_session(session, title);
    }

    pub fn release_session(&mut self, session: SessionId) {
        self.by_session(session).release_session(session);
    }

    pub fn adopt_session(&mut self, session: SessionId) {
        self.by_session(session).adopt_session(session);
    }

    pub fn delete_session(&mut self, session: SessionId) {
        self.by_session(session).delete_session(session);
    }
}

pub fn host_name(url: &str) -> String {
    let host = agentbench_core::host_of(url);
    if host.is_empty() { "workspace".to_string() } else { host }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tagged_id_names_its_server_and_the_primary_keeps_plain_ids() {
        assert_eq!(tag(0, 7), 7);
        let t = tag(3, 7);
        assert_eq!((slot_of(t), untag(t)), (3, 7));
        assert_eq!(slot_of(7), 0);
    }
}
