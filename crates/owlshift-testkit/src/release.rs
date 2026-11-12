//! The release key of the test bench (OWL-541): `owlshift-cli-bench` trusts
//! it for `owlshift self-update`, as the shipped binary trusts
//! `minisign.pub`, so the tests can sign releases a local server serves.
//! Made on 2026-11-12 with the `minisign` crate, which signs the test
//! releases (a key minisign 0.11 makes with `-G -W` does not read there:
//! "Corrupted key"); its secret half is in `tests/self_update.rs` alone.

/// The public key, as `minisign.pub` is written.
pub const BENCH_RELEASE_KEY: &str = "untrusted comment: minisign public key: 932AAB4AB9E5EECB
RWTL7uW5Sqsqk/0s2Ls93JBgJzbVOsC7f7On3P4LVEjQZJVCgoyLc/Jk
";
