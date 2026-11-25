use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_MAX: usize = 2;
pub const DEFAULT_IDLE: Duration = Duration::from_secs(600);
pub const DEFAULT_MEMORY_MB: u64 = 6144;
pub const QUEUE_WAIT: Duration = Duration::from_secs(60);
pub const RECENT: Duration = Duration::from_secs(60);
const GB: f64 = 1024.0 * 1024.0 * 1024.0;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub max: usize,
    pub idle: Duration,
    pub memory_mb: Option<u64>,
    pub queue_wait: Duration,
    pub poll: Duration,
    pub scope: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            max: DEFAULT_MAX,
            idle: DEFAULT_IDLE,
            memory_mb: Some(DEFAULT_MEMORY_MB),
            queue_wait: QUEUE_WAIT,
            poll: Duration::from_secs(2),
            scope: true,
        }
    }
}

pub type Stopper = Arc<dyn Fn(Duration, &str) -> bool + Send + Sync>;

struct Entry {
    id: u64,
    holder: String,
    name: String,
    root: PathBuf,
    pid: Option<u32>,
    resident: u64,
    stopper: Option<Stopper>,
}

#[derive(Default)]
struct State {
    next: u64,
    running: Vec<Entry>,
    queue: VecDeque<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Held {
    pub holder: String,
    pub name: String,
    pub root: PathBuf,
    pub pid: Option<u32>,
    pub resident: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Freed {
    pub servers: usize,
    pub bytes: u64,
}

impl Freed {
    pub fn gb(&self) -> f64 {
        self.bytes as f64 / GB
    }
}

pub struct Pool {
    config: Mutex<Config>,
    state: Mutex<State>,
    changed: Notify,
    skew: Mutex<Duration>,
    reaper: AtomicBool,
}

pub struct Slot {
    pool: Arc<Pool>,
    id: u64,
}

impl Slot {
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let gone = {
            let mut s = lock(&self.pool.state);
            let at = s.running.iter().position(|e| e.id == self.id);
            at.map(|i| s.running.remove(i))
        };
        if let Some(e) = gone {
            tracing::info!(
                server = %e.name,
                root = %e.root.display(),
                holder = %e.holder,
                pid = ?e.pid,
                resident_mb = e.resident >> 20,
                "language server stopped"
            );
        }
        self.pool.changed.notify_waiters();
    }
}

impl Pool {
    pub fn new(config: Config) -> Arc<Pool> {
        Arc::new(Pool {
            config: Mutex::new(config),
            state: Mutex::default(),
            changed: Notify::new(),
            skew: Mutex::default(),
            reaper: AtomicBool::new(false),
        })
    }

    pub fn global() -> Arc<Pool> {
        static POOL: OnceLock<Arc<Pool>> = OnceLock::new();
        POOL.get_or_init(|| Pool::new(Config::default())).clone()
    }

    pub fn config(&self) -> Config {
        lock(&self.config).clone()
    }

    pub fn configure(&self, config: Config) {
        let changed = {
            let mut c = lock(&self.config);
            let changed = *c != config;
            *c = config;
            changed
        };
        if changed {
            self.changed.notify_waiters();
        }
    }

    pub fn now(&self) -> Instant {
        Instant::now() + *lock(&self.skew)
    }

    pub fn advance(&self, by: Duration) {
        *lock(&self.skew) += by;
    }

    pub fn held(&self) -> Vec<Held> {
        lock(&self.state)
            .running
            .iter()
            .map(|e| Held {
                holder: e.holder.clone(),
                name: e.name.clone(),
                root: e.root.clone(),
                pid: e.pid,
                resident: e.resident,
            })
            .collect()
    }

    pub fn total(&self) -> Freed {
        let s = lock(&self.state);
        Freed { servers: s.running.len(), bytes: s.running.iter().map(|e| e.resident).sum() }
    }

    pub fn attach(&self, id: u64, pid: Option<u32>, stopper: Stopper) {
        if let Some(e) = lock(&self.state).running.iter_mut().find(|e| e.id == id) {
            e.pid = pid;
            e.stopper = Some(stopper);
        }
    }

    pub fn set_resident(&self, id: u64, bytes: u64) {
        if let Some(e) = lock(&self.state).running.iter_mut().find(|e| e.id == id) {
            e.resident = bytes;
        }
    }

