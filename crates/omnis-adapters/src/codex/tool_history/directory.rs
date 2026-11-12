use percent_encoding::percent_decode_str;
use url::Url;

use super::json_digest;

pub(super) fn from_uri(uri: &str) -> Option<[u8; 32]> {
    let url = Url::parse(uri).ok()?;
    if url.scheme() != "file" {
        return None;
    }
    let mut components = Vec::new();
    for segment in url.path_segments()? {
        let component = percent_decode_str(segment).decode_utf8().ok()?;
        if component.contains(['/', '\0']) {
            return None;
        }
        components.push(component);
    }
    let drive = components.first().is_some_and(|s| is_drive(s));
    let windows = drive || url.host_str().is_some();
    if windows && components.iter().any(|s| s.contains('\\')) {
        return None;
    }
    normalized(
        url.host_str(),
        windows,
        components.iter().map(AsRef::as_ref),
    )
}

pub(super) fn from_path(path: &str) -> Option<[u8; 32]> {
    if path.contains('\0') {
        return None;
    }
    let ordinary = omnis_core::ordinary_windows_path(path);
    let path = ordinary.as_deref().unwrap_or(path);
    let drive = path.get(..2).is_some_and(is_drive)
        && path
            .as_bytes()
            .get(2)
            .is_some_and(|s| matches!(s, b'/' | b'\\'));
    if drive {
        return normalized(None, true, path.split(['/', '\\']));
    }
    if let Some(unc) = path.strip_prefix(r"\\").or_else(|| path.strip_prefix("//")) {
        let mut components = unc.split(['/', '\\']);
        let host = components.next()?;
        if matches!(host, "" | "." | ".." | "?") {
            return None;
        }
        return normalized(Some(host), true, components);
    }
    normalized(None, false, path.strip_prefix('/')?.split('/'))
}

fn is_drive(component: &str) -> bool {
    matches!(component.as_bytes(), [letter, b':'] if letter.is_ascii_alphabetic())
}

fn normalized<'a>(
    host: Option<&str>,
    windows: bool,
    mut components: impl Iterator<Item = &'a str>,
) -> Option<[u8; 32]> {
    let root = if windows {
        let root = components.next()?;
        if matches!(root, "" | "." | "..") {
            return None;
        }
        root.to_ascii_lowercase()
    } else {
        String::new()
    };
    let mut parts = Vec::new();
    for part in components {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            name => parts.push(name),
        }
    }
    let mut path = parts.join("/");
    if windows {
        path.make_ascii_lowercase();
    }
    Some(json_digest(&(
        host.map(str::to_ascii_lowercase),
        root,
        path,
    )))
}

#[cfg(test)]
mod tests {
    use super::{from_path, from_uri};

    #[test]
    fn recorded_directories_follow_the_source_path_convention() {
        for (uri, same, different) in [
            (
                "file:///work/Case%20File",
                "/work/./old/../Case File",
                "/work/case file",
            ),
            (
                "file:///c:/Work/Case%20File",
                r"C:\WORK\Temp\..\case file",
                r"D:\Work\case file",
            ),
            (
                "file://machine/share/Case%20File",
                r"\\MACHINE\SHARE\..\case file",
                r"\\machine\other\case file",
            ),
            ("file:///work/%5Cname", r"/work/\name", "/work/name"),
        ] {
            let expected = from_uri(uri).unwrap();
            assert_eq!(from_path(same), Some(expected), "{uri}");
            assert_ne!(from_path(different).unwrap(), expected, "{uri}");
        }
        assert_eq!(from_path(r"\\?\C:\folder"), from_uri("file:///C:/folder"));
        assert_eq!(
            from_path(r"\\?\UNC\server\share\folder"),
            from_uri("file://server/share/folder")
        );
        assert!(from_path("C:relative").is_none());
        assert!(from_uri("file:///C:/encoded%5Cseparator").is_none());
        assert!(from_uri("file:///%00/bad/path/dGVzdA").is_none());
    }
}
