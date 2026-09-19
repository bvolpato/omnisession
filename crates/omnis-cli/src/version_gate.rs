pub(crate) fn is_at_least(version: &str, minimum: &str) -> bool {
    numeric_triplet(version)
        .zip(numeric_triplet(minimum))
        .is_some_and(|(version, minimum)| version >= minimum)
}

/// Like [`is_at_least`], but a pre-release of exactly the minimum (`0.146.0-alpha.1`,
/// `0.19.1rc1`, `0.19.1.dev3`) sorts before it. Build metadata and post-releases do not.
pub(crate) fn is_release_at_least(version: &str, minimum: &str) -> bool {
    let Some((triplet, minimum)) = numeric_triplet(version).zip(numeric_triplet(minimum)) else {
        return false;
    };
    triplet > minimum
        || (triplet == minimum
            && split_version(version).is_some_and(|(_, suffix)| !is_prerelease(suffix)))
}

/// First version in a provider's version output: a whitespace-separated word that starts with
/// `major.minor.patch`. A pre-release, build, or post-release suffix stays attached
/// (`0.147.0-alpha.3`, `1.2.3+build`, `0.20.0rc1`, `0.19.1.post1`). Numbers inside other words
/// are not versions: `v22.3.0` in a banner, `(1.80.0)`, a path, or an IP address.
pub(crate) fn find_version(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .map(|word| word.trim_end_matches([',', ';', ':', '.']))
        .find(|word| {
            split_version(word).is_some_and(|(_, suffix)| {
                suffix.is_empty()
                    || suffix.starts_with(['-', '+'])
                    || suffix
                        .trim_start_matches('.')
                        .starts_with(|c: char| c.is_ascii_alphabetic())
            })
        })
        .map(str::to_owned)
}

/// Splits `1.2.3-rc.1` into its `major.minor.patch` core and the suffix after it.
fn split_version(version: &str) -> Option<(&str, &str)> {
    let mut end = 0;
    for component in 0..3 {
        let digits = version[end..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count();
        if digits == 0 {
            return None;
        }
        end += digits;
        if component < 2 {
            if version.as_bytes().get(end) != Some(&b'.') {
                return None;
            }
            end += 1;
        }
    }
    Some(version.split_at(end))
}

fn is_prerelease(suffix: &str) -> bool {
    !(suffix.is_empty() || suffix.starts_with('+') || suffix.starts_with(".post"))
}

fn numeric_triplet(version: &str) -> Option<(u64, u64, u64)> {
    let (core, suffix) = split_version(version)?;
    // A fourth numeric component is not a version this gate understands.
    if suffix.starts_with('.') && !suffix[1..].starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    let mut parts = core.split('.');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_minimum_and_newer_versions() {
        assert!(is_at_least("0.146.0", "0.146.0"));
        assert!(is_at_least("0.146.1", "0.146.0"));
        assert!(is_at_least("1.0.0", "0.146.0"));
        assert!(is_at_least("2026.07.24-a1b2c3d", "2026.07.23"));
    }

    #[test]
    fn finds_version_word_and_ignores_numbers_inside_other_words() {
        for (output, version) in [
            ("codex-cli 0.146.0\n", "0.146.0"),
            ("2.1.220 (Claude Code)", "2.1.220"),
            ("grok 0.2.117 (f1c0609308) [stable]", "0.2.117"),
            (
                "codex-cli 0.147.0-alpha.3 (rustc 1.80.0)",
                "0.147.0-alpha.3",
            ),
            ("grok 0.2.118-beta.1+build.7", "0.2.118-beta.1+build.7"),
            ("0.20.0rc1", "0.20.0rc1"),
            ("0.19.1.post1", "0.19.1.post1"),
            ("Pi version 0.82.1.", "0.82.1"),
            // Decoys before the real version must not win.
            (
                "Now using node v22.3.0 (npm v10.8.1)\ncodex-cli 0.100.0",
                "0.100.0",
            ),
            (
                "/opt/homebrew/Cellar/codex/0.150.0/bin/codex: codex-cli 0.100.0",
                "0.100.0",
            ),
            (
                "listening on 127.0.0.1:4000\ngrok 0.2.100 (abc) [stable]",
                "0.2.100",
            ),
            ("agent 10.0.0.1 1.1.7", "1.1.7"),
        ] {
            assert_eq!(find_version(output).as_deref(), Some(version), "{output}");
        }
        for output in [
            "unknown", "0.146", "build 7", "v1.2.3", "(1.2.3)", "1.2.3.4",
        ] {
            assert_eq!(find_version(output), None, "{output}");
        }
    }

    #[test]
    fn prerelease_of_the_minimum_is_older_than_the_minimum() {
        for version in [
            "0.146.0",
            "0.146.0+build.7",
            "0.146.0.post1",
            "0.146.1-alpha.1",
        ] {
            assert!(is_release_at_least(version, "0.146.0"), "{version}");
        }
        for version in [
            "0.146.0-alpha.1",
            "0.146.0rc1",
            "0.146.0.dev3",
            "0.145.9",
            "1.2",
        ] {
            assert!(!is_release_at_least(version, "0.146.0"), "{version}");
        }
    }

    #[test]
    fn rejects_older_and_malformed_versions() {
        assert!(!is_at_least("0.145.9", "0.146.0"));
        assert!(!is_at_least("0.146", "0.146.0"));
        assert!(!is_at_least("current", "0.146.0"));
    }
}
