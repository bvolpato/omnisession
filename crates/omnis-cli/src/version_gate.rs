pub(crate) fn is_at_least(version: &str, minimum: &str) -> bool {
    numeric_triplet(version)
        .zip(numeric_triplet(minimum))
        .is_some_and(|(version, minimum)| version >= minimum)
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
    fn rejects_older_and_malformed_versions() {
        assert!(!is_at_least("0.145.9", "0.146.0"));
        assert!(!is_at_least("0.146", "0.146.0"));
        assert!(!is_at_least("current", "0.146.0"));
    }
}
