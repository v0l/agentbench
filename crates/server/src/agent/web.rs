//! `web_search` and `web_fetch`: let an agent look things up on the web and read or download a
//! page. Search is DuckDuckGo's keyless HTML endpoint; fetch turns a page into readable markdown
//! (no scripts, menus or footers) that the agent can read in pages, or have a small model answer a question about.

use super::files::{self, number};
use crate::titles::Titles;
use egui_bench::agent::grants::Access;
use egui_bench::agent::workspace::Workspace;
use futures_util::StreamExt;
use scraper::node::Node;
use scraper::{ElementRef, Html, Selector};
use serde_json::Value;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

const SEARCH_URL: &str = "https://html.duckduckgo.com/html/";
const SEARCH_RESULTS: usize = 8;
const SEARCH_MAX: usize = 20;
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const SAVE_TIMEOUT: Duration = Duration::from_secs(300);
/// The most of a page that is read into memory to show.
const READ_BYTES: usize = 5_000_000;
/// The most of a file that is downloaded to disk.
const SAVE_BYTES: usize = 200_000_000;
const PAGE_CHARS: usize = 20_000;
const PAGE_MAX: usize = 100_000;
const BROWSER: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0 agentbench";

const QUERY: [&str; 3] = ["query", "q", "search"];
const URL: [&str; 3] = ["url", "uri", "link"];

fn client_error(e: reqwest::Error) -> String {
    super::native::describe(&e)
}

/// Wait for a request or the user's stop, whichever comes first.
async fn until_stopped<T>(
    stop: &CancellationToken,
    work: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::select! {
        _ = stop.cancelled() => Err("stopped".into()),
        r = work => r,
    }
}

// ---- search ----

#[derive(Debug, PartialEq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

fn sel(css: &str) -> Selector {
    Selector::parse(css).expect("a valid selector")
}

fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// DuckDuckGo wraps each result link in a redirect that carries the real address in `uddg`.
fn result_url(href: &str) -> Option<String> {
    let link = Url::parse(SEARCH_URL).ok()?.join(href).ok()?;
    let target = link
        .query_pairs()
        .find(|(k, _)| k == "uddg")
        .map(|(_, v)| Url::parse(&v))
        .unwrap_or(Ok(link))
        .ok()?;
    matches!(target.scheme(), "http" | "https").then(|| target.to_string())
}

pub fn parse_results(page: &str) -> Result<Vec<Hit>, String> {
    let doc = Html::parse_document(page);
    if doc.select(&sel(".anomaly-modal__box, #challenge-form")).next().is_some() {
        return Err("DuckDuckGo asked for a bot check and is rate limiting; wait a minute, or \
                    fetch a search page you know with web_fetch"
            .into());
    }
    let (result, link, snippet) = (sel(".result"), sel(".result__a"), sel(".result__snippet"));
    let mut hits = Vec::new();
    for r in doc.select(&result) {
        if r.value().classes().any(|c| c == "result--ad") {
            continue;
        }
        let Some(a) = r.select(&link).next() else { continue };
        let title = squash(&a.text().collect::<String>());
        let Some(url) = a.value().attr("href").and_then(result_url) else { continue };
        if title.is_empty() {
            continue;
        }
        let snippet = r.select(&snippet).next().map(|s| squash(&s.text().collect::<String>()));
        hits.push(Hit { title, url, snippet: snippet.unwrap_or_default() });
    }
    Ok(hits)
}

fn host_matches(url: &str, domain: &str) -> bool {
    let domain = domain.trim().trim_start_matches('-').trim_start_matches("www.").to_lowercase();
    let host =
        Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_lowercase)).unwrap_or_default();
    !domain.is_empty() && (host == domain || host.ends_with(&format!(".{domain}")))
}

