//! Build-time tarball assembly for the SSH bootstrap.
//!
//! Runs every time the session crate compiles. Walks the workspace
//! sibling source trees (`apps/daemon`, `crates/wire`), the workspace
//! `Cargo.lock`, and `rust-toolchain.toml`, then bundles them with the
//! self-contained `bootstrap-root/Cargo.toml` into a gzipped tar
//! archive at `$OUT_DIR/codemuxd-bootstrap.tar.gz`.
//!
//! `crates/session/src/bootstrap.rs` embeds that archive via
//! `include_bytes!`. `cargo:rerun-if-changed` directives below ensure
//! the build script re-runs whenever any of the bundled files change.
//!
//! Why a build script and not hand-enumerated `include_bytes!` macros:
//! a hand-list silently misses any new file added under those trees.
//! Walking the directories at build time keeps the tarball in lockstep
//! with the source.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

type DynError = Box<dyn std::error::Error>;

fn main() -> Result<(), DynError> {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    // crates/session → crates → workspace_root
    let workspace_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .ok_or("CARGO_MANIFEST_DIR has no two-level parent")?
        .to_path_buf();

    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);
    let tarball_path = out_dir.join("codemuxd-bootstrap.tar.gz");

    let tar_gz = File::create(&tarball_path)?;
    let enc = flate2::write::GzEncoder::new(tar_gz, flate2::Compression::default());
    let mut tar = tar::Builder::new(enc);

    // Recursively bundle the daemon and wire source trees.
    let daemon_dir = workspace_root.join("apps").join("daemon");
    let wire_dir = workspace_root.join("crates").join("wire");
    bundle_dir(&mut tar, &daemon_dir, "apps/daemon")?;
    bundle_dir(&mut tar, &wire_dir, "crates/wire")?;

    // Workspace-root files the bootstrap depends on. Cargo.lock pins
    // dep versions for reproducible builds; rust-toolchain.toml pins
    // the rustc the remote must use.
    let cargo_lock = workspace_root.join("Cargo.lock");
    let toolchain = workspace_root.join("rust-toolchain.toml");
    bundle_file(&mut tar, &cargo_lock, "Cargo.lock")?;
    bundle_file(&mut tar, &toolchain, "rust-toolchain.toml")?;

    // The bootstrap-root manifest replaces the live workspace manifest
    // when the tarball unpacks (the live one references crates we do
    // not ship in the bootstrap). See bootstrap-root/Cargo.toml.
    let bootstrap_root = manifest_dir.join("bootstrap-root").join("Cargo.toml");
    bundle_file(&mut tar, &bootstrap_root, "Cargo.toml")?;

    let enc = tar.into_inner()?;
    enc.finish()?.flush()?;

    // Cargo's `rerun-if-changed` does not recurse into directories. We
    // emit one directive per tracked path so the build script reruns
    // when any source file changes. Dirs themselves are listed too so
    // file-add/remove also triggers a rerun.
    println!("cargo:rerun-if-changed={}", daemon_dir.display());
    println!("cargo:rerun-if-changed={}", wire_dir.display());
    rerun_for_walk(&daemon_dir)?;
    rerun_for_walk(&wire_dir)?;
    println!("cargo:rerun-if-changed={}", cargo_lock.display());
    println!("cargo:rerun-if-changed={}", toolchain.display());
    println!("cargo:rerun-if-changed={}", bootstrap_root.display());

    Ok(())
}

/// Recursively append every regular file under `src` to the archive,
/// rooted at `dst_prefix`. Skips dot-directories (e.g. `.git`,
/// `.vscode`) and any `target` build dir if one happens to live inside
/// the tree — neither belongs in the bootstrap.
fn bundle_dir<W: Write>(
    tar: &mut tar::Builder<W>,
    src: &Path,
    dst_prefix: &str,
) -> Result<(), DynError> {
    walk(src, &mut |path, rel| {
        let rel_str = rel.to_str().ok_or("non-utf8 path in source tree")?;
        let dst = format!("{dst_prefix}/{rel_str}");
        tar.append_path_with_name(path, &dst)?;
        Ok(())
    })
}

/// Append a single file to the archive at `dst` (the tarball-internal
/// path). Errors if `src` doesn't exist — the bootstrap is broken
/// without these pieces, so failing the build is correct.
fn bundle_file<W: Write>(tar: &mut tar::Builder<W>, src: &Path, dst: &str) -> Result<(), DynError> {
    if !src.exists() {
        return Err(format!("required bootstrap input missing: {}", src.display()).into());
    }
    tar.append_path_with_name(src, dst)?;
    Ok(())
}

/// Walk `root` recursively, calling `f(path, rel_to_root)` for each
/// regular file. Skips entries whose file name starts with '.' or
/// equals "target" so build artifacts and editor scratch never end up
/// in the archive.
fn walk(
    root: &Path,
    f: &mut dyn FnMut(&Path, &Path) -> Result<(), DynError>,
) -> Result<(), DynError> {
    walk_inner(root, root, f)
}

fn walk_inner(
    root: &Path,
    cur: &Path,
    f: &mut dyn FnMut(&Path, &Path) -> Result<(), DynError>,
) -> Result<(), DynError> {
    for entry in fs::read_dir(cur)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str == "target" {
            continue;
        }
        let path = entry.path();
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            walk_inner(root, &path, f)?;
        } else if metadata.is_file() {
            let rel = path.strip_prefix(root)?;
            f(&path, rel)?;
        }
        // Symlinks and other entry types are silently skipped — neither
        // belongs in a source tarball; ignoring them keeps the bundle
        // hermetic.
    }
    Ok(())
}

/// Emit `cargo:rerun-if-changed` for every regular file under `root`.
/// Same skip rules as `walk`.
fn rerun_for_walk(root: &Path) -> Result<(), DynError> {
    walk(root, &mut |path, _rel| {
        println!("cargo:rerun-if-changed={}", path.display());
        Ok(())
    })
}
