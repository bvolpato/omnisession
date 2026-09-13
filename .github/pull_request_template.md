## Summary

<!-- What changed and why. Link issues with "Fixes #123". -->

## Validation

<!-- Commands you ran and their results. Check what applies. -->

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --locked --workspace --all-features`
- [ ] `node scripts/provider-compatibility.mjs check` (manifest or compatibility docs changed)
- [ ] `pnpm --dir website typecheck` and website build (website changed)

## Fidelity or security impact

<!-- Provider-store writes, native import or deletion paths, redaction, indexing, CLI output, or shims. Write "None" if nothing applies. -->

## Checklist

- [ ] Fixtures are synthetic. No real transcripts, credentials, absolute personal paths, or proprietary source.
- [ ] Docs updated where behavior changed (README, compatibility manifest, or RFC).
