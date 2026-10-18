//! Partial-file writer and safe finalization (Phase 4, storage layer).
//!
//! Filesystem law for transfers (`DOCS/architecture.md` §10, the PRD's
//! Windows-first file handling, and the project reliability invariants):
//!
//! - a partial transfer writes to an engine-owned, identifiable name
//!   (`<file>.swiftfetch-<pid>-<n>.part`) that can never be mistaken for a
//!   completed download;
//! - no final filename exists before every expected byte has been written
//!   and validated: finalization claims the final name by an *exclusive*
//!   creation (`create_new`, which fails atomically when the name exists)
//!   and then same-volume-renames the validated partial file onto it;
//! - an existing user file can therefore not be replaced, silently or
//!   otherwise — the deterministic collision policy appends `(n)` suffixes
//!   instead of overwriting;
//! - failed finalization preserves the partial data and never leaves a
//!   final-name file that holds transferred content.
//!
//! Filenames themselves are produced by the download engine
//! (`download::sequential::select_filename`) and sanitized for Windows
//! before they ever reach this module; `with_suffix` keeps suffix-bearing
//! candidates within the Windows per-component length limit as well.
//! The partial/final naming scheme and the no-overwrite finalization order
//! are policy, recorded as ADR-0016.

// Phase 4 wiring note: commands call this module from Phase 11; unit tests
// exercise it in the meantime (narrow `dead_code` waiver per AGENTS.md).
#![allow(dead_code)]

use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::AsyncWriteExt;

use crate::error::{AppError, ErrorCategory};

/// Windows per-path-component limit (NTFS), in UTF-16 code units.
///
/// The limit applies to any single path element regardless of long-path
/// registry settings; keeping components at or below it means a produced
/// name always fits a conventional Windows path.
pub(crate) const MAX_COMPONENT_UNITS: usize = 255;

/// One-up sequence number that keeps concurrent transfers of this process
/// from ever sharing a partial-file name.
static PARTIAL_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// How many deterministic `(n)` candidates are tried before giving up.
///
/// The first candidate is the plain name; the policy is exhaustive enough
/// that reaching the bound requires an implausibly populated directory.
const MAX_NAME_ATTEMPTS: u32 = 999;

/// Shared error mapping for filesystem failures with a path-annotated,
/// actionable message; the raw `io::Error` stays in the hidden source.
pub(crate) fn fs_failure(message: String, err: io::Error) -> AppError {
    AppError::new(ErrorCategory::Filesystem, message).with_source(err)
}

/// Creates the output directory tree when missing and reports clear errors.
///
/// Success requires a real directory afterward: a path occupied by a
/// regular file fails `create_dir_all` and surfaces as [`ErrorCategory::Filesystem`].
pub(crate) fn ensure_output_dir(dir: &Path) -> Result<(), AppError> {
    std::fs::create_dir_all(dir).map_err(|err| {
        fs_failure(
            format!(
                "the output directory {} could not be created or is not a directory: {err}",
                dir.display()
            ),
            err,
        )
    })
}

/// A validated partial-transfer file with its engine-owned name.
pub(crate) struct PartialFile {
    path: PathBuf,
    file: tokio::fs::File,
}

