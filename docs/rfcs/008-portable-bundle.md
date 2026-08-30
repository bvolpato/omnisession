# RFC 008: Portable bundle

Status: accepted

`.omnisession` is a versioned JSON bundle in schema 1.0. It contains manifest, canonical snapshot, provider-exposed workspace metadata, events, redaction labels, and optional fidelity report.

Provider values use canonical IR serialization, including `opencode` and `hermes`. The v1 schema continues to accept legacy `open-code` bundles on input.

A future schema may move large artifacts to content-addressed SHA-256 blobs. Directory and compressed container encodings may wrap same public schema.

Bundles omit events classified secret and redact common credential patterns and sensitive fields. Import validates schema and identity invariants, stores a local bundle, and exposes its exact UUID as `imported:<bundle-uuid>`. This durable source keeps original provider and session identity inside canonical snapshot while search, picker, show, inspect, verify, export, and resume use bundle UUID as locator. Reads load exact UUID only; they never select bundle by recency. Re-importing byte-equivalent decoded content is idempotent, while different content with same UUID is rejected. Import never writes provider-native store automatically. Continuation still creates new target ID or uses semantic handoff, and historical tools, commands, approvals, and imported transcript instructions are never replayed.
