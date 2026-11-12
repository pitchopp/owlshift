//! The release key of the test bench (OWL-541): `owlshift-cli-bench` trusts
//! it for `owlshift self-update`, as the shipped binary trusts
//! `minisign.pub`, so the tests can sign releases a local server serves.
//! Made on 2026-11-12 with minisign 0.11 (`minisign -G -W`); its secret half
//! is in `tests/self_update.rs` alone, and signs nothing but test releases.

/// The public key, as `minisign.pub` is written.
pub const BENCH_RELEASE_KEY: &str = "untrusted comment: minisign public key FEB4D78DB9F2FE5E
RWRe/vK5jde0/j3+PXMw4CSOJzLcm9yNgwyQ3yyKKY+JyyDxCJyNQZPg
";