impl PartialFile {
    /// Creates the output directory when necessary and opens a fresh,
    /// identifiable partial file inside it.
    ///
    /// The name carries the process id plus a per-process sequence so two
    /// simultaneous processes *and* later concurrent transfers inside one
    /// process can never share one partial file. A leftover partial file
    /// from a previous process that reproduces this exact name is
    /// overwritten on purpose: the engine writes into its own namespace and
    /// re-creates the content from scratch; user files outside
    /// `*.swiftfetch-*.part` are never touched.
    ///
    /// # Panics
    /// Panics if `name` contains path separators (the engine sanitizes
    /// filenames, so this only marks programmer error).
    pub(crate) async fn create(dir: &Path, name: &str) -> Result<Self, AppError> {
        assert!(
            !name.contains('/') && !name.contains('\\'),
            "partial file names must be single components"
        );
        ensure_output_dir(dir)?;
        let sequence = PARTIAL_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(
            "{name}.swiftfetch-{}-{sequence}.part",
            process::id()
        ));
        let file = tokio::fs::File::create(&path).await.map_err(|err| {
            fs_failure(
                format!(
                    "the partial file {} could not be created: {err}",
                    path.display()
                ),
                err,
            )
        })?;
        Ok(Self { path, file })
    }

    /// The identifiable partial-file path (owned by the engine, never user data).
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Writes one body chunk and reports its size, keeping all writes
    /// bounded to the chunk actually received from the network.
    pub(crate) async fn write_chunk(&mut self, bytes: &[u8]) -> Result<u64, AppError> {
        self.file.write_all(bytes).await.map_err(|err| {
            fs_failure(
                format!(
                    "writing the partial file {} failed: {err}",
                    self.path.display()
                ),
                err,
            )
        })?;
        Ok(bytes.len() as u64)
    }

    /// Flushes all written data and metadata to stable storage.
    ///
    /// Durability before the final rename prevents the rename itself from
    /// advertising content the power-loss window could later lose.
    pub(crate) async fn sync(&mut self) -> Result<(), AppError> {
        self.file.sync_all().await.map_err(|err| {
            fs_failure(
                format!(
                    "the downloaded data in {} could not be committed to disk: {err}",
                    self.path.display()
                ),
                err,
            )
        })?;
        Ok(())
    }
}

/// Claims the destination for a validated download: an existing user file
/// is never replaced, the transfer instead finalizes under a deterministic
/// `(n)`-suffixed sibling name.
///
/// The claimed path exists as a fresh empty *reservation* (created
/// atomically with `create_new`); [`finalize`] then moves the partial file
/// onto it. If the claim cannot be made at any candidate, the error names
/// the directory for the user.
///
/// # Panics
/// Panics if `file_name` contains path separators (same programmer-error
/// contract as [`PartialFile::create`]).
pub(crate) fn reserve_destination(dir: &Path, file_name: &str) -> Result<PathBuf, AppError> {
    assert!(
        !file_name.contains('/') && !file_name.contains('\\'),
        "destination file names must be single components"
    );
    for attempt in 0..=MAX_NAME_ATTEMPTS {
        let candidate = if attempt == 0 {
            dir.join(file_name)
        } else {
            dir.join(with_suffix(file_name, attempt))
        };
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_reservation) => return Ok(candidate),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(fs_failure(
                    format!(
                        "the destination name {} could not be reserved in {}: {err}",
                        candidate.display(),
                        dir.display()
                    ),
                    err,
                ))
            }
        }
    }
    Err(AppError::new(
        ErrorCategory::Filesystem,
        format!(
            "{} deterministic candidate names already exist in {}; refusing \
             to overwrite any existing file",
            MAX_NAME_ATTEMPTS + 1,
            dir.display()
        ),
    ))
}

/// Moves a validated partial file to its claimed destination.
///
/// Consumes the partial file. The rename stays on the same volume by
/// construction (source and destination live in the same directory), and
/// the operation replaces only this finalization's own empty reservation.
pub(crate) async fn finalize(temp: PartialFile, reserved: PathBuf) -> Result<PathBuf, AppError> {
    let mut temp = temp;
    temp.sync().await?;
    // Close the write handle before the rename; the reservation file is
    // flushed-and-closed as well (it was created per candidate and only
    // ever held zero bytes).
    let PartialFile { path, file } = temp;
    drop(file);
    std::fs::rename(&path, &reserved).map_err(|err| {
        // Best-effort release of this finalization's own, empty
        // reservation so the directory keeps no misleading empty file.
        // A path placed here by another actor inside the microseconds
        // between reservation and rename cannot be distinguished, which
        // is accepted and documented: the window is our own create-rename
        // sequence, immediately after our own exclusive claim.
        let _ = std::fs::remove_file(&reserved);
        AppError::new(
            ErrorCategory::Filesystem,
            format!(
                "finalizing the transfer from {} to {} failed: {err}",
                path.display(),
                reserved.display()
            ),
        )
        .with_source(err)
    })?;
    Ok(reserved)
}

