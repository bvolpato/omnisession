use std::{net::Ipv6Addr, str, sync::LazyLock};

use omnis_ir::Provider;
use regex::Regex;

use crate::fingerprint;

/// Repository fingerprints from the exact Git remote and its Codex representation.
pub struct RemoteFingerprints {
    pub(super) exact: String,
    codex: Option<String>,
}

impl RemoteFingerprints {
    pub(super) fn new(remote: &[u8]) -> Self {
        Self {
            exact: fingerprint(remote),
            codex: str::from_utf8(remote)
                .ok()
                .and_then(codex_remote)
                .map(|value| fingerprint(value.as_bytes())),
        }
    }

    /// Accepts the exact Git remote for every source and the sanitized URL for Codex.
    #[must_use]
    pub fn matches(&self, recorded: &str, provider: Option<Provider>) -> bool {
        self.exact == recorded
            || (provider == Some(Provider::Codex) && self.codex.as_deref() == Some(recorded))
    }
}

// Codex removes userinfo from URL authorities and SCP-style SSH remotes. Keep the
// original bytes for the helper, transport, host, port, path, query and fragment.
// Only recognized addresses get an alternate fingerprint. All others use exact matching.
fn codex_remote(remote: &str) -> Option<String> {
    static ADDRESS: LazyLock<Regex> = LazyLock::new(|| {
        let host = r"(?P<host>[a-zA-Z0-9._-]+|\[[0-9a-fA-F:.]+\])";
        let userinfo = r"(?P<userinfo>(?:[a-zA-Z0-9._~!$&'()*+,;=:-]|%[0-9a-fA-F]{2})+)";
        let pattern = format!(
            r"^(?P<scheme>https?|ssh|git|file)://{userinfo}@{host}(?::(?P<port>\d+))?(?:/|$)"
        );
        Regex::new(&pattern).expect("Git URL pattern")
    });
    static SCP: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(?P<userinfo>[a-zA-Z0-9._-]+)@(?P<host>[a-zA-Z0-9._-]+|\[[0-9a-fA-F:.]+\]):.")
            .expect("SCP-style Git remote pattern")
    });

    if !remote.contains('@') {
        return None;
    }
    let mut address = remote;
    while let Some((helper, nested)) = address.split_once("::") {
        if helper.is_empty()
            || !helper
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
        {
            break;
        }
        address = nested;
    }
    if address.len() != remote.len() && address.chars().any(char::is_whitespace) {
        return None;
    }
    let captures = ADDRESS
        .captures(address)
        .or_else(|| SCP.captures(address))?;
    let host = captures.name("host")?.as_str();
    if host.starts_with('[') {
        host[1..host.len() - 1].parse::<Ipv6Addr>().ok()?;
    }
    if let Some(port) = captures.name("port") {
        port.as_str().parse::<u16>().ok()?;
    }
    let userinfo = captures.name("userinfo")?;
    let ssh = captures
        .name("scheme")
        .is_none_or(|scheme| scheme.as_str() == "ssh");
    let user = userinfo.as_str().split(':').next()?;
    if ssh && userinfo.as_str() == "git" {
        return None;
    }
    let prefix = remote.len() - address.len() + userinfo.start();
    let retained_user = if ssh && user == "git" { "git@" } else { "" };
    Some(format!(
        "{}{retained_user}{}",
        &remote[..prefix],
        &address[userinfo.end() + 1..]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_codex_userinfo_and_legacy_exact_fingerprints() {
        for (raw, recorded) in [
            (
                "https://reader@example.invalid:8443/a%2Fb.git?ref=one#two",
                "https://example.invalid:8443/a%2Fb.git?ref=one#two",
            ),
            (
                "hg::hg::https://ci%40example.invalid@example.invalid/a.git",
                "hg::hg::https://example.invalid/a.git",
            ),
            (
                "ssh://reader@example.invalid/a.git",
                "ssh://example.invalid/a.git",
            ),
            (
                "ssh://git:example@example.invalid/a.git",
                "ssh://git@example.invalid/a.git",
            ),
            (
                "reader@[2001:db8::1]:dir name/proj.git",
                "[2001:db8::1]:dir name/proj.git",
            ),
        ] {
            let current = RemoteFingerprints::new(raw.as_bytes());
            assert!(current.matches(&fingerprint(raw.as_bytes()), Some(Provider::Codex)));
            assert!(current.matches(&fingerprint(recorded.as_bytes()), Some(Provider::Codex)));
            assert!(!current.matches(&fingerprint(recorded.as_bytes()), Some(Provider::Claude)));
        }
    }

    #[test]
    fn requires_exact_transport_host_port_and_repository() {
        let current =
            RemoteFingerprints::new(b"https://reader@example.invalid:8443/a%2Fb.git?ref=one#two");
        for other in [
            "http://example.invalid:8443/a%2Fb.git?ref=one#two",
            "https://other.invalid:8443/a%2Fb.git?ref=one#two",
            "https://example.invalid:443/a%2Fb.git?ref=one#two",
            "https://example.invalid:8443/a/b.git?ref=one#two",
            "https://example.invalid:8443/a%2Fb.git?ref=three#two",
            "https://example.invalid:8443/a%2Fb.git?ref=one",
            "https://other@example.invalid:8443/a%2Fb.git?ref=one#two",
        ] {
            assert!(!current.matches(&fingerprint(other.as_bytes()), Some(Provider::Codex)));
        }
        for raw in [
            "git@example.invalid:org/repo.git",
            "ssh://git@example.invalid/org/repo.git",
            "https://example.invalid/path?ref=reader@other.invalid",
            "dir/reader@example.invalid:project",
            "https://reader@[bad::bad::bad]/project",
            "ext::ssh reader@example.invalid:/project",
        ] {
            assert!(codex_remote(raw).is_none());
        }
    }
}
