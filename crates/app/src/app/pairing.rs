use crate::client::Server;
use std::path::{Path, PathBuf};

#[derive(serde::Serialize, serde::Deserialize)]
struct Saved {
    url: String,
    token: String,
}

pub(super) fn read(file: &Path) -> Option<Server> {
    let text = std::fs::read(file).ok()?;
    let saved: Saved = serde_json::from_slice(&text).ok()?;
    (!saved.url.trim().is_empty() && !saved.token.trim().is_empty())
        .then_some(Server { url: saved.url, token: saved.token })
}

pub(super) fn write(file: &Path, server: &Server) -> std::io::Result<()> {
    use std::io::Write;
    let saved = Saved { url: server.url.clone(), token: server.token.clone() };
    let body = serde_json::to_vec(&saved).map_err(std::io::Error::other)?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = file.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(&body)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, file)?;
    if let Some(dir) = file.parent()
        && let Ok(d) = std::fs::File::open(dir)
    {
        let _ = d.sync_all();
    }
    Ok(())
}