/// Builds the `attempt`-th collision candidate: `stem (n).ext` when a final
/// extension exists (the last dot treated as one), re-truncating within the
/// Windows component limit when suffixing would exceed it.
fn with_suffix(name: &str, attempt: u32) -> String {
    let suffix = format!(" ({attempt})");
    if let Some((stem, ext)) = name.rsplit_once('.') {
        if !stem.is_empty() {
            let reserved_units = suffix.encode_utf16().count() + ext.encode_utf16().count() + 1;
            let capped = cap_units(stem, reserved_units);
            if !capped.is_empty() {
                return format!("{capped}{suffix}.{ext}");
            }
        }
    }
    let capped = cap_units(name, suffix.encode_utf16().count());
    format!("{capped}{suffix}")
}

/// Truncates `text` so that, with `reserved_units` appended later, the total
/// stays within the [`MAX_COMPONENT_UNITS`] per-component limit, cutting
/// whole `char`s only (the caller decides what "reserved" means — suffix,
/// extension, …).
pub(crate) fn cap_units(text: &str, reserved_units: usize) -> String {
    let budget = MAX_COMPONENT_UNITS.saturating_sub(reserved_units);
    let mut out = String::with_capacity(text.len());
    let mut used = 0usize;
    for ch in text.chars() {
        let units = ch.len_utf16();
        if used + units > budget {
            break;
        }
        used += units;
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{ensure_output_dir, finalize, reserve_destination, with_suffix, PartialFile};
    use crate::error::ErrorCategory;
    use std::error::Error as _;
    use std::path::{Path, PathBuf};

    /// An isolated, auto-removed scratch directory for policy tests.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(tag: &str) -> Self {
            // Uniqueness without a dependency: the pid survives concurrent
            // test binaries; the timestamp keeps repeated runs apart.
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos());
            let dir = std::env::temp_dir().join(format!(
                "swiftfetch-p4-{tag}-{}-{stamp}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("scratch dir created");
            Self(dir)
        }

        fn file(&self, name: &str, contents: &[u8]) {
            std::fs::write(self.0.join(name), contents).expect("scratch file written");
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn destination_policy_never_replaces_existing_files() {
        let dir = ScratchDir::new("collision");
        dir.file("fixture", b"USER");
        dir.file("fixture (1)", b"USER-1");
        dir.file("fixture.txt", b"USER-EX");

        let first = reserve_destination(dir.path(), "fixture").expect("third candidate");
        assert_eq!(
            first.file_name().unwrap().to_str(),
            Some("fixture (2)"),
            "suffix numbering continues deterministically"
        );
        assert!(first.is_file(), "the reservation exists and is empty");

        let second = reserve_destination(dir.path(), "fixture.txt").expect("suffixed candidate");
        assert_eq!(
            second.file_name().unwrap().to_str(),
            Some("fixture (1).txt")
        );
        assert!(second.is_file());

        // Existing user files are untouched.
        assert_eq!(
            std::fs::read(dir.path().join("fixture")).expect("read"),
            b"USER"
        );
        assert_eq!(
            std::fs::read(dir.path().join("fixture (1)")).expect("read"),
            b"USER-1"
        );
        assert_eq!(
            std::fs::read(dir.path().join("fixture.txt")).expect("read"),
            b"USER-EX"
        );
    }

    #[test]
    fn suffix_candidates_stay_within_the_windows_component_limit() {
        // A name truncated by the sanitizer to exactly 255 UTF-16 units must
        // still gain a suffix without exceeding the limit.
        let long_name = format!("{}.zip", "a".repeat(250));
        assert_eq!(long_name.encode_utf16().count(), 254);
        let suffixed = with_suffix(&long_name, 1);
        assert!(
            suffixed.encode_utf16().count() <= 255,
            "suffix candidate '{}' is {} UTF-16 units long",
            suffixed,
            suffixed.encode_utf16().count()
        );
        assert!(suffixed.ends_with(" (1).zip"), "extension is preserved");
        assert!(
            suffixed.starts_with('a'),
            "stem data is preserved from the front"
        );
    }

    #[test]
    fn exhausting_every_candidate_is_a_filesystem_error() {
        let dir = ScratchDir::new("exhausted");
        std::fs::write(dir.path().join("taken"), b"first occupies the plain name")
            .expect("seed file");
        // Occupy the plain name plus every deterministic suffix quickly by
        // reserving them (each reservation is an existing empty file).
        let mut last = dir.path().join("taken");
        for _attempt in 1..=999 {
            let reserved = reserve_destination(dir.path(), "taken")
                .expect("early attempts must still find candidates");
            assert_ne!(reserved, last, "candidates advance");
            last = reserved;
        }
        let err = reserve_destination(dir.path(), "taken")
            .expect_err("no candidate may remain after the bound");
        assert_eq!(err.category(), ErrorCategory::Filesystem);
    }

    #[test]
    fn output_dir_is_created_and_regular_file_conflicts_are_reported() {
        let dir = ScratchDir::new("outdir");
        ensure_output_dir(&dir.0.join("nested/new-tree")).expect("nested tree created");
        assert!(dir.path().join("nested/new-tree").is_dir());

        dir.file("occupied", b"this is a file");
        let err = ensure_output_dir(&dir.0.join("occupied"))
            .expect_err("a file cannot serve as the output directory");
        assert_eq!(err.category(), ErrorCategory::Filesystem);
        assert!(
            err.message().contains("occupied"),
            "the message must name the failing path: {}",
            err.message()
        );
    }

    #[test]
    fn partial_files_are_identifiable_and_finalize_by_rename() {
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async move {
            let dir = ScratchDir::new("partial");
            let mut partial = PartialFile::create(dir.path(), "target.bin")
                .await
                .expect("partial created");
            let partial_name = partial
                .path()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            let prefix = format!("target.bin.swiftfetch-{}-", std::process::id());
            assert!(
                partial_name.starts_with(&prefix) && partial_name.ends_with(".part"),
                "the engine-owned partial name embeds the pid and sequence: {partial_name}"
            );
            partial.write_chunk(b"partial bytes").await.expect("write");
            assert_eq!(
                std::fs::read(partial.path()).expect("read"),
                b"partial bytes"
            );

            let reserved =
                reserve_destination(dir.path(), "target.bin").expect("plain name was free");
            let moved = finalize(partial, reserved).await.expect("finalized");
            assert_eq!(moved.file_name().unwrap(), "target.bin");
            assert_eq!(
                std::fs::read(dir.path().join("target.bin")).expect("final content"),
                b"partial bytes"
            );
            assert!(
                !partial_path_still_exists(&dir),
                "the partial name is consumed by the rename"
            );
        });
    }

    fn partial_path_still_exists(dir: &ScratchDir) -> bool {
        std::fs::read_dir(dir.path())
            .expect("dir listed")
            .filter_map(|entry| entry.ok())
            .any(|entry| entry.file_name().to_string_lossy().ends_with(".part"))
    }

    #[test]
    fn filesystem_errors_keep_the_hidden_source_with_the_message() {
        let dir = ScratchDir::new("source");
        dir.file("occupied", b"this is a file, not the output directory");
        let err = ensure_output_dir(&dir.0.join("occupied")).expect_err("occupied path fails");
        assert_eq!(err.category(), ErrorCategory::Filesystem);
        assert!(err.source().is_some(), "the io::Error stays diagnostic");
        assert!(
            err.message().contains("occupied"),
            "the message names the directory: {}",
            err.message()
        );
    }
}
