use std::net::{IpAddr, SocketAddr, UdpSocket};

#[derive(Debug, PartialEq)]
pub struct Found {
    pub url: String,
    pub guessed: bool,
}

pub fn route_address() -> Option<IpAddr> {
    let probe = |bind: &str, to: &str| {
        let s = UdpSocket::bind(bind).ok()?;
        s.connect(to).ok()?;
        let ip = s.local_addr().ok()?.ip();
        (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
    };
    probe("0.0.0.0:0", "192.0.2.1:80").or_else(|| probe("[::]:0", "[2001:db8::1]:80"))
}

pub fn url_for(given: Option<&str>, listen: SocketAddr, route: impl FnOnce() -> Option<IpAddr>) -> Result<Found, String> {
    if let Some(url) = given.map(str::trim).filter(|u| !u.is_empty()) {
        let url = url.trim_end_matches('/');
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(format!("--url {url}: give the whole address, like http://{url}"));
        }
        return Ok(Found { url: url.to_string(), guessed: false });
    }
    let ip = listen.ip();
    if ip.is_loopback() {
        return Err(format!(
            "this server listens on {listen}, which only this machine can reach. Start it with --listen 0.0.0.0:{} or pass --url with the address other machines use",
            listen.port()
        ));
    }
    let (ip, guessed) = if ip.is_unspecified() {
        match route() {
            Some(found) => (found, true),
            None => return Err("could not find this machine's network address: pass --url with the address other machines use, like --url http://10.0.0.2:7420".into()),
        }
    } else {
        (ip, false)
    };
    Ok(Found { url: format!("http://{}", SocketAddr::new(ip, listen.port())), guessed })
}

pub fn text(name: &str, found: &Found, token: &str) -> String {
    let link = agentbench_core::pairing::Pairing { url: found.url.clone(), token: token.to_string(), name: name.to_string() }.link();
    let mut out = format!("name   {name}\nurl    {}\ntoken  {token}\n", found.url);
    if found.guessed {
        out.push_str("\nThe url is this machine's own address. In a container or behind NAT or a proxy, pass --url with the address the window uses.\n");
    }
    out.push_str(&format!("\nIn the window, open Add a workspace and paste this into the address field:\n{link}\n"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentbench_core::pairing::Pairing;

    fn at(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn no_route() -> Option<IpAddr> {
        None
    }

    #[test]
    fn a_given_url_wins_over_the_listen_address() {
        let f = url_for(Some(" https://ab.example.com/ "), at("0.0.0.0:7420"), || panic!("not asked")).unwrap();
        assert_eq!(f, Found { url: "https://ab.example.com".into(), guessed: false });
        assert!(url_for(Some("10.0.0.2:7420"), at("0.0.0.0:7420"), no_route).unwrap_err().contains("http://10.0.0.2:7420"));
    }

    #[test]
    fn an_unspecified_listen_address_takes_this_machines_address() {
        let f = url_for(None, at("0.0.0.0:7420"), || Some("10.100.2.73".parse().unwrap())).unwrap();
        assert_eq!(f, Found { url: "http://10.100.2.73:7420".into(), guessed: true });
        let f = url_for(None, at("[::]:9000"), || Some("fd00::5".parse().unwrap())).unwrap();
        assert_eq!(f.url, "http://[fd00::5]:9000");
        let f = url_for(None, at("10.100.2.73:7420"), || panic!("not asked")).unwrap();
        assert_eq!(f, Found { url: "http://10.100.2.73:7420".into(), guessed: false });
    }

    #[test]
    fn with_no_address_to_offer_it_asks_for_url() {
        assert!(url_for(None, at("127.0.0.1:7420"), no_route).unwrap_err().contains("--url"));
        assert!(url_for(None, at("0.0.0.0:7420"), no_route).unwrap_err().contains("--url"));
        assert!(url_for(Some("  "), at("0.0.0.0:7420"), no_route).is_err());
    }

    #[test]
    fn the_printout_has_the_token_and_a_link_the_window_reads_back() {
        let f = Found { url: "http://10.100.2.73:7420".into(), guessed: true };
        let out = text("i9", &f, "0123abcd");
        assert!(out.contains("name   i9\n") && out.contains("url    http://10.100.2.73:7420\n") && out.contains("token  0123abcd\n"), "{out}");
        assert!(out.contains("pass --url"), "{out}");
        let link = out.lines().last().unwrap();
        assert_eq!(
            Pairing::parse(link),
            Some(Pairing { url: "http://10.100.2.73:7420".into(), token: "0123abcd".into(), name: "i9".into() })
        );
        let sure = text("i9", &Found { guessed: false, ..f }, "t");
        assert!(!sure.contains("pass --url"), "{sure}");
    }
}
