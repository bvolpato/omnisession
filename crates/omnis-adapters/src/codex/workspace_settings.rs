//! Checks the Codex 0.167.1 `ThreadSettingsSnapshot` and its permission and collaboration types.
//! The [protocol definitions](https://github.com/openai/codex/tree/0a94398fa60c4fac0e7659e4213a0a36e613dd46/codex-rs/protocol/src) define the accepted fields and aliases.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

/// Returns the directory from the saved settings objects that Codex 0.167.1 can restore.
pub(super) fn saved_directory(settings: &Value) -> Option<PathBuf> {
    let cwd = native_path(&settings["cwd"])?;
    let collaboration = &settings["collaboration_mode"];
    let model = &collaboration["settings"];
    let valid = settings["model"].is_string()
        && settings["model_provider_id"].is_string()
        && optional(&settings["service_tier"], Value::is_string)
        && approval(&settings["approval_policy"])
        && one_of(
            &settings["approvals_reviewer"],
            &["user", "auto_review", "guardian_subagent"],
        )
        && permissions(&settings["permission_profile"])
        && optional(&settings["active_permission_profile"], |p| {
            p["id"].is_string() && optional(&p["extends"], Value::is_string)
        })
        && optional(&settings["runtime_workspace_roots"], native_paths)
        && optional(&settings["reasoning_effort"], effort)
        && optional(&settings["reasoning_summary"], |s| {
            one_of(s, &["auto", "concise", "detailed", "none"])
        })
        && optional(&settings["personality"], |p| {
            one_of(p, &["none", "friendly", "pragmatic"])
        })
        && one_of(
            &collaboration["mode"],
            &[
                "default",
                "plan",
                "code",
                "pair_programming",
                "execute",
                "custom",
            ],
        )
        && model["model"].is_string()
        && optional(&model["reasoning_effort"], effort)
        && optional(&model["developer_instructions"], Value::is_string)
        && settings
            .get("disabled_plugin_ids")
            .is_none_or(|v| all(v, Value::is_string));
    valid.then(|| {
        // Codex resolves parent components before the filesystem follows symlinks.
        let mut directory = PathBuf::new();
        for component in cwd.components() {
            match component {
                Component::ParentDir => {
                    directory.pop();
                }
                Component::CurDir => {}
                _ => directory.push(component.as_os_str()),
            }
        }
        directory
    })
}

fn optional(value: &Value, valid: impl FnOnce(&Value) -> bool) -> bool {
    value.is_null() || valid(value)
}

fn one_of(value: &Value, names: &[&str]) -> bool {
    value.as_str().is_some_and(|name| names.contains(&name))
}

fn all(value: &Value, valid: impl FnMut(&Value) -> bool) -> bool {
    value
        .as_array()
        .is_some_and(|values| values.iter().all(valid))
}

fn known_fields(value: &Value, names: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|map| map.keys().all(|name| names.contains(&name.as_str())))
}

fn native_path(value: &Value) -> Option<&Path> {
    let path = value.as_str().filter(|s| !s.contains('\0'))?;
    Some(Path::new(path)).filter(|p| p.is_absolute())
}

fn native_paths(value: &Value) -> bool {
    all(value, |v| native_path(v).is_some())
}

fn effort(value: &Value) -> bool {
    value.as_str().is_some_and(|s| !s.is_empty())
}

fn approval(value: &Value) -> bool {
    if one_of(value, &["untrusted", "on-request", "on-failure", "never"]) {
        return true;
    }
    let granular = &value["granular"];
    known_fields(value, &["granular"])
        && ["sandbox_approval", "rules", "mcp_elicitations"]
            .iter()
            .all(|key| granular[key].is_boolean())
        && ["skill_approval", "request_permissions"]
            .iter()
            .all(|key| granular.get(key).is_none_or(Value::is_boolean))
}

fn permissions(profile: &Value) -> bool {
    let network = &profile["network"];
    let fs = &profile["file_system"];
    match profile.get("type") {
        Some(tag) if tag == "disabled" => true,
        Some(tag) if tag == "external" => one_of(network, &["restricted", "enabled"]),
        Some(tag) if tag == "managed" => {
            one_of(network, &["restricted", "enabled"])
                && (fs["type"] == "unrestricted" || (fs["type"] == "restricted" && entries(fs)))
        }
        Some(_) => false,
        None => {
            known_fields(profile, &["network", "file_system"])
                && optional(network, |n| {
                    n.is_object() && optional(&n["enabled"], Value::is_boolean)
                })
                && optional(fs, |f| {
                    (known_fields(f, &["entries", "glob_scan_max_depth"]) && entries(f))
                        || (known_fields(f, &["read", "write"])
                            && optional(&f["read"], native_paths)
                            && optional(&f["write"], native_paths))
                })
        }
    }
}

fn entries(fs: &Value) -> bool {
    // The legacy entries field defaults to an empty list. Tagged restricted profiles require it.
    fs.get("entries").map_or_else(
        || fs.get("type").is_none(),
        |v| {
            all(v, |entry| {
                one_of(&entry["access"], &["read", "write", "deny", "none"])
                    && optional(&entry["missing_path_behavior"], |v| v == "skip")
                    && match entry["path"]["type"].as_str() {
                        Some("path") => permission_path(&entry["path"]["path"]),
                        Some("glob_pattern") => entry["path"]["pattern"].is_string(),
                        Some("special") => special_path(&entry["path"]["value"]),
                        _ => false,
                    }
            })
        },
    ) && optional(&fs["glob_scan_max_depth"], |n| {
        n.as_u64()
            .is_some_and(|n| n != 0 && usize::try_from(n).is_ok())
    })
}

fn special_path(value: &Value) -> bool {
    match value["kind"].as_str() {
        Some("root" | "minimal" | "tmpdir" | "slash_tmp") => true,
        Some("project_roots" | "current_working_directory") => {
            optional(&value["subpath"], Value::is_string)
        }
        Some("unknown") => {
            value["path"].is_string() && optional(&value["subpath"], Value::is_string)
        }
        _ => false,
    }
}

fn permission_path(value: &Value) -> bool {
    let Some(s) = value.as_str().filter(|s| !s.contains('\0')) else {
        return false;
    };
    if Path::new(s).is_absolute() {
        return true;
    }
    let bytes = s.as_bytes();
    if matches!(bytes, [drive, b':', b'/' | b'\\', ..] if drive.is_ascii_alphabetic()) {
        return true;
    }
    if !matches!(bytes, [b'/' | b'\\', b'/' | b'\\', ..]) {
        return s.starts_with('/');
    }
    let mut components = s[2..].split(['/', '\\']);
    matches!((components.next(), components.next()), (Some(host), Some(share))
        if !host.is_empty() && !share.is_empty() && !matches!(host, "?" | "."))
}
