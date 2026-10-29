use agentbench_core::{Screen, SessionId};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

const ROWS: u16 = 30;
const COLS: u16 = 110;
const LIMIT: u16 = 400;

struct Term {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    screen: Arc<Mutex<vt100::Parser>>,
}

impl Drop for Term {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

#[derive(Clone, Default)]
pub struct Terminals(Arc<Mutex<HashMap<SessionId, Term>>>);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn nz(n: u16) -> std::num::NonZeroU16 {
    std::num::NonZeroU16::new(n.max(1)).unwrap_or(std::num::NonZeroU16::MIN)
}

fn shell() -> String {
    std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into())
}

impl Terminals {
    pub fn open(
        &self,
        session: SessionId,
        cwd: &Path,
        program: Option<&str>,
    ) -> Result<(), String> {
        let mut all = lock(&self.0);
        if all.contains_key(&session) {
            return Ok(());
        }
        let pair = native_pty_system()
            .openpty(PtySize { rows: ROWS, cols: COLS, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| e.to_string())?;
        let mut cmd = CommandBuilder::new(program.map(str::to_string).unwrap_or_else(shell));
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
        let screen = Arc::new(Mutex::new(vt100::Parser::new(nz(ROWS), nz(COLS), 0)));
        let feed = screen.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                lock(&feed).process(&buf[..n]);
            }
        });
        all.insert(session, Term { master: pair.master, writer, child, screen });
        Ok(())
    }

    pub fn write(&self, session: SessionId, data: &[u8]) -> Result<(), String> {
        let mut all = lock(&self.0);
        let t = all.get_mut(&session).ok_or("no terminal is open for this chat")?;
        t.writer.write_all(data).and_then(|_| t.writer.flush()).map_err(|e| e.to_string())
    }

    pub fn resize(&self, session: SessionId, rows: u16, cols: u16) -> Result<(), String> {
        let (rows, cols) = (rows.clamp(5, LIMIT), cols.clamp(20, LIMIT));
        let all = lock(&self.0);
        let t = all.get(&session).ok_or("no terminal is open for this chat")?;
        t.master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| e.to_string())?;
        lock(&t.screen).set_size(nz(rows), nz(cols));
        Ok(())
    }

    pub fn screen(&self, session: SessionId) -> Option<Screen> {
        let mut all = lock(&self.0);
        let t = all.get_mut(&session)?;
        let alive = t.child.try_wait().ok().flatten().is_none();
        let parser = lock(&t.screen);
        let s = parser.screen();
        let (rows, cols) = s.size();
        let (rows, cols) = (rows.get(), cols.get());
        let (row, col) = s.cursor_position();
        Some(Screen {
            rows: s.rows(0, cols).collect(),
            cursor_row: row,
            cursor_col: col,
            cursor_shown: !s.hide_cursor(),
            height: rows,
            width: cols,
            alive,
        })
    }

    pub fn close(&self, session: SessionId) {
        lock(&self.0).remove(&session);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn id() -> SessionId {
        "00000000-0000-4000-8000-000000000009".parse().unwrap()
    }

    fn wait_for(t: &Terminals, text: &str) -> Screen {
        for _ in 0..100 {
            if let Some(s) = t.screen(id())
                && s.rows.iter().any(|r| r.contains(text))
            {
                return s;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("never showed {text:?}: {:?}", t.screen(id()));
    }

    #[test]
    fn typed_input_reaches_the_shell_and_its_output_shows_on_the_screen() {
        let t = Terminals::default();
        let dir = tempfile::tempdir().unwrap();
        t.open(id(), dir.path(), Some("/bin/sh")).unwrap();
        t.write(id(), b"echo $((6*7)) in; pwd\n").unwrap();
        let s = wait_for(&t, "42 in");
        assert!(s.alive);
        assert!(
            s.rows.iter().any(|r| r.contains(dir.path().file_name().unwrap().to_str().unwrap()))
        );
        t.resize(id(), 12, 50).unwrap();
        let s = t.screen(id()).unwrap();
        assert_eq!((s.height, s.width), (12, 50));
        t.write(id(), b"exit\n").unwrap();
        for _ in 0..100 {
            if !t.screen(id()).unwrap().alive {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(!t.screen(id()).unwrap().alive);
        t.close(id());
        assert!(t.screen(id()).is_none());
    }
}
