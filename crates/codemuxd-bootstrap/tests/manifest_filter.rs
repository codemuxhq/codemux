//! Integration test entry point for the build-script helper at
//! `build/manifest_filter.rs`. The helper is included via `#[path]`
//! because it lives outside the lib's compilation graph (see the
//! module-level comment in `manifest_filter.rs` for rationale). Its
//! inline `#[cfg(test)] mod tests` block is what actually runs here
//! — this file exists only to give Cargo a test target that pulls
//! the helper into a `cfg(test)` compilation.

#[path = "../build/manifest_filter.rs"]
mod manifest_filter;
