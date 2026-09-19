pub(crate) fn is_at_least(version: &str, minimum: &str) -> bool {
    numeric_triplet(version)
        .zip(numeric_triplet(minimum))
        .is_some_and(|(version, minimum)| version >= minimum)
}

/// First `major.minor.patch` in a provider's version output. A `v` prefix, pre-release or build
/// suffix (`0.147.0-alpha.3`, `1.2.3+build`, `0.20.0rc1`), or fourth component keeps its leading
/// triplet, so such builds are gated like the release they belong to.
pub(crate) fn find_version(output: &str) -> Option<String> {
    output
        .split(|character: char| !character.is_ascii_digit() && character != '.')
        .find_map(|candidate| {
            let mut components = candidate.split('.');
            let triplet = [components.next()?, components.next()?, components.next()?];
            triplet
                .iter()
                .all(|component| component.parse::<u64>().is_ok())
                .then(|| triplet.join("."))
        })
}

fn numeric_triplet(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.split_once('-').map_or(version, |(core, _)| core);
    let mut parts = core.split('.');
    let triplet = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(triplet)
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
    fn finds_release_triplet_in_decorated_version_output() {
        for (output, version) in [
            ("codex-cli 0.146.0\n", "0.146.0"),
            ("2.1.220 (Claude Code)", "2.1.220"),
            ("grok 0.2.117 (f1c0609308) [stable]", "0.2.117"),
            ("Hermes Agent v0.19.1", "0.19.1"),
            ("codex-cli 0.147.0-alpha.3", "0.147.0"),
            ("grok 0.2.118-beta.1+build.7", "0.2.118"),
            ("0.20.0rc1", "0.20.0"),
            ("agy 1.2.3.4", "1.2.3"),
            ("Pi version 0.82.1.", "0.82.1"),
        ] {
            assert_eq!(find_version(output).as_deref(), Some(version), "{output}");
        }
        for output in ["unknown", "0.146", "build 7"] {
            assert_eq!(find_version(output), None, "{output}");
        }
    }

    #[test]
    fn rejects_older_and_malformed_versions() {
        assert!(!is_at_least("0.145.9", "0.146.0"));
        assert!(!is_at_least("0.146", "0.146.0"));
        assert!(!is_at_least("current", "0.146.0"));
    }
}