/// `domains` keeps results from the listed sites; a leading `-` drops a site instead.
pub fn filter_domains(hits: Vec<Hit>, domains: &[String]) -> Vec<Hit> {
    let (drop, keep): (Vec<&String>, Vec<&String>) =
        domains.iter().partition(|d| d.trim().starts_with('-'));
    hits.into_iter()
        .filter(|h| keep.is_empty() || keep.iter().any(|d| host_matches(&h.url, d)))
        .filter(|h| !drop.iter().any(|d| host_matches(&h.url, d)))
        .collect()
}

fn list_of(args: &Value, keys: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for k in keys {
        match args.get(*k) {
            Some(Value::String(s)) => {
                out.extend(s.split([',', ' ']).filter(|d| !d.is_empty()).map(str::to_string))
            }
            Some(Value::Array(l)) => {
                out.extend(l.iter().filter_map(Value::as_str).map(str::to_string))
            }
            _ => {}
        }
    }
    out
}

pub fn format_hits(query: &str, hits: &[Hit]) -> String {
    if hits.is_empty() {
        return format!("no results for {query:?}; try other words, or fewer filters");
    }
    let mut out = String::new();
    for (i, h) in hits.iter().enumerate() {
        out.push_str(&format!("{}. {}\n   {}\n", i + 1, h.title, h.url));
        if !h.snippet.is_empty() {
            out.push_str(&format!("   {}\n", h.snippet));
        }
    }
    out.push_str("\nRead a result in full with web_fetch.");
    out
}

pub async fn search(args: &Value, stop: &CancellationToken) -> Result<String, String> {
    let query = files::text(args, &QUERY).map(str::trim).unwrap_or_default();
    if query.is_empty() {
        return Err("missing `query`".into());
    }
    let domains = list_of(args, &["domains", "domain", "sites"]);
    let want = number(args, &["limit", "max_results", "num_results"])
        .filter(|n| *n > 0)
        .unwrap_or(SEARCH_RESULTS)
        .min(SEARCH_MAX);
    let mut search_url = Url::parse(SEARCH_URL).map_err(|e| e.to_string())?;
    search_url.query_pairs_mut().append_pair("q", query);
    let page = until_stopped(stop, async {
        let resp = crate::http::client()
            .get(search_url)
            .header("Accept", "text/html")
            .header("User-Agent", BROWSER)
            .timeout(READ_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("search failed: {}", client_error(e)))?;
        let status = resp.status();
        if status.as_u16() == 202 {
            return Err("DuckDuckGo is rate limiting; wait a minute and try again".into());
        }
        let body = resp.text().await.map_err(client_error)?;
        if !status.is_success() {
            return Err(format!("search failed: {status}"));
        }
        Ok(body)
    })
    .await?;
    let hits = filter_domains(parse_results(&page)?, &domains);
    let hits: Vec<Hit> = hits.into_iter().take(want).collect();
    Ok(format_hits(query, &hits))
}

// ---- fetch ----

#[derive(Debug, PartialEq)]
pub enum Kind {
    Html,
    Text,
    Binary,
}

pub fn kind_of(content_type: &str, body: &[u8]) -> Kind {
    let ct = content_type.split(';').next().unwrap_or("").trim().to_lowercase();
    let sniff = || {
        let head = String::from_utf8_lossy(&body[..body.len().min(2048)]).to_lowercase();
        head.contains("<html") || head.trim_start().starts_with("<!doctype html")
    };
    match ct.as_str() {
        "text/html" | "application/xhtml+xml" => Kind::Html,
        "" | "application/octet-stream" | "text/plain" => {
            if body[..body.len().min(8000)].contains(&0) {
                Kind::Binary
            } else if sniff() {
                Kind::Html
            } else {
                Kind::Text
            }
        }
        t if t.starts_with("text/")
            || t.ends_with("json")
            || t.ends_with("xml")
            || t.ends_with("yaml")
            || t.ends_with("javascript")
            || t.ends_with("toml") =>
        {
            Kind::Text
        }
        _ => Kind::Binary,
    }
}

