# OmniSession documentation

Start with the [README](../README.md) for install and everyday use. This page indexes everything else.

## Guide

| Topic | Section |
| --- | --- |
| Quick start | [README: Quick start](../README.md#quick-start) |
| Install on Linux or macOS | [README: Linux and macOS](../README.md#linux-and-macos) |
| Install on Windows (preview) | [README: Windows x86-64 (preview)](../README.md#windows-x86-64-preview) |
| Build from source | [README: From source](../README.md#from-source) |
| Check detected agents and stores | [README: Check this machine](../README.md#check-this-machine) |
| Session picker and keys | [README: Pick a session](../README.md#pick-a-session) |
| Search from the command line | [README: Search](../README.md#search) |
| Continue or fork in another agent | [README: Continue or fork](../README.md#continue-or-fork) |
| Delete a session | [README: Delete a session](../README.md#delete-a-session) |
| Provider shims | [README: Provider shims](../README.md#provider-shims) |
| Portable bundles | [README: Portable bundles](../README.md#portable-bundles) |
| Environment variables | [README: Configuration](../README.md#configuration) |
| Troubleshooting | [README: FAQ](../README.md#faq) |

## Reference

- [Compatibility](COMPATIBILITY.md): version gates, per-platform capabilities, validation evidence, provider-specific transfer details, and conformance tests.
- [Architecture](ARCHITECTURE.md): crates, data flow, transfer modes, native import lifecycle, and safety invariants.
- [Portable bundle schema](../schemas/omnisession-bundle-v1.schema.json)
- [Changelog](../CHANGELOG.md)

## Design

The [RFC index](rfcs/README.md) holds OmniSession's specifications.

| RFC | Topic | Status |
| --- | --- | --- |
| [001](rfcs/001-canonical-event-model.md) | Canonical event model | Accepted |
| [002](rfcs/002-workspace-identity.md) | Workspace identity | Accepted |
| [003](rfcs/003-adapter-protocol.md) | Adapter protocol | Accepted |
| [004](rfcs/004-transfer-modes.md) | Transfer modes and fidelity | Accepted; richer event fidelity remains partial |
| [005](rfcs/005-routing-and-shims.md) | Routing and shims | Accepted |
| [006](rfcs/006-threat-model.md) | Threat model | Accepted |
| [007](rfcs/007-native-materialization.md) | Native materialization | Accepted |
| [008](rfcs/008-portable-bundle.md) | Portable bundle | Accepted |
| [009](rfcs/009-native-deletion.md) | Native session deletion | Accepted |
| [010](rfcs/010-antigravity-ide-target.md) | Antigravity IDE target | Draft |

## Project

- [Roadmap](ROADMAP.md)
- [Contributing](../CONTRIBUTING.md)
- [Security policy](../SECURITY.md)
- [Code of Conduct](../CODE_OF_CONDUCT.md)
