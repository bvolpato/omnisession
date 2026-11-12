use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
};

use anyhow::{Context, Result, anyhow, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};

use super::Config;

const MAX_DECODED: usize = 4 * 1024 * 1024;
const MAX_ENCODED: usize = MAX_DECODED.div_ceil(3) * 4;

#[derive(Clone, Copy, Debug)]
pub(in super::super) enum Preference {
    Config,
    Requirements,
}

impl Preference {
    #[cfg(target_os = "macos")]
    fn key(self) -> &'static str {
        match self {
            Self::Config => "config_toml_base64",
            Self::Requirements => "requirements_toml_base64",
        }
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Config => "configuration",
            Self::Requirements => "requirements",
        }
    }
}

#[derive(Debug)]
pub(in super::super) struct MacPreferences {
    read: fn(Preference) -> Result<Option<String>>,
    cache: Mutex<[Option<Projection>; 2]>,
}

#[derive(Debug)]
struct Projection {
    home: PathBuf,
    digest: [u8; 32],
    config: Arc<Config>,
}

impl MacPreferences {
    #[cfg(target_os = "macos")]
    pub(in super::super) fn system() -> Self {
        Self::new(forced_value)
    }

    #[cfg(any(target_os = "macos", test))]
    pub(super) fn new(read: fn(Preference) -> Result<Option<String>>) -> Self {
        Self {
            read,
            cache: Mutex::default(),
        }
    }

    pub(super) fn config(&self, home: &Path, key: Preference) -> Result<Option<Arc<Config>>> {
        let Some(encoded) = (self.read)(key)? else {
            return Ok(None);
        };
        ensure!(
            encoded.len() <= MAX_ENCODED,
            "managed preference exceeds the 4 MiB decoded limit"
        );
        let encoded = encoded.trim();
        let digest: [u8; 32] = Sha256::digest(encoded.as_bytes()).into();
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        let slot = &mut cache[key as usize];
        if let Some(previous) = slot
            && previous.home == home
            && previous.digest == digest
        {
            return Ok(Some(Arc::clone(&previous.config)));
        }
        let decoded = STANDARD
            .decode(encoded)
            .map_err(|_| anyhow!("managed preference must be base64"))?;
        ensure!(
            decoded.len() <= MAX_DECODED,
            "managed preference exceeds the 4 MiB decoded limit"
        );
        let text =
            std::str::from_utf8(&decoded).context("managed preference must contain UTF-8")?;
        let config = Config::parse(text, home, matches!(key, Preference::Requirements))?;
        *slot = Some(Projection {
            home: home.to_owned(),
            digest,
            config: Arc::clone(&config),
        });
        Ok(Some(config))
    }
}

#[cfg(target_os = "macos")]
fn forced_value(preference: Preference) -> Result<Option<String>> {
    use objc2_core_foundation::{
        CFPreferencesAppValueIsForced, CFPreferencesCopyAppValue, CFString,
    };

    let domain = CFString::from_str("com.openai.codex");
    let key = CFString::from_str(preference.key());
    if !CFPreferencesAppValueIsForced(&key, &domain) {
        return Ok(None);
    }
    let value = CFPreferencesCopyAppValue(&key, &domain);
    // CopyAppValue can fall back to a user default if the administrator removes the policy.
    if !CFPreferencesAppValueIsForced(&key, &domain) {
        return Ok(None);
    }
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value
        .downcast_ref::<CFString>()
        .context("managed preference must be a string")?;
    ensure!(
        usize::try_from(value.length())? <= MAX_ENCODED,
        "managed preference exceeds the 4 MiB decoded limit"
    );
    Ok(Some(value.to_string()))
}