const SKIP: [&str; 20] = [
    "script", "style", "noscript", "template", "svg", "head", "nav", "footer", "aside", "form",
    "iframe", "canvas", "button", "select", "input", "dialog", "audio", "video", "object", "embed",
];

#[derive(Default)]
struct Out {
    buf: String,
}

impl Out {
    fn gap(&self) -> bool {
        self.buf.is_empty() || self.buf.ends_with([' ', '\n'])
    }

    fn text(&mut self, t: &str, pre: bool) {
        if pre {
            self.buf.push_str(t);
            return;
        }
        let words = squash(t);
        if words.is_empty() {
            if !t.is_empty() && !self.gap() {
                self.buf.push(' ');
            }
            return;
        }
        if t.starts_with(char::is_whitespace) && !self.gap() {
            self.buf.push(' ');
        }
        self.buf.push_str(&words);
        if t.ends_with(char::is_whitespace) {
            self.buf.push(' ');
        }
    }

    fn trim_end(&mut self) {
        let n = self.buf.trim_end_matches(' ').len();
        self.buf.truncate(n);
    }

    /// End the current line.
    fn line(&mut self) {
        self.trim_end();
        if !self.buf.is_empty() && !self.buf.ends_with('\n') {
            self.buf.push('\n');
        }
    }

    /// End the current line and leave a blank one.
    fn block(&mut self) {
        self.line();
        if !self.buf.is_empty() && !self.buf.ends_with("\n\n") {
            self.buf.push('\n');
        }
    }
}

struct Walk<'a> {
    out: Out,
    base: &'a Url,
    lists: Vec<Option<usize>>,
}

impl Walk<'_> {
    fn children(&mut self, el: ElementRef, pre: bool) {
        for child in el.children() {
            match child.value() {
                Node::Text(t) => self.out.text(t, pre),
                Node::Element(_) => {
                    if let Some(e) = ElementRef::wrap(child) {
                        self.element(e, pre);
                    }
                }
                _ => {}
            }
        }
    }

    fn element(&mut self, el: ElementRef, pre: bool) {
        let name = el.value().name();
        if SKIP.contains(&name)
            || el.value().attr("hidden").is_some()
            || el.value().attr("aria-hidden") == Some("true")
        {
            return;
        }
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = name[1..].parse::<usize>().unwrap_or(1);
                self.out.block();
                self.out.buf.push_str(&"#".repeat(level));
                self.out.buf.push(' ');
                self.children(el, pre);
                self.out.block();
            }
            "p" | "blockquote" | "figure" | "table" | "details" | "dl" => {
                self.out.block();
                self.children(el, pre);
                self.out.block();
            }
            "div" | "section" | "article" | "main" | "header" | "tr" | "dt" | "dd"
            | "figcaption" | "summary" | "address" => {
                self.out.line();
                self.children(el, pre);
                self.out.line();
            }
            "br" => self.out.buf.push('\n'),
            "hr" => {
                self.out.block();
                self.out.buf.push_str("---");
                self.out.block();
            }
            "ul" | "ol" => {
                self.out.line();
                self.lists.push(if name == "ol" { Some(1) } else { None });
                self.children(el, pre);
                self.lists.pop();
                self.out.line();
                if self.lists.is_empty() {
                    self.out.block();
                }
            }
            "li" => {
                self.out.line();
                let indent = "  ".repeat(self.lists.len().saturating_sub(1));
                self.out.buf.push_str(&indent);
                match self.lists.last_mut() {
                    Some(Some(n)) => {
                        self.out.buf.push_str(&format!("{n}. "));
                        *n += 1;
                    }
                    _ => self.out.buf.push_str("- "),
                }
                self.children(el, pre);
                self.out.line();
            }
            "pre" => {
                self.out.block();
                self.out.buf.push_str("```\n");
                self.children(el, true);
                self.out.line();
                self.out.buf.push_str("```");
                self.out.block();
            }
            "code" if !pre => {
                self.out.buf.push('`');
                self.children(el, pre);
                self.out.buf.push('`');
            }
            "td" | "th" => {
                self.children(el, pre);
                self.out.trim_end();
                self.out.buf.push_str(" | ");
            }
            "a" => self.link(el, pre),
            _ => self.children(el, pre),
        }
    }

    fn link(&mut self, el: ElementRef, pre: bool) {
        let start = self.out.buf.len();
        self.children(el, pre);
        let href = el.value().attr("href").map(str::trim).unwrap_or_default();
        let shown = &self.out.buf[start..];
        let lead = shown.len() - shown.trim_start().len();
        if shown.trim().is_empty() || href.is_empty() || href.starts_with('#') {
            return;
        }
        let Ok(link) = self.base.join(href) else { return };
        if !matches!(link.scheme(), "http" | "https") {
            return;
        }
        self.out.trim_end();
        self.out.buf.insert(start + lead, '[');
        self.out.buf.push_str(&format!("]({link})"));
    }
}

