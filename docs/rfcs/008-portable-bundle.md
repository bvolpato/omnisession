# RFC 008: Portable bundle

Status: accepted

`.omnisession` is a versioned JSON bundle in v0.1. It contains manifest, canonical snapshot, provider-exposed workspace metadata, events, redaction labels, and optional fidelity report.

Large artifacts move to content-addressed SHA-256 blobs in v0.2. Directory and compressed container encodings may wrap the same public schema.

Bundles omit events classified secret and redact common credential patterns and sensitive fields. Import validates schema and identity invariants, rejects duplicate bundle IDs, and stores a new local bundle. It never writes a provider-native store automatically.
