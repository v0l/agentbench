use crate::store::Store;
use agentbench_core::Trust;
use serde_json::Value;

pub const CACHE_MS: i64 = 3_600_000;

pub fn permission_trusted(permission: &str) -> bool {
    matches!(permission, "admin" | "write" | "maintain")
}

pub fn from_association(association: &str) -> Option<Trust> {
    match association.to_ascii_uppercase().as_str() {
        "OWNER" => Some(Trust::Trusted),
        "COLLABORATOR" | "MEMBER" => None,
        _ => Some(Trust::Untrusted),
    }
}

fn cache_key(repo: &str, user: &str) -> String {
    format!("github_perm_{}_{}", repo.to_ascii_lowercase(), user.to_ascii_lowercase())
}

fn cached(store: &Store, repo: &str, user: &str, now: i64) -> Option<Trust> {
    let raw = store.setting(&cache_key(repo, user)).ok().flatten()?;
    let (level, at) = raw.split_once('|')?;
    let at: i64 = at.parse().ok()?;
    (now - at < CACHE_MS && now >= at).then(|| Trust::parse(level))
}

fn lookup(gh: &str, repo: &str, user: &str) -> Trust {
    let url = format!("repos/{repo}/collaborators/{user}/permission");
    let r = super::sync::call(gh, &["api", &url]);
    if !r.ok {
        return Trust::Untrusted;
    }
    let permission = serde_json::from_str::<Value>(&r.stdout)
        .ok()
        .and_then(|v| v.get("permission").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();
    if permission_trusted(&permission) { Trust::Trusted } else { Trust::Untrusted }
}

pub fn resolve(store: &Store, gh: &str, repo: &str, user: &str, association: &str, now: i64) -> Trust {
    if user.is_empty() {
        return Trust::Untrusted;
    }
    if let Some(t) = from_association(association) {
        return t;
    }
    if let Some(t) = cached(store, repo, user, now) {
        return t;
    }
    let level = lookup(gh, repo, user);
    let _ = store.set_setting(&cache_key(repo, user), &format!("{}|{now}", level.as_str()));
    level
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Gh {
        dir: tempfile::TempDir,
        gh: String,
    }

    fn stub(perms: &[(&str, &str)]) -> Gh {
        let dir = tempfile::tempdir().unwrap();
        let gh = dir.path().join("gh");
        let mut script = String::from("#!/bin/sh\necho \"$@\" >> \"$(dirname \"$0\")/calls\"\nfor a in \"$@\"; do last=\"$a\"; done\ncase \"$last\" in\n");
        for (user, perm) in perms {
            script.push_str(&format!("  */collaborators/{user}/permission) echo '{{\"permission\":\"{perm}\"}}' ;;\n"));
        }
        script.push_str("  *) echo 'gh: Not Found (HTTP 404)' >&2; exit 1 ;;\nesac\n");
        crate::testing_stub::write_script(&gh, &script);
        Gh { gh: gh.to_string_lossy().into_owned(), dir }
    }

    impl Gh {
        fn calls(&self) -> usize {
            std::fs::read_to_string(self.dir.path().join("calls")).map(|c| c.lines().count()).unwrap_or(0)
        }
    }

    fn level(s: &Store, g: &Gh, user: &str, assoc: &str, now: i64) -> Trust {
        resolve(s, &g.gh, "o/r", user, assoc, now)
    }

    #[test]
    fn collaborator_permissions_map_to_trust() {
        let s = Store::memory().unwrap();
        let g = stub(&[("admin1", "admin"), ("writer", "write"), ("maint", "maintain"), ("reader", "read"), ("triager", "triage"), ("nobody", "none")]);
        for (user, want) in [
            ("admin1", Trust::Trusted),
            ("writer", Trust::Trusted),
            ("maint", Trust::Trusted),
            ("reader", Trust::Untrusted),
            ("triager", Trust::Untrusted),
            ("nobody", Trust::Untrusted),
            ("ghost", Trust::Untrusted),
        ] {
            assert_eq!(level(&s, &g, user, "COLLABORATOR", 1_000), want, "{user}");
        }
    }

    #[test]
    fn the_author_association_is_the_cheap_first_check() {
        let s = Store::memory().unwrap();
        let g = stub(&[("stranger", "admin")]);
        assert_eq!(level(&s, &g, "o", "OWNER", 0), Trust::Trusted);
        for assoc in ["NONE", "CONTRIBUTOR", "FIRST_TIME_CONTRIBUTOR", "FIRST_TIMER", "MANNEQUIN", ""] {
            assert_eq!(level(&s, &g, "stranger", assoc, 0), Trust::Untrusted, "{assoc}");
        }
        assert_eq!(g.calls(), 0, "no lookup for owners and strangers");
        assert_eq!(level(&s, &g, "stranger", "MEMBER", 0), Trust::Trusted);
        assert_eq!(g.calls(), 1);
    }

    #[test]
    fn a_lookup_is_cached_for_an_hour() {
        let s = Store::memory().unwrap();
        let g = stub(&[("writer", "write")]);
        assert_eq!(level(&s, &g, "writer", "COLLABORATOR", 1_000), Trust::Trusted);
        assert_eq!(level(&s, &g, "Writer", "COLLABORATOR", 1_000 + CACHE_MS - 1), Trust::Trusted);
        assert_eq!(g.calls(), 1);
        assert_eq!(level(&s, &g, "writer", "COLLABORATOR", 1_000 + CACHE_MS), Trust::Trusted);
        assert_eq!(g.calls(), 2);
    }

    #[test]
    fn a_failing_gh_means_untrusted() {
        let s = Store::memory().unwrap();
        assert_eq!(resolve(&s, "/nonexistent/gh", "o/r", "writer", "COLLABORATOR", 0), Trust::Untrusted);
    }
}