/// A page as markdown: the title, then the main content with headings, lists, code and links
/// kept and the scripts, menus and footers dropped.
pub fn html_to_markdown(page: &str, base: &Url) -> String {
    let doc = Html::parse_document(page);
    let title = doc
        .select(&sel("title"))
        .next()
        .map(|t| squash(&t.text().collect::<String>()))
        .unwrap_or_default();
    let root = ["main", "article", "[role=main]", "body"]
        .iter()
        .find_map(|s| doc.select(&sel(s)).next())
        .unwrap_or_else(|| doc.root_element());
    let mut walk = Walk { out: Out::default(), base, lists: Vec::new() };
    walk.element(root, false);
    let body = walk.out.buf;
    let lines: Vec<&str> = body.lines().map(str::trim_end).collect();
    let mut text = String::new();
    let mut blank = 0;
    for line in lines {
        if line.trim().is_empty() {
            blank += 1;
            if blank <= 1 && !text.is_empty() {
                text.push('\n');
            }
        } else {
            blank = 0;
            text.push_str(line);
            text.push('\n');
        }
    }
    let text = text.trim().to_string();
    match (title.is_empty(), text.starts_with("# ")) {
        (false, false) => format!("# {title}\n\n{text}"),
        _ => text,
    }
}

/// `chars` characters of `text` from character `offset`, and a note on how to read on.
pub fn page(text: &str, offset: usize, chars: usize) -> String {
    let total = text.chars().count();
    if offset >= total && total > 0 {
        return format!("[offset {offset} is past the end; the page is {total} characters]");
    }
    let shown: String = text.chars().skip(offset).take(chars).collect();
    let end = offset + shown.chars().count();
    if end < total {
        format!(
            "{shown}\n\n[showing characters {offset}-{end} of {total}; call again with offset={end} for more]"
        )
    } else if offset > 0 {
        format!("{shown}\n\n[characters {offset}-{total} of {total}: the end]")
    } else {
        shown
    }
}

fn file_name(url: &Url, content_type: &str) -> String {
    let last = url.path_segments().and_then(|mut s| s.next_back()).unwrap_or_default();
    if !last.is_empty() {
        return last.to_string();
    }
    match content_type.split(';').next().unwrap_or("").trim() {
        "text/html" => "index.html".into(),
        "application/json" => "download.json".into(),
        _ => "download".into(),
    }
}

fn parse_url(raw: &str) -> Result<Url, String> {
    let raw = raw.trim();
    let url = Url::parse(raw)
        .or_else(|_| Url::parse(&format!("https://{raw}")))
        .map_err(|e| format!("{raw:?} is not a web address ({e})"))?;
    match url.scheme() {
        "http" | "https" if url.host_str().is_some() => Ok(url),
        s => Err(format!("only http and https addresses can be fetched, not {s}")),
    }
}