    pub async fn acquire(
        self: &Arc<Self>,
        holder: &str,
        name: &str,
        root: &Path,
        stop: &CancellationToken,
    ) -> Result<Slot, String> {
        self.start_reaper();
        let wait = self.config().queue_wait;
        let deadline = tokio::time::Instant::now() + wait;
        let ticket = {
            let mut s = lock(&self.state);
            s.next += 1;
            let t = s.next;
            s.queue.push_back(t);
            t
        };
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let max = self.config().max.max(1);
                let mut s = lock(&self.state);
                if s.queue.front() == Some(&ticket) && s.running.len() < max {
                    s.queue.pop_front();
                    s.running.push(Entry {
                        id: ticket,
                        holder: holder.to_string(),
                        name: name.to_string(),
                        root: root.to_path_buf(),
                        pid: None,
                        resident: 0,
                        stopper: None,
                    });
                    drop(s);
                    self.changed.notify_waiters();
                    return Ok(Slot { pool: self.clone(), id: ticket });
                }
            }
            let gave_up = tokio::select! {
                _ = &mut notified => None,
                _ = tokio::time::sleep_until(deadline) => Some(self.busy(wait)),
                _ = stop.cancelled() => Some("stopped".to_string()),
            };
            if let Some(why) = gave_up {
                lock(&self.state).queue.retain(|t| *t != ticket);
                self.changed.notify_waiters();
                return Err(why);
            }
        }
    }

    fn busy(&self, waited: Duration) -> String {
        let max = self.config().max.max(1);
        let held: Vec<String> = self
            .held()
            .iter()
            .map(|h| {
                let size = if h.resident > 0 {
                    format!(", {:.1} GB", h.resident as f64 / GB)
                } else {
                    String::new()
                };
                format!("{} ({} in {}{size})", h.holder, h.name, h.root.display())
            })
            .collect();
        format!(
            "the host is busy: {} language servers are running, the most allowed (lsp_max_servers {max}), and none came free in {} s. Held by {}. Use search_files or grep instead for now",
            held.len(),
            waited.as_secs(),
            if held.is_empty() { "nobody".to_string() } else { held.join(", ") }
        )
    }

    pub fn stop_idle(&self, unused_for: Duration, why: &str) -> Freed {
        let candidates: Vec<(Stopper, u64)> = lock(&self.state)
            .running
            .iter()
            .filter_map(|e| e.stopper.clone().map(|s| (s, e.resident)))
            .collect();
        let mut freed = Freed::default();
        for (stopper, resident) in candidates {
            if stopper(unused_for, why) {
                freed.servers += 1;
                freed.bytes += resident;
            }
        }
        freed
    }

    pub fn reap(&self) -> Freed {
        let idle = self.config().idle;
        self.stop_idle(idle, &format!("stopped after {} min without a call", idle.as_secs().div_ceil(60)))
    }

    pub fn stop_idle_language_servers(&self) -> Freed {
        self.stop_idle(RECENT, "stopped to free memory while the host was under pressure")
    }

    fn start_reaper(self: &Arc<Self>) {
        if self.reaper.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak: Weak<Pool> = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let every = match weak.upgrade() {
                    Some(p) => (p.config().idle / 4).clamp(Duration::from_secs(1), Duration::from_secs(60)),
                    None => break,
                };
                tokio::time::sleep(every).await;
                match weak.upgrade() {
                    Some(p) => {
                        p.reap();
                    }
                    None => break,
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quick(max: usize) -> Arc<Pool> {
        Pool::new(Config { max, queue_wait: Duration::from_millis(300), ..Config::default() })
    }

    #[tokio::test]
    async fn the_queue_is_first_come_first_served() {
        let pool = Pool::new(Config { max: 1, queue_wait: Duration::from_secs(10), ..Config::default() });
        let stop = CancellationToken::new();
        let first = pool.acquire("a", "ra", Path::new("/a"), &stop).await.unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut waiting = Vec::new();
        for who in ["b", "c"] {
            let (pool, stop, order) = (pool.clone(), stop.clone(), order.clone());
            waiting.push(tokio::spawn(async move {
                let slot = pool.acquire(who, "ra", Path::new("/x"), &stop).await.unwrap();
                lock(&order).push(who);
                tokio::time::sleep(Duration::from_millis(50)).await;
                drop(slot);
            }));
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        drop(first);
        for w in waiting {
            w.await.unwrap();
        }
        assert_eq!(*lock(&order), ["b", "c"]);
    }

    #[tokio::test]
    async fn a_call_past_the_cap_waits_then_names_the_holders() {
        let pool = quick(2);
        let stop = CancellationToken::new();
        let _a = pool.acquire("session one", "rust-analyzer", Path::new("/w/one"), &stop).await.unwrap();
        let _b = pool.acquire("session two", "rust-analyzer", Path::new("/w/two"), &stop).await.unwrap();
        let started = Instant::now();
        let err = pool.acquire("session three", "rust-analyzer", Path::new("/w/three"), &stop).await.err().unwrap();
        assert!(started.elapsed() >= Duration::from_millis(250));
        assert!(err.contains("the host is busy"), "{err}");
        assert!(err.contains("session one (rust-analyzer in /w/one)") && err.contains("session two"), "{err}");
        assert!(err.contains("search_files"), "{err}");
        assert_eq!(pool.total().servers, 2);
        assert!(lock(&pool.state).queue.is_empty());
    }

    #[tokio::test]
    async fn a_freed_slot_goes_to_the_one_waiting() {
        let pool = Pool::new(Config { max: 1, queue_wait: Duration::from_secs(10), ..Config::default() });
        let stop = CancellationToken::new();
        let a = pool.acquire("a", "ra", Path::new("/a"), &stop).await.unwrap();
        let waiter = {
            let (pool, stop) = (pool.clone(), stop.clone());
            tokio::spawn(async move { pool.acquire("b", "ra", Path::new("/b"), &stop).await.map(|s| s.id()) })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!waiter.is_finished());
        drop(a);
        assert!(waiter.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn stopping_idle_servers_asks_each_and_adds_up_what_was_freed() {
        let pool = quick(3);
        let stop = CancellationToken::new();
        let a = pool.acquire("a", "ra", Path::new("/a"), &stop).await.unwrap();
        let b = pool.acquire("b", "ra", Path::new("/b"), &stop).await.unwrap();
        pool.set_resident(a.id(), 3 << 30);
        pool.set_resident(b.id(), 1 << 30);
        let asked = Arc::new(Mutex::new(Vec::new()));
        let held = Arc::new(Mutex::new(Some(a)));
        let (h, k) = (held.clone(), asked.clone());
        pool.attach(1, Some(10), Arc::new(move |d, _| {
            lock(&k).push(d);
            lock(&h).take().is_some()
        }));
        pool.attach(b.id(), Some(11), Arc::new(|_, _| false));
        let freed = pool.stop_idle_language_servers();
        assert_eq!(freed, Freed { servers: 1, bytes: 3 << 30 });
        assert_eq!(*lock(&asked), [RECENT]);
        assert_eq!(pool.held().len(), 1);
        assert_eq!(pool.held()[0].holder, "b");
    }
}
