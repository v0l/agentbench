use serde_json::{Map, Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const KEEP: usize = 2000;

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub id: String,
    pub status: String,
    pub title: String,
    pub project: String,
    pub managed: bool,
}

impl Row {
    fn full(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("kind".into(), json!("attention"));
        m.insert("id".into(), json!(self.id));
        m.insert("status".into(), json!(self.status));
        m.insert("title".into(), json!(self.title));
        m.insert("project".into(), json!(self.project));
        m.insert("managed".into(), json!(self.managed));
        m
    }

    fn diff(&self, old: &Row) -> Option<Map<String, Value>> {
        if self == old {
            return None;
        }
        let mut m = Map::new();
        m.insert("kind".into(), json!("attention"));
        m.insert("id".into(), json!(self.id));
        if self.status != old.status {
            m.insert("status".into(), json!(self.status));
        }
        if self.title != old.title {
            m.insert("title".into(), json!(self.title));
        }
        if self.project != old.project {
            m.insert("project".into(), json!(self.project));
        }
        if self.managed != old.managed {
            m.insert("managed".into(), json!(self.managed));
        }
        Some(m)
    }
}

pub struct Feed {
    pub epoch: u64,
    pub cursor: u64,
    pub reset: bool,
    pub changes: Vec<Value>,
}

impl Feed {
    pub fn wakes(&self) -> bool {
        self.reset || !self.changes.is_empty()
    }

    pub fn json(&self) -> Value {
        json!({
            "epoch": self.epoch,
            "cursor": self.cursor,
            "reset": self.reset,
            "changes": self.changes,
        })
    }
}

struct Inner {
    seq: u64,
    last: HashMap<String, Row>,
    log: VecDeque<(u64, Map<String, Value>)>,
}

pub struct Attention {
    epoch: u64,
    inner: Mutex<Inner>,
}

impl Default for Attention {
    fn default() -> Self {
        Attention::new()
    }
}

impl Attention {
    pub fn new() -> Attention {
        let epoch =
            SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(1);
        Attention {
            epoch,
            inner: Mutex::new(Inner { seq: 0, last: HashMap::new(), log: VecDeque::new() }),
        }
    }

