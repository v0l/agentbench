//! The link that hands a phone a foreman server: its address and its token, in one string that a
//! QR code can carry. `foreman://pair?url=https%3A%2F%2Fforeman.example.com&token=…`.

const SCHEME: &str = "foreman://pair";

/// Where a server is and the token that lets a window in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pairing {
    pub url: String,
    pub token: String,
}

impl Pairing {
    /// The link, for a QR code or to paste.
    pub fn link(&self) -> String {
        format!("{SCHEME}?url={}&token={}", escape(self.url.trim()), escape(self.token.trim()))
    }

    /// Reads a link made by [`Pairing::link`]. Anything else, or a link missing either part, is
    /// not a pairing.
    pub fn parse(text: &str) -> Option<Pairing> {
        let query = text.trim().strip_prefix(SCHEME)?.strip_prefix('?')?;
        let (mut url, mut token) = (None, None);
        for pair in query.split('&') {
            let (k, v) = pair.split_once('=')?;
            match k {
                "url" => url = Some(unescape(v)?),
                "token" => token = Some(unescape(v)?),
                _ => {}
            }
        }
        let (url, token) = (url?, token?);
        (!url.trim().is_empty() && !token.trim().is_empty()).then_some(Pairing { url, token })
    }
}

/// Percent-encodes everything but the characters a URL leaves alone.
fn escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn unescape(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_carries_the_address_and_token_and_reads_back() {
        let p = Pairing { url: "https://foreman.harkin.network".into(), token: "a/b+c=d&e".into() };
        let link = p.link();
        assert!(link.starts_with("foreman://pair?url=https%3A%2F%2Fforeman.harkin.network&token="));
        assert!(!link.contains("a/b"), "the token is escaped: {link}");
        assert_eq!(Pairing::parse(&link), Some(p));
    }

    #[test]
    fn anything_else_is_not_a_pairing() {
        assert_eq!(Pairing::parse("https://foreman.example.com"), None);
        assert_eq!(Pairing::parse("foreman://pair?url=http%3A%2F%2Fx"), None);
        assert_eq!(Pairing::parse("foreman://pair?url=&token=t"), None);
        assert_eq!(Pairing::parse("foreman://pair?url=%zz&token=t"), None);
        assert_eq!(Pairing::parse(""), None);
    }

    #[test]
    fn spaces_around_a_pasted_link_are_ignored() {
        let p =
            Pairing::parse("  foreman://pair?token=t&url=http%3A%2F%2F10.0.0.2%3A7420\n").unwrap();
        assert_eq!((p.url.as_str(), p.token.as_str()), ("http://10.0.0.2:7420", "t"));
    }
}
