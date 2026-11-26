use std::{
    collections::HashMap,
    fs,
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    time::{Duration, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use omnis_ir::Provider;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const MAX_CACHED_VERSIONS: usize = 64;

static PREPROBED_VERSIONS: OnceLock<Mutex<HashMap<(Provider, PathBuf), String>>> = OnceLock::new();

fn preprobed_versions() -> &'static Mutex<HashMap<(Provider, PathBuf), String>> {
    PREPROBED_VERSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Clears any in-process preprobed versions so retries always probe fresh.
pub(crate) fn clear_preprobed() {
    if let Ok(mut map) = preprobed_versions().lock() {
        map.clear();
    }
}

/// Probes `binary` ahead of time on a background thread while another probe runs concurrently.
pub(crate) fn preprobe_version(binary: &Path, provider: Provider) {
    if let Ok(version) = installed_version_uncached_in_process(binary, provider)
        && let Ok(mut map) = preprobed_versions().lock()
    {
        map.insert((provider, binary.to_path_buf()), version);
    }
}

/// Bound on a provider's `--version` probe. The first launch after a provider update can take
/// several seconds on macOS, and a timeout sends the transfer to semantic handoff.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Codex installs helper shims before parsing `--help` or `--version` on its first run.
pub(crate) fn isolate_first_codex_probe(
    provider: Provider,
    command: &mut Command,
) -> Result<Option<tempfile::TempDir>> {
    if provider != Provider::Codex {
        return Ok(None);
    }
    if let Ok(home) = std::env::var("CODEX_HOME")
        && !home.is_empty()
    {
        let path = Path::new(&home);
        if path.is_relative() {
            command.env(
                "CODEX_HOME",
                omnis_adapters::CodexAdapter::absolute_home(path)?,
            );
        }
        return Ok(None);
    }
    let first_run = omnis_adapters::CodexAdapter::default_home().is_some_and(|home| {
        home.symlink_metadata()
            .is_err_and(|error| error.kind() == ErrorKind::NotFound)
    });
    if !first_run {
        return Ok(None);
    }
    let home = crate::codex_import::temporary_home()
        .context("creating a temporary Codex home for the capability check")?;
    command.env("CODEX_HOME", home.path());
    Ok(Some(home))
}

/// Reads the installed provider version with one deadline for the launcher and its workers.
pub(crate) fn installed_version(binary: &Path, provider: Provider) -> Result<String> {
    if let Ok(mut map) = preprobed_versions().lock()
        && let Some(version) = map.remove(&(provider, binary.to_path_buf()))
    {
        return Ok(version);
    }
    installed_version_uncached_in_process(binary, provider)
}

fn installed_version_uncached_in_process(binary: &Path, provider: Provider) -> Result<String> {
    let cache_path = version_cache_path();
    let cache_key = cache_path
        .as_deref()
        .and_then(|_| native_binary_cache_key(binary, provider));
    let mut cache = if cache_key.is_some() {
        cache_path.as_deref().map_or_else(|| json!({}), read_cache)
    } else {
        json!({})
    };
    if let Some(key) = &cache_key
        && let Some(version) = cache.get(key).and_then(Value::as_str)
    {
        return Ok(version.to_owned());
    }
    let version = command_version(
        crate::provider_launch::provider_process(binary)?,
        binary,
        provider,
    )?;
    if let (Some(key), Some(path), Some(entries)) =
        (cache_key, cache_path.as_deref(), cache.as_object_mut())
    {
        if entries.len() >= MAX_CACHED_VERSIONS {
            entries.clear();
        }
        entries.insert(key, json!(version));
        let _ = write_cache(path, &cache);
    }
    Ok(version)
}

fn version_cache_path() -> Option<PathBuf> {
    omnis_store::state_root()
        .ok()
        .map(|root| root.join("cache").join("provider-versions.json"))
}

/// Whether the resolved file is an install of the command itself. A versioned install is named after
/// its command or its version, such as `versions/2.1.220`. Another name can be a dispatcher, whose
/// own file does not change when the program that it selects does.
pub(crate) fn is_named_install(binary: &Path, canonical: &Path) -> bool {
    let name = |value: Option<&std::ffi::OsStr>| {
        value.and_then(std::ffi::OsStr::to_str).map(str::to_owned)
    };
    name(canonical.file_name())
        .zip(name(binary.file_stem()))
        .is_some_and(|(resolved, command)| {
            resolved.starts_with(&command)
                || resolved.starts_with(|first: char| first.is_ascii_digit())
        })
}

/// Only cache compiled native executables (never `#!` scripts, Windows command shims, or links to a
/// dispatcher whose downstream target can change independently of the wrapper file).
fn native_binary_cache_key(binary: &Path, provider: Provider) -> Option<String> {
    let canonical = fs::canonicalize(binary).ok()?;
    if !is_named_install(binary, &canonical) {
        return None;
    }
    #[cfg(windows)]
    if canonical.extension().is_some_and(|ext| {
        ext.eq_ignore_ascii_case("cmd")
            || ext.eq_ignore_ascii_case("bat")
            || ext.eq_ignore_ascii_case("ps1")
    }) {
        return None;
    }
    let mut header = [0_u8; 2];
    let mut file = fs::File::open(&canonical).ok()?;
    if file.read(&mut header).ok()? >= 2 && &header == b"#!" {
        return None;
    }
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    let mut digest = Sha256::new();
    digest.update(env!("CARGO_PKG_VERSION").as_bytes());
    digest.update([0]);
    digest.update(provider.to_string().as_bytes());
    digest.update([0]);
    let path_bytes = canonical.as_os_str().as_encoded_bytes();
    digest.update(path_bytes.len().to_le_bytes());
    digest.update(path_bytes);
    digest.update(metadata.len().to_le_bytes());
    digest.update(modified.as_nanos().to_le_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        digest.update(metadata.dev().to_le_bytes());
        digest.update(metadata.ino().to_le_bytes());
        digest.update(metadata.ctime().to_le_bytes());
        digest.update(metadata.ctime_nsec().to_le_bytes());
    }
    Some(hex::encode(digest.finalize()))
}

fn read_cache(path: &Path) -> Value {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|mut cache| cache.get_mut("versions_v1").map(Value::take))
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn write_cache(path: &Path, cache: &Value) -> Result<()> {
    let directory = path.parent().context("version cache has no directory")?;
    fs::create_dir_all(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut temporary, &json!({ "versions_v1": cache }))?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

pub(crate) fn command_version(
    mut command: Command,
    binary: &Path,
    provider: Provider,
) -> Result<String> {
    let name = crate::transfer::provider_name(provider);
    let limit = match provider {
        Provider::Antigravity | Provider::Pi | Provider::OhMyPi => 8 * 1024,
        _ => 1024 * 1024,
    };
    let parse = version_parser(provider);
    let _home = isolate_first_codex_probe(provider, &mut command)?;
    let output = crate::interrupt::with_provider_probes(|| {
        omnis_adapters::bounded_provider_line_output(
            command.arg("--version").stdin(Stdio::null()),
            PROBE_TIMEOUT,
            limit,
            move |line| parse(line).is_some(),
        )
    })
    .with_context(|| format!("checking {name} version with `{}`", binary.display()))?;
    if !output.status.success() {
        bail!("{name} version probe exited with status {}", output.status);
    }
    let text = String::from_utf8(output.stdout)
        .with_context(|| format!("{name} version was not UTF-8"))?;
    parse(&text).with_context(|| format!("{name} returned an unrecognized version"))
}

fn version_parser(provider: Provider) -> fn(&str) -> Option<String> {
    match provider {
        Provider::CursorCli => crate::cursor_import::parse_version,
        Provider::OhMyPi => |text| {
            text.lines()
                .map(str::trim)
                .find_map(|line| find_version(line.strip_prefix("omp/").unwrap_or(line)))
        },
        _ => find_version,
    }
}

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

    #[cfg(unix)]
    #[test]
    fn native_binary_cache_skips_dispatchers_but_not_versioned_installs() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        for (target, link, cached) in [
            ("mise", "codex", false),
            ("2.1.220", "claude", true),
            ("grok-0.2.117", "grok", true),
        ] {
            fs::write(dir.path().join(target), b"\x7fELFfake-binary").expect("write binary");
            symlink(target, dir.path().join(link)).expect("link binary");
            assert_eq!(
                native_binary_cache_key(&dir.path().join(link), Provider::Codex).is_some(),
                cached,
                "{link}"
            );
        }
    }

    #[test]
    fn omp_version_is_read_past_a_launcher_banner() {
        for output in ["omp/16.4.1\n", "Now using node v22.3.0\nomp/16.4.1\n"] {
            assert_eq!(
                version_parser(Provider::OhMyPi)(output).as_deref(),
                Some("16.4.1"),
                "{output}"
            );
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

    #[test]
    fn native_binary_cache_skips_scripts_and_invalidates_on_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("wrapper");
        fs::write(&script, b"#!/bin/sh\necho 1.0.0\n").expect("write script");
        assert!(native_binary_cache_key(&script, Provider::Antigravity).is_none());

        let binary = dir.path().join("native-bin");
        fs::write(&binary, b"\x7fELFfake-binary-v1").expect("write binary");
        let first =
            native_binary_cache_key(&binary, Provider::Antigravity).expect("native binary key");
        let other_provider =
            native_binary_cache_key(&binary, Provider::Codex).expect("codex binary key");
        assert_ne!(first, other_provider);

        fs::write(&binary, b"\x7fELFfake-binary-v2-longer").expect("update binary");
        let updated =
            native_binary_cache_key(&binary, Provider::Antigravity).expect("updated binary key");
        assert_ne!(first, updated);
    }
}