    pub fn refresh(&self, rows: Vec<Row>) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut now: HashMap<String, Row> = HashMap::with_capacity(rows.len());
        let mut deltas = Vec::new();
        for row in rows {
            match g.last.get(&row.id) {
                None => deltas.push(row.full()),
                Some(old) => deltas.extend(row.diff(old)),
            }
            now.insert(row.id.clone(), row);
        }
        for id in g.last.keys().filter(|id| !now.contains_key(*id)) {
            let mut m = Map::new();
            m.insert("kind".into(), json!("attention"));
            m.insert("id".into(), json!(id));
            m.insert("removed".into(), json!(true));
            deltas.push(m);
        }
        for d in deltas {
            g.seq += 1;
            let seq = g.seq;
            g.log.push_back((seq, d));
        }
        while g.log.len() > KEEP {
            g.log.pop_front();
        }
        g.last = now;
    }

    pub fn feed(&self, since: u64, epoch: Option<u64>) -> Feed {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let oldest = g.log.front().map(|(s, _)| *s).unwrap_or(g.seq + 1);
        let stale = epoch != Some(self.epoch) || since > g.seq || (since < g.seq && oldest > since + 1);
        if stale {
            let mut rows: Vec<&Row> = g.last.values().collect();
            rows.sort_by(|a, b| a.id.cmp(&b.id));
            return Feed {
                epoch: self.epoch,
                cursor: g.seq,
                reset: true,
                changes: rows.into_iter().map(|r| Value::Object(r.full())).collect(),
            };
        }
        let mut order: Vec<String> = Vec::new();
        let mut merged: HashMap<String, Map<String, Value>> = HashMap::new();
        for (_, d) in g.log.iter().filter(|(s, _)| *s > since) {
            let id = d["id"].as_str().unwrap_or_default().to_string();
            let removed = d.contains_key("removed");
            match merged.get_mut(&id) {
                None => {
                    order.push(id.clone());
                    merged.insert(id, d.clone());
                }
                Some(m) if removed || m.contains_key("removed") => *m = d.clone(),
                Some(m) => m.extend(d.clone()),
            }
        }
        Feed {
            epoch: self.epoch,
            cursor: g.seq,
            reset: false,
            changes: order.into_iter().filter_map(|id| merged.remove(&id)).map(Value::Object).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, status: &str) -> Row {
        Row { id: id.into(), status: status.into(), title: format!("chat {id}"), project: "demo".into(), managed: false }
    }

    #[test]
    fn a_delta_carries_only_the_fields_that_changed() {
        let a = Attention::new();
        a.refresh(vec![row("a", "running"), row("b", "idle")]);
        let first = a.feed(0, None);
        assert!(first.reset);
        assert_eq!(first.changes.len(), 2);
        a.refresh(vec![row("a", "waiting"), row("b", "idle")]);
        let f = a.feed(first.cursor, Some(first.epoch));
        assert!(!f.reset);
        assert_eq!(f.changes, vec![json!({"kind": "attention", "id": "a", "status": "waiting"})]);
    }

    #[test]
    fn the_full_row_and_the_diff_carry_managed() {
        let a = Attention::new();
        a.refresh(vec![row("a", "running")]);
        let first = a.feed(0, None);
        assert_eq!(first.changes[0]["managed"], json!(false));
        let mut managed = row("a", "running");
        managed.managed = true;
        a.refresh(vec![managed.clone()]);
        let f = a.feed(first.cursor, Some(first.epoch));
        assert_eq!(f.changes, vec![json!({"kind": "attention", "id": "a", "managed": true})]);
        let cursor = f.cursor;
        a.refresh(vec![row("a", "running")]);
        let f = a.feed(cursor, Some(first.epoch));
        assert_eq!(f.changes, vec![json!({"kind": "attention", "id": "a", "managed": false})]);
    }

    #[test]
    fn several_flips_inside_a_poll_send_only_the_latest() {
        let a = Attention::new();
        a.refresh(vec![row("a", "idle")]);
        let first = a.feed(0, None);
        a.refresh(vec![row("a", "running")]);
        a.refresh(vec![row("a", "waiting")]);
        let mut renamed = row("a", "waiting");
        renamed.title = "new".into();
        a.refresh(vec![renamed]);
        let f = a.feed(first.cursor, Some(first.epoch));
        assert_eq!(
            f.changes,
            vec![json!({"kind": "attention", "id": "a", "status": "waiting", "title": "new"})]
        );
    }

    #[test]
    fn a_vanished_session_is_a_removal() {
        let a = Attention::new();
        a.refresh(vec![row("a", "idle"), row("b", "idle")]);
        let first = a.feed(0, None);
        a.refresh(vec![row("b", "idle")]);
        let f = a.feed(first.cursor, Some(first.epoch));
        assert_eq!(f.changes, vec![json!({"kind": "attention", "id": "a", "removed": true})]);
    }

    #[test]
    fn a_stale_cursor_or_other_epoch_gets_one_reset_then_deltas() {
        let a = Attention::new();
        a.refresh(vec![row("a", "idle")]);
        let wrong_epoch = a.feed(1, Some(7));
        assert!(wrong_epoch.reset);
        let future = a.feed(99, Some(wrong_epoch.epoch));
        assert!(future.reset);
        assert_eq!(future.changes.len(), 1);
        a.refresh(vec![row("a", "running")]);
        let next = a.feed(future.cursor, Some(future.epoch));
        assert!(!next.reset);
        assert_eq!(next.changes.len(), 1);
        assert!(!a.feed(next.cursor, Some(next.epoch)).wakes());
    }

    #[test]
    fn a_log_that_outgrew_the_cursor_resets() {
        let a = Attention::new();
        a.refresh(vec![row("a", "idle")]);
        let first = a.feed(0, None);
        for i in 0..KEEP + 5 {
            a.refresh(vec![row("a", if i % 2 == 0 { "running" } else { "idle" })]);
        }
        assert!(a.feed(first.cursor, Some(first.epoch)).reset);
    }
}
