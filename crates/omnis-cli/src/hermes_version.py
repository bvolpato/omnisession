#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Probe the selected Hermes environment without running its launcher."""

import argparse
import importlib.metadata
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import sysconfig
import tempfile
from urllib.parse import unquote, urlparse


ENTRY_POINT = "hermes_cli.main:main"
UPSTREAM_URLS = {
    "https://github.com/NousResearch/hermes-agent",
    "https://github.com/NousResearch/hermes-agent.git",
    "git@github.com:NousResearch/hermes-agent.git",
    "ssh://git@github.com/NousResearch/hermes-agent.git",
}


def git(source, *args):
    result = subprocess.run(
        ["git", "--no-optional-locks", "-c", "core.fsmonitor=false", "-C", str(source), *args],
        check=True,
        capture_output=True,
        text=True,
        timeout=3,
    )
    return result.stdout.strip()


def runtime_version(distribution, expected_commit, expected_tag):
    direct_url = json.loads(distribution.read_text("direct_url.json") or "{}")
    if not isinstance(direct_url, dict) or not isinstance(direct_url.get("dir_info", {}), dict):
        raise RuntimeError("Hermes editable-source metadata is malformed")
    parsed = urlparse(direct_url.get("url", ""))
    if (
        direct_url.get("dir_info", {}).get("editable") is not True
        or parsed.scheme != "file"
        or parsed.netloc not in ("", "localhost")
        or not Path(unquote(parsed.path)).is_absolute()
    ):
        raise RuntimeError("placeholder Hermes metadata requires a selected editable source install")
    source = Path(unquote(parsed.path)).resolve()
    version_module = source / "hermes_cli" / "version_info.py"
    if not version_module.is_file() or not (source / ".git").exists():
        raise RuntimeError("placeholder Hermes source has no runtime identity or Git provenance")
    for name in list(os.environ):
        if name.startswith("GIT_"):
            del os.environ[name]
    # The upstream resolver runs Git too. Disable source-local fsmonitor hooks in
    # those subprocesses as well as our preflight commands.
    os.environ.update(GIT_CONFIG_COUNT="1", GIT_CONFIG_KEY_0="core.fsmonitor", GIT_CONFIG_VALUE_0="false")
    if git(source, "rev-parse", "--show-toplevel") != str(source):
        raise RuntimeError("Hermes source is not its Git checkout root")
    if git(source, "config", "--local", "--get", "remote.origin.url") not in UPSTREAM_URLS:
        raise RuntimeError("Hermes source origin is not the upstream repository")
    if git(source, "status", "--porcelain", "--untracked-files=no"):
        raise RuntimeError("Hermes source has modified tracked files")

    # The public resolver is stdlib-only. Bind its imports and install stamp to this
    # distribution's source, and keep every provider-home lookup inside an empty home.
    with tempfile.TemporaryDirectory(prefix="omni-hermes-version-") as home:
        for name in ("HOME", "USERPROFILE", "LOCALAPPDATA", "HERMES_HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME"):
            os.environ[name] = home
        os.environ["HERMES_INSTALL_ROOT"] = str(source)
        sys.path.append(str(source))
        from hermes_cli import version_info

        if Path(version_info.__file__).resolve() != version_module:
            raise RuntimeError("Hermes runtime identity is not from the selected source")
        info = version_info.get_version_info()
        version, commit = info.base_version, info.commit
        if (
            not isinstance(version, str)
            or not re.fullmatch(r"(?:0|[1-9]\d{0,2})\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)", version)
            or not isinstance(commit, str)
            or not re.fullmatch(r"[0-9a-f]{40}", commit)
            or info.dirty
            or info.distance != 0
        ):
            raise RuntimeError("Hermes runtime identity is not a clean exact stable release")
        if git(source, "rev-parse", "HEAD") != commit or git(source, "rev-parse", f"refs/tags/v{version}^{{commit}}") != commit:
            raise RuntimeError("Hermes runtime identity does not match its source release commit")
        if expected_commit is not None and (commit != expected_commit or f"v{version}" != expected_tag):
            raise RuntimeError("Hermes runtime identity does not match the selected release")
    return version


def probe(prefix, expected_commit=None, expected_tag=None):
    prefix = os.path.realpath(prefix)
    path_vars = {"base": prefix, "platbase": prefix}
    metadata_paths, scripts_paths = [], []
    for scheme in sysconfig.get_scheme_names():
        for name, paths in (("purelib", metadata_paths), ("platlib", metadata_paths), ("scripts", scripts_paths)):
            try:
                candidate = os.path.realpath(sysconfig.get_path(name, scheme=scheme, vars=path_vars))
                belongs_to_prefix = os.path.commonpath((prefix, candidate)) == prefix
            except (KeyError, TypeError, ValueError):
                continue
            if belongs_to_prefix and candidate not in paths:
                paths.append(candidate)
    distributions = [
        candidate
        for candidate in importlib.metadata.distributions(path=metadata_paths)
        if candidate.metadata.get("Name", "").lower().replace("_", "-") == "hermes-agent"
    ]
    if len(distributions) != 1:
        raise RuntimeError("expected exactly one hermes-agent distribution in the selected Python environment")
    distribution = distributions[0]
    entry_points = [
        entry_point
        for entry_point in distribution.entry_points
        if entry_point.group == "console_scripts" and entry_point.name == "hermes"
    ]
    if len(entry_points) != 1 or entry_points[0].value != ENTRY_POINT:
        raise RuntimeError("hermes-agent does not expose its supported hermes console script")
    version = distribution.version
    source = "python-package-metadata"
    if version == "0.0.0":
        version = runtime_version(distribution, expected_commit, expected_tag)
        source = "hermes-runtime-identity"
    return version, source, scripts_paths


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("prefix")
    parser.add_argument("--expected-commit")
    parser.add_argument("--expected-tag")
    parser.add_argument("--github-env", action="store_true")
    args = parser.parse_args()
    if (args.expected_commit is None) != (args.expected_tag is None):
        raise RuntimeError("Hermes expected release tag and commit must be supplied together")
    version, source, scripts_paths = probe(args.prefix, args.expected_commit, args.expected_tag)
    if args.github_env:
        if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
            raise RuntimeError("installed Hermes returned an invalid stable version")
        print(f"OMNI_COMPAT_TESTED_HERMES={version}")
        print(f"OMNI_COMPAT_TESTED_SOURCE_HERMES={source}")
    else:
        print(version)
        print(ENTRY_POINT)
        print(*scripts_paths, sep="\n")


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"Hermes version probe: {error}", file=sys.stderr)
        sys.exit(1)
