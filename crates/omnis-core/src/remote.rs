use std::{net::Ipv6Addr, path::Path, str, sync::LazyLock};

use omnis_ir::Provider;
use regex::Regex;

use crate::{CaptureError, fingerprint, git_command, trim_trailing_newlines};

/// Repository fingerprints from the exact Git remote and its Codex representation.
pub struct RemoteFingerprints {
    pub(super) exact: String,
    codex: Option<String>,
    exact_uses_ssh_home: bool,
    codex_uses_ssh_home: bool,
}

impl RemoteFingerprints {
    pub(super) fn new(remote: &[u8]) -> Self {
        let sanitized = str::from_utf8(remote).ok().and_then(codex_remote);
        Self {
            exact_uses_ssh_home: uses_implicit_ssh_home(&String::from_utf8_lossy(remote)),
            codex_uses_ssh_home: sanitized.as_deref().is_some_and(uses_implicit_ssh_home),
            exact: fingerprint(remote),
            codex: sanitized.map(|value| fingerprint(value.as_bytes())),
        }
    }

    /// Accepts the exact Git remote for every source and the sanitized URL for Codex.
    #[must_use]
    pub fn matches(&self, recorded: &str, provider: Option<Provider>) -> bool {
        self.exact == recorded
            || (provider == Some(Provider::Codex) && self.codex.as_deref() == Some(recorded))
    }

    /// Checks whether a missing source workspace can be bound to this checkout.
    /// An account-relative SSH path with no recorded user also needs a local copy of the recorded commit.
    ///
    /// # Errors
    ///
    /// Returns an error if Git cannot run the local object check.
    pub fn matches_relocated(
        &self,
        recorded: &str,
        provider: Provider,
        head: Option<&str>,
        directory: &Path,
    ) -> Result<bool, CaptureError> {
        if !self.matches(recorded, Some(provider)) {
            return Ok(false);
        }
        let needs_commit = if self.exact == recorded {
            self.exact_uses_ssh_home
        } else {
            self.codex_uses_ssh_home
        };
        if !needs_commit {
            return Ok(true);
        }
        let Some(head) = head
            .filter(|id| matches!(id.len(), 40 | 64) && id.bytes().all(|b| b.is_ascii_hexdigit()))
        else {
            return Ok(false);
        };
        let commit = format!("{head}^{{commit}}");
        let output = git_command(
            directory,
            &["rev-parse", "--verify", "--end-of-options", &commit],
        )
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .output()
        .map_err(CaptureError::Io)?;
        Ok(output.status.success()
            && trim_trailing_newlines(&output.stdout).eq_ignore_ascii_case(head.as_bytes()))
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
    let address = helper_address(remote);
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

fn helper_address(remote: &str) -> &str {
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
    address
}

fn uses_implicit_ssh_home(remote: &str) -> bool {
    // Git decodes the URL before it locates the SSH path.
    static PATH_SEPARATOR: LazyLock<Regex> =
        LazyLock::new(|| Regex::new("(?i)/|%2f").expect("SSH path separator"));
    let address = helper_address(remote);
    if let Some((scheme, rest)) = address.split_once("://")
        && !scheme.contains(['/', ':'])
    {
        if !["ssh", "git+ssh", "ssh+git"]
            .iter()
            .any(|s| scheme.eq_ignore_ascii_case(s))
        {
            return false;
        }
        return PATH_SEPARATOR.find(rest).is_some_and(|separator| {
            let path = &rest[separator.end()..];
            rest[..separator.start()]
                .rsplit_once('@')
                .is_none_or(|(user, _)| user.is_empty() || user.starts_with(':'))
                && (path.starts_with('~')
                    || path.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("%7e")))
        });
    }

    // An IPv6 host can contain colons before the SCP path separator.
    let address = address.strip_prefix('@').unwrap_or(address);
    if cfg!(windows)
        && address.as_bytes().get(1) == Some(&b':')
        && address.as_bytes()[0].is_ascii_alphabetic()
    {
        return false;
    }
    let host_path = if let Some(ipv6) = address.strip_prefix('[') {
        ipv6.split_once("]:")
    } else {
        address.split_once(':')
    };
    host_path.is_some_and(|(host, path)| {
        !host.is_empty()
            && !host.contains(['@', '/', '\\'])
            && (!path.starts_with('/') || path.starts_with("/~"))
    })
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
    fn relocating_an_account_relative_ssh_url_needs_more_than_the_remote() {
        for (remote, recorded) in [
            (
                "second@host.invalid:project.git",
                "host.invalid:project.git",
            ),
            ("host.invalid:project.git", "host.invalid:project.git"),
            ("second@[::1]:~/project.git", "[::1]:~/project.git"),
            ("hg::[::1]:/~/project.git", "hg::[::1]:/~/project.git"),
            (
                "ssh://second@host.invalid/~/project.git",
                "ssh://host.invalid/~/project.git",
            ),
            (
                "ssh://[::1]:22/%7Ealice/project.git",
                "ssh://[::1]:22/%7Ealice/project.git",
            ),
            (
                "ssh://host.invalid%2F%7E/project.git",
                "ssh://host.invalid%2F%7E/project.git",
            ),
            (
                "git+ssh://host.invalid/~/project.git",
                "git+ssh://host.invalid/~/project.git",
            ),
        ] {
            let current = RemoteFingerprints::new(remote.as_bytes());
            for head in [None, Some("HEAD"), Some("-v")] {
                assert!(
                    !current
                        .matches_relocated(
                            &fingerprint(recorded.as_bytes()),
                            Provider::Codex,
                            head,
                            Path::new(".")
                        )
                        .unwrap(),
                    "accepted {remote}"
                );
            }
        }
        for (remote, recorded) in [
            (
                "https://second@host.invalid/project.git",
                "https://host.invalid/project.git",
            ),
            ("second@[::1]:/srv/project.git", "[::1]:/srv/project.git"),
            (
                "ssh://second@host.invalid/srv/project.git",
                "ssh://host.invalid/srv/project.git",
            ),
            (
                "first@host.invalid:project.git",
                "first@host.invalid:project.git",
            ),
            (
                "git@host.invalid:project.git",
                "git@host.invalid:project.git",
            ),
        ] {
            assert!(
                RemoteFingerprints::new(remote.as_bytes())
                    .matches_relocated(
                        &fingerprint(recorded.as_bytes()),
                        Provider::Codex,
                        None,
                        Path::new(".")
                    )
                    .unwrap(),
                "rejected {remote}"
            );
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
