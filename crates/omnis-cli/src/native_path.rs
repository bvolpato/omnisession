use std::path::Path;

use anyhow::{Context, Result, bail};

pub(crate) struct FileUriParts {
    pub external: String,
    pub fs_path: String,
    pub uri_path: String,
}

pub(crate) fn file_uri(path: &Path) -> Result<String> {
    Ok(file_uri_parts(path)?.external)
}

pub(crate) fn file_uri_parts(path: &Path) -> Result<FileUriParts> {
    #[cfg(windows)]
    {
        windows_file_uri_parts(path)
    }
    #[cfg(not(windows))]
    {
        if !path.is_absolute() {
            bail!("file URI requires an absolute path");
        }
        let path = path.to_str().context("file URI requires a UTF-8 path")?;
        Ok(FileUriParts {
            external: format!("file://{}", percent_encode_path(path)),
            fs_path: path.to_owned(),
            uri_path: path.to_owned(),
        })
    }
}

#[cfg(any(not(target_os = "linux"), test))]
pub(crate) fn path_from_file_uri(uri: &str) -> Option<std::path::PathBuf> {
    let encoded = uri.strip_prefix("file://")?;
    let decoded = percent_decode(encoded)?;
    #[cfg(windows)]
    {
        windows_path_from_file_uri(&decoded)
    }
    #[cfg(not(windows))]
    {
        decoded
            .starts_with('/')
            .then(|| std::path::PathBuf::from(decoded))
    }
}

#[cfg(windows)]
fn windows_file_uri_parts(path: &Path) -> Result<FileUriParts> {
    use std::path::{Component, Prefix};

    let mut components = path.components();
    let drive = match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
            Prefix::UNC(..) | Prefix::VerbatimUNC(..) => {
                bail!("UNC paths are not supported for native materialization")
            }
            Prefix::DeviceNS(_) | Prefix::Verbatim(_) => {
                bail!("device paths are not supported for native materialization")
            }
        },
        _ => bail!("Windows file URI requires an absolute drive path"),
    };
    if !matches!(components.next(), Some(Component::RootDir)) {
        bail!("Windows file URI requires an absolute drive path");
    }

    let mut segments = Vec::new();
    for component in components {
        match component {
            Component::Normal(segment) => {
                segments.push(segment.to_str().context("file URI requires a UTF-8 path")?);
            }
            Component::CurDir => {}
            Component::ParentDir => bail!("file URI path must be normalized"),
            Component::Prefix(_) | Component::RootDir => {
                bail!("file URI path contains an unexpected root")
            }
        }
    }

    let drive = char::from(drive);
    let suffix = segments.join("/");
    let uri_path = if suffix.is_empty() {
        format!("/{drive}:/")
    } else {
        format!("/{drive}:/{suffix}")
    };
    let fs_path = if suffix.is_empty() {
        format!("{drive}:\\")
    } else {
        format!("{drive}:\\{}", segments.join("\\"))
    };
    Ok(FileUriParts {
        external: format!("file://{}", percent_encode_path(&uri_path)),
        fs_path,
        uri_path,
    })
}

#[cfg(windows)]
fn windows_path_from_file_uri(decoded: &str) -> Option<std::path::PathBuf> {
    let path = decoded
        .strip_prefix('/')
        .filter(|path| path.as_bytes().get(1) == Some(&b':'))
        .unwrap_or(decoded);
    if path.starts_with("//") || path.starts_with(r"\\") {
        return None;
    }
    let bytes = path.as_bytes();
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || !matches!(bytes[2], b'/' | b'\\')
    {
        return None;
    }
    Some(std::path::PathBuf::from(path.replace('/', r"\")))
}

fn percent_encode_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b':' | b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(encoded, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }
    encoded
}

#[cfg(any(not(target_os = "linux"), test))]
fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex_value(*bytes.get(index + 1)?)?;
            let low = hex_value(*bytes.get(index + 2)?)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

#[cfg(any(not(target_os = "linux"), test))]
const fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    #[test]
    fn posix_file_uri_encodes_reserved_and_unicode_bytes() {
        let parts =
            file_uri_parts(Path::new("/workspace/Zoë #100%")).expect("POSIX file URI parts");
        assert_eq!(parts.external, "file:///workspace/Zo%C3%AB%20%23100%25");
        assert_eq!(parts.fs_path, "/workspace/Zoë #100%");
        assert_eq!(parts.uri_path, "/workspace/Zoë #100%");
        assert_eq!(
            path_from_file_uri(&parts.external).as_deref(),
            Some(Path::new("/workspace/Zoë #100%"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_file_uri_strips_verbatim_prefix_and_encodes_reserved_bytes() {
        let parts = file_uri_parts(Path::new(r"\\?\C:\Users\Zoë Dev\100%#repo"))
            .expect("Windows file URI parts");
        assert_eq!(parts.fs_path, r"C:\Users\Zoë Dev\100%#repo");
        assert_eq!(parts.uri_path, "/C:/Users/Zoë Dev/100%#repo");
        assert_eq!(
            parts.external,
            "file:///C:/Users/Zo%C3%AB%20Dev/100%25%23repo"
        );
        assert_eq!(
            path_from_file_uri(&parts.external).as_deref(),
            Some(Path::new(r"C:\Users\Zoë Dev\100%#repo"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_file_uri_rejects_unc_materialization() {
        let error = file_uri(Path::new(r"\\server\share\workspace"))
            .expect_err("UNC materialization must fail closed");
        assert!(error.to_string().contains("UNC paths are not supported"));
        assert!(path_from_file_uri("file://server/share/workspace").is_none());
    }
}
