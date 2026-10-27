use super::files::{self, number};
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub const DIRECTORY: &str = "https://skills.sh";
const DEFAULT: usize = 8;
const MOST: usize = 20;
const TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Deserialize, PartialEq)]
pub struct Skill {
    pub source: String,
    #[serde(rename = "skillId")]
    pub skill_id: String,
    #[serde(default)]
    pub installs: u64,
}

#[derive(Deserialize)]
struct Found {
    #[serde(default)]
    skills: Vec<Skill>,
}

fn installs(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}K", n as f64 / 1000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

pub fn format_skills(base: &str, query: &str, skills: &[Skill]) -> String {
    if skills.is_empty() {
        return format!(
            "no skills found for {query:?}. Try other words, or carry on without one. \
             Browse the directory at {base}"
        );
    }
    let mut out = format!("{} skills for {query:?}, most installed first:\n", skills.len());
    for s in skills {
        out.push_str(&format!(
            "\n{}@{}  {} installs\n  {base}/{}/{}\n  npx skills add {}@{} -g -y\n",
            s.source,
            s.skill_id,
            installs(s.installs),
            s.source,
            s.skill_id,
            s.source,
            s.skill_id,
        ));
    }
    out.push_str(
        "\nRead the skill's page before relying on it. Prefer many installs and a known author. \
         Installing runs someone else's instructions on this machine: ask the user before you do, \
         then run the command with bash (it needs Node) and load_skill to read it.",
    );
    out
}

pub async fn find_at(base: &str, query: &str, want: usize) -> Result<Vec<Skill>, String> {
    let mut url = url::Url::parse(&format!("{}/api/search", base.trim_end_matches('/')))
        .map_err(|e| e.to_string())?;
    url.query_pairs_mut().append_pair("q", query).append_pair("limit", &want.to_string());
    let resp = crate::http::client()
        .get(url)
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("the skills directory failed: {}", super::native::describe(&e)))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("the skills directory failed: {status}"));
    }
    let found: Found = resp.json().await.map_err(|e| format!("unreadable skills answer: {e}"))?;
    let mut skills = found.skills;
    skills.truncate(want);
    Ok(skills)
}

pub async fn find(args: &Value, stop: &CancellationToken) -> Result<String, String> {
    let query = files::text(args, &["query", "q", "search"]).map(str::trim).unwrap_or_default();
    if query.is_empty() {
        return Err("missing `query`".into());
    }
    let want =
        number(args, &["limit", "max_results"]).filter(|n| *n > 0).unwrap_or(DEFAULT).min(MOST);
    tokio::select! {
        _ = stop.cancelled() => Err("stopped".into()),
        skills = find_at(DIRECTORY, query, want) => Ok(format_skills(DIRECTORY, query, &skills?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const ANSWER: &str = r#"{"query":"pdf","searchType":"fuzzy","skills":[
        {"id":"anthropics/skills/pdf","source":"anthropics/skills","skillId":"pdf","name":"pdf","installs":217748},
        {"id":"x/y/tiny","source":"x/y","skillId":"tiny","name":"tiny","installs":12}],"count":2}"#;

    async fn directory(
        status: &'static str,
        body: &'static str,
    ) -> (String, tokio::sync::oneshot::Receiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = s.read(&mut buf).await.unwrap_or(0);
            let _ = tx.send(String::from_utf8_lossy(&buf[..n]).to_string());
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(head.as_bytes()).await;
            let _ = s.write_all(body.as_bytes()).await;
        });
        (format!("http://{addr}"), rx)
    }

    #[tokio::test]
    async fn the_directory_is_asked_for_the_query_and_the_answer_is_read() {
        let (base, request) = directory("200 OK", ANSWER).await;
        let skills = find_at(&base, "pdf forms", 5).await.unwrap();
        let line = request.await.unwrap();
        let first = line.lines().next().unwrap();
        assert!(
            first.starts_with("GET /api/search?q=pdf+forms&limit=5 ")
                || first.starts_with("GET /api/search?q=pdf%20forms&limit=5 "),
            "{first}"
        );
        assert_eq!(skills.len(), 2);
        assert_eq!(
            skills[0],
            Skill { source: "anthropics/skills".into(), skill_id: "pdf".into(), installs: 217748 }
        );
    }

    #[tokio::test]
    async fn a_failing_directory_is_reported() {
        let (base, _) = directory("503 Service Unavailable", "{}").await;
        let err = find_at(&base, "pdf", 5).await.unwrap_err();
        assert!(err.contains("503"), "{err}");
    }

    #[tokio::test]
    async fn the_answer_has_more_than_the_limit_it_is_cut() {
        let (base, _) = directory("200 OK", ANSWER).await;
        assert_eq!(find_at(&base, "pdf", 1).await.unwrap().len(), 1);
    }

    #[test]
    fn each_skill_shows_its_page_and_install_command() {
        let skills = vec![
            Skill { source: "anthropics/skills".into(), skill_id: "pdf".into(), installs: 217748 },
            Skill { source: "x/y".into(), skill_id: "tiny".into(), installs: 12 },
        ];
        let text = format_skills("https://skills.sh", "pdf", &skills);
        assert!(text.contains("anthropics/skills@pdf  217.7K installs"));
        assert!(text.contains("https://skills.sh/anthropics/skills/pdf"));
        assert!(text.contains("npx skills add anthropics/skills@pdf -g -y"));
        assert!(text.contains("x/y@tiny  12 installs"));
        assert!(text.contains("ask the user before"));
    }

    #[test]
    fn no_results_say_so_without_an_install_hint() {
        let text = format_skills("https://skills.sh", "zzz", &[]);
        assert!(text.contains("no skills found"));
        assert!(!text.contains("npx"));
    }

    #[test]
    fn install_counts_are_short() {
        assert_eq!(installs(999), "999");
        assert_eq!(installs(1500), "1.5K");
        assert_eq!(installs(2_300_000), "2.3M");
    }

    #[tokio::test]
    async fn a_missing_query_is_refused() {
        let err = find(&serde_json::json!({}), &CancellationToken::new()).await.unwrap_err();
        assert_eq!(err, "missing `query`");
    }
}