/// Read a body up to `max` bytes. The flag says whether more was left unread.
async fn body(resp: reqwest::Response, max: usize) -> Result<(Vec<u8>, bool), String> {
    let mut stream = resp.bytes_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(client_error)?;
        if out.len() + chunk.len() > max {
            out.extend_from_slice(&chunk[..max - out.len()]);
            return Ok((out, true));
        }
        out.extend_from_slice(&chunk);
    }
    Ok((out, false))
}

pub async fn fetch(
    args: &Value,
    ws: &Workspace,
    titles: Option<&Titles>,
    stop: &CancellationToken,
) -> Result<String, String> {
    let url = parse_url(files::text(args, &URL).ok_or("missing `url`")?)?;
    let save = files::text(args, &["save_to", "save", "download_to", "output_path"])
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let offset = number(args, &["offset", "start"]).unwrap_or(0);
    let chars = number(args, &["max_chars", "limit"]).filter(|n| *n > 0).unwrap_or(PAGE_CHARS);
    let chars = chars.min(PAGE_MAX);
    let raw = args.get("raw").and_then(Value::as_bool).unwrap_or(false);
    let prompt = files::text(args, &["prompt", "question", "extract"])
        .map(str::trim)
        .filter(|p| !p.is_empty());
    if prompt.is_some() && save.is_some() {
        return Err("prompt reads the page, so it cannot be combined with save_to".into());
    }
    let extractor = match (prompt, titles) {
        (Some(_), None) => {
            return Err("there is no title model to read the page; fetch it without prompt \
                        and read it yourself"
                .into());
        }
        (Some(p), Some(t)) => Some((t, p)),
        _ => None,
    };

    until_stopped(stop, async {
        let resp = crate::http::client()
            .get(url.clone())
            .header("User-Agent", BROWSER)
            .header("Accept", "text/html,application/xhtml+xml,text/plain,application/json;q=0.9,*/*;q=0.5")
            .timeout(if save.is_some() { SAVE_TIMEOUT } else { READ_TIMEOUT })
            .send()
            .await
            .map_err(|e| format!("{url}: {}", client_error(e)))?;
        let status = resp.status();
        let final_url = resp.url().clone();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if !status.is_success() {
            let (bytes, _) = body(resp, 2000).await.unwrap_or_default();
            let text = String::from_utf8_lossy(&bytes);
            let hint = match status.as_u16() {
                401 | 403 | 429 => " (the site refuses automated readers; try another source)",
                404 | 410 => " (the page is gone; search for it again)",
                _ => "",
            };
            return Err(format!("{final_url}: {status}{hint}\n{}", squash(&text).chars().take(300).collect::<String>()));
        }

        if let Some(save) = save {
            let mut dest = ws.resolve_for(save, Access::Write)?;
            if dest.is_dir() || save.ends_with('/') {
                dest.push(file_name(&final_url, &content_type));
            }
            let (bytes, cut) = body(resp, SAVE_BYTES).await?;
            if cut {
                return Err(format!(
                    "{final_url} is larger than {} MB, which is the most that is downloaded",
                    SAVE_BYTES / 1_000_000
                ));
            }
            if let Some(dir) = dest.parent() {
                tokio::fs::create_dir_all(dir).await.map_err(|e| e.to_string())?;
            }
            tokio::fs::write(&dest, &bytes)
                .await
                .map_err(|e| format!("could not write {}: {e}", dest.display()))?;
            return Ok(format!(
                "saved {} bytes ({}) from {final_url} to {}",
                bytes.len(),
                if content_type.is_empty() { "unknown type" } else { &content_type },
                ws.shown(&dest)
            ));
        }

        let (bytes, cut) = body(resp, READ_BYTES).await?;
        let mut text = match kind_of(&content_type, &bytes) {
            Kind::Binary => {
                return Err(format!(
                    "{final_url} is {} ({} bytes), not text; pass save_to to download it, then read_file can show pictures, sound and video",
                    if content_type.is_empty() { "binary data" } else { &content_type },
                    bytes.len()
                ));
            }
            Kind::Html if !raw => html_to_markdown(&String::from_utf8_lossy(&bytes), &final_url),
            _ => String::from_utf8_lossy(&bytes).into_owned(),
        };
        if cut {
            text.push_str("\n\n[the page was cut at 5 MB]");
        }
        let mut out = String::new();
        if final_url != url {
            out.push_str(&format!("[redirected to {final_url}]\n\n"));
        }
        if let Some((titles, prompt)) = extractor {
            let answer = titles.extract(&text, prompt).await.map_err(|e| {
                format!(
                    "could not read {final_url} with the title model: {e}; fetch it without \
                     prompt and read it yourself"
                )
            })?;
            out.push_str(&format!("From {final_url}, by the title model:\n\n{answer}"));
            return Ok(out);
        }
        out.push_str(&page(&text, offset, chars));
        Ok(out)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const DDG: &str = r#"<html><body>
      <div class="result result--ad"><a class="result__a" href="https://ads.example/x">Buy now</a></div>
      <div class="result results_links">
        <h2><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust-lang.org%2Fbook%2F&rut=abc">The Rust
           Programming Language</a></h2>
        <a class="result__snippet" href="x">Learn   <b>Rust</b> with the book.</a>
      </div>
      <div class="result"><a class="result__a" href="https://github.com/rust-lang/rust">rust-lang/rust</a></div>
      <div class="result"><a class="result__a" href="javascript:void(0)">Bad</a></div>
    </body></html>"#;

    #[test]
    fn search_results_are_read_from_duckduckgo_html() {
        let hits = parse_results(DDG).unwrap();
        assert_eq!(
            hits,
            vec![
                Hit {
                    title: "The Rust Programming Language".into(),
                    url: "https://doc.rust-lang.org/book/".into(),
                    snippet: "Learn Rust with the book.".into(),
                },
                Hit {
                    title: "rust-lang/rust".into(),
                    url: "https://github.com/rust-lang/rust".into(),
                    snippet: String::new(),
                },
            ]
        );
        let text = format_hits("rust", &hits);
        assert!(text.starts_with(
            "1. The Rust Programming Language\n   https://doc.rust-lang.org/book/\n   Learn Rust"
        ));
        assert!(format_hits("zzz", &[]).starts_with("no results for \"zzz\""));
        assert!(
            parse_results(r#"<div id="challenge-form"></div>"#).unwrap_err().contains("bot check")
        );
    }

    #[test]
    fn domains_keep_or_drop_sites() {
        let hits = || parse_results(DDG).unwrap();
        let only = filter_domains(hits(), &["github.com".into()]);
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].title, "rust-lang/rust");
        let without = filter_domains(hits(), &["-github.com".into()]);
        assert_eq!(without.len(), 1);
        assert!(without[0].url.contains("rust-lang.org"));
        let sub = filter_domains(hits(), &["www.rust-lang.org".into()]);
        assert_eq!(sub.len(), 1, "a parent domain matches its subdomains");
        assert_eq!(filter_domains(hits(), &[]).len(), 2);
        assert_eq!(
            list_of(&json!({"domains": "a.com, -b.com"}), &["domains"]),
            ["a.com", "-b.com"]
        );
    }

    #[test]
    fn a_page_becomes_readable_markdown() {
        let html = r##"<html><head><title>  My   Page </title><style>p{}</style></head><body>
          <nav><a href="/home">Home</a></nav>
          <main>
            <h1>Heading</h1>
            <p>Some <b>bold</b> text and a <a href="/docs/intro">link</a>, then <code>code()</code>.</p>
            <ul><li>one</li><li>two<ul><li>nested</li></ul></li></ul>
            <ol><li>first</li><li>second</li></ol>
            <pre>fn main() {
    println!("hi");
}</pre>
            <script>alert(1)</script>
            <a href="#top">skip</a><span hidden>secret</span>
          </main>
          <footer>Copyright</footer></body></html>"##;
        let base = Url::parse("https://example.com/a/b").unwrap();
        let md = html_to_markdown(html, &base);
        assert_eq!(
            md,
            "# Heading\n\nSome bold text and a [link](https://example.com/docs/intro), then `code()`.\n\n\
             - one\n- two\n  - nested\n\n1. first\n2. second\n\n```\nfn main() {\n    println!(\"hi\");\n}\n```\n\nskip"
        );
        assert!(
            !md.contains("alert")
                && !md.contains("Home")
                && !md.contains("Copyright")
                && !md.contains("secret")
        );
        // with no heading of its own, the page title heads it
        let md = html_to_markdown("<title>T</title><body><p>hello</p></body>", &base);
        assert_eq!(md, "# T\n\nhello");
    }

    #[test]
    fn long_pages_are_read_in_pieces() {
        let text = "abcdefghij";
        assert_eq!(page(text, 0, 20), "abcdefghij");
        let first = page(text, 0, 4);
        assert!(
            first.starts_with("abcd\n\n[showing characters 0-4 of 10; call again with offset=4")
        );
        assert!(page(text, 8, 4).starts_with("ij\n\n[characters 8-10 of 10: the end]"));
        assert!(page(text, 50, 4).contains("past the end"));
        assert_eq!(
            page("héllo wörld", 1, 4),
            "éllo\n\n[showing characters 1-5 of 11; call again with offset=5 for more]"
        );
    }

    #[test]
    fn the_kind_comes_from_the_header_then_the_bytes() {
        assert_eq!(kind_of("text/html; charset=utf-8", b"x"), Kind::Html);
        assert_eq!(kind_of("application/json", b"{}"), Kind::Text);
        assert_eq!(kind_of("image/png", b"\x89PNG"), Kind::Binary);
        assert_eq!(kind_of("", b"<!DOCTYPE html><p>x"), Kind::Html);
        assert_eq!(kind_of("application/octet-stream", b"a\0b"), Kind::Binary);
        assert_eq!(kind_of("text/plain", b"just words"), Kind::Text);
        assert!(parse_url("file:///etc/passwd").is_err());
        assert_eq!(parse_url("example.com/x").unwrap().as_str(), "https://example.com/x");
    }

    /// A one-route web server that answers `path -> (content type, body)`.
    async fn serve(routes: Vec<(&'static str, &'static str, Vec<u8>)>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else { return };
                let routes = routes.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let (status, ct, body) = match routes.iter().find(|r| r.0 == path) {
                        Some((_, ct, b)) => ("200 OK", *ct, b.clone()),
                        None => ("404 Not Found", "text/plain", b"nothing here".to_vec()),
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = s.write_all(head.as_bytes()).await;
                    let _ = s.write_all(&body).await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn fetch_reads_pages_and_downloads_files() {
        let site = serve(vec![
            (
                "/page",
                "text/html",
                b"<title>Hi</title><body><h1>Hello</h1><p>World</p></body>".to_vec(),
            ),
            ("/data.json", "application/json", br#"{"a":1}"#.to_vec()),
            ("/logo.png", "image/png", vec![0x89, b'P', b'N', b'G', 0, 1, 2]),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path()).unwrap();
        ws.grants().grant(std::path::PathBuf::from(std::path::MAIN_SEPARATOR_STR), true);
        let stop = CancellationToken::new();

        let out = fetch(&json!({"url": format!("{site}/page")}), &ws, None, &stop).await.unwrap();
        assert_eq!(out, "# Hello\n\nWorld");
        let out = fetch(&json!({"url": format!("{site}/page"), "raw": true}), &ws, None, &stop)
            .await
            .unwrap();
        assert!(out.starts_with("<title>Hi</title>"), "{out}");
        let out =
            fetch(&json!({"url": format!("{site}/data.json")}), &ws, None, &stop).await.unwrap();
        assert_eq!(out, r#"{"a":1}"#);

        let err =
            fetch(&json!({"url": format!("{site}/logo.png")}), &ws, None, &stop).await.unwrap_err();
        assert!(err.contains("save_to"), "{err}");
        let out =
            fetch(&json!({"url": format!("{site}/logo.png"), "save_to": "dl/"}), &ws, None, &stop)
                .await
                .unwrap();
        assert!(out.contains("saved 7 bytes") && out.contains("dl/logo.png"), "{out}");
        assert_eq!(std::fs::read(dir.path().join("dl/logo.png")).unwrap().len(), 7);
        fetch(&json!({"url": format!("{site}/page"), "save_to": "p.html"}), &ws, None, &stop)
            .await
            .unwrap();
        assert!(std::fs::read_to_string(dir.path().join("p.html")).unwrap().contains("<h1>Hello"));

        let err =
            fetch(&json!({"url": format!("{site}/nope")}), &ws, None, &stop).await.unwrap_err();
        assert!(err.contains("404") && err.contains("gone"), "{err}");
        assert!(fetch(&json!({}), &ws, None, &stop).await.unwrap_err().contains("url"));
        assert!(fetch(&json!({"url": "ftp://x/y"}), &ws, None, &stop).await.is_err());
    }

    #[tokio::test]
    async fn a_prompt_is_answered_by_the_title_model() {
        use crate::agent::native::{Model, Shared};
        use crate::titles::{TitleModel, Titles, fake};

        let site = serve(vec![(
            "/page",
            "text/html",
            b"<title>Hi</title><body><h1>Hello</h1><p>World of widgets</p></body>".to_vec(),
        )])
        .await;
        let (url, seen) = fake::server(vec![
            (200, fake::completion("<think>hm</think>A page greeting the world of widgets.")),
            (500, "{}".into()),
        ]);
        let titles = Titles::new(
            TitleModel { url, model: "small".into(), ..TitleModel::default() },
            Shared::new(Model {
                url: "http://unused".into(),
                model: "big".into(),
                ..Model::default()
            }),
        );
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path()).unwrap();
        let stop = CancellationToken::new();
        let args = json!({"url": format!("{site}/page"), "prompt": "what is sold?"});

        let out = fetch(&args, &ws, Some(&titles), &stop).await.unwrap();
        assert!(out.starts_with("From http://"), "{out}");
        assert!(out.ends_with("A page greeting the world of widgets."), "{out}");
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen[0]["model"], "small", "the title model answers, not the main one");
            let ask = seen[0]["messages"][1]["content"].as_str().unwrap();
            assert!(
                ask.starts_with("Request: what is sold?\n\nPage:\n\n# Hello\n\nWorld of widgets"),
                "{ask}"
            );
        }

        // a failing title model is explained, and the page can still be read
        let err = fetch(&args, &ws, Some(&titles), &stop).await.unwrap_err();
        assert!(err.contains("could not read") && err.contains("without prompt"), "{err}");
        let err = fetch(&args, &ws, None, &stop).await.unwrap_err();
        assert!(err.contains("no title model"), "{err}");
        let save = json!({"url": format!("{site}/page"), "prompt": "x", "save_to": "x"});
        assert!(fetch(&save, &ws, Some(&titles), &stop).await.unwrap_err().contains("save_to"));
    }

    #[tokio::test]
    #[ignore = "reaches the internet"]
    async fn live_search_and_fetch() {
        let stop = CancellationToken::new();
        let out = search(&json!({"query": "rust programming language book", "limit": 3}), &stop)
            .await
            .unwrap();
        println!("{out}");
        assert!(out.starts_with("1. "), "{out}");
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path()).unwrap();
        let page =
            fetch(&json!({"url": "https://example.com", "max_chars": 500}), &ws, None, &stop)
                .await
                .unwrap();
        println!("{page}");
        assert!(page.contains("Example Domain"));
    }
}
