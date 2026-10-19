//! Safe automatic ZIP extraction (Phase 10).
//!
//! The extraction engine runs **only after a download finalized**: the
//! caller passes an already-finalized archive path, so a transfer that is
//! still streaming never reaches this module. Its guarantees, in the
//! terms of `DOCS/phases.md` Phase 10:
//!
//! - **Entry paths cannot escape the destination.** Every entry name is
//!   checked before writing: rooted/absolute names (leading `/` or `\`),
//!   `..` traversal, Windows drive prefixes (`C:`, `C:dir`, `C:\`, verbatim
//!   `\\?\…`), UNC paths (`\\server\share`), names resolving outside the
//!   extraction root, and (Windows-first law) components whose stem is a
//!   reserved device name (`CON`, `NUL`, `COM1`, …) are all refused — a
//!   reserved name can land on a device, which is as much "outside" as any
//!   traversal. The checks are lexical (host-independent) plus a resolved
//!   containment check, so behavior does not silently differ per platform.
//! - **No silent overwrites.** The collision policy: existing files are
//!   skipped by default and reported (outcome list + warning log);
//!   replacement must be configured explicitly. A repeated extraction
//!   therefore never destroys user data silently.
//! - **Bounded work.** Entry claims are screened before anything is
//!   written, and the same bounds are enforced against actually written
//!   bytes (an archive may lie about its uncompressed size). The engine
//!   defaults are 1 GiB per entry and 10 GiB total; a partial bomb file
//!   written until the bound fires is removed again.
//! - **The archive is preserved.** Nothing here deletes or rewrites the
//!   archive, whatever happens; every failing path leaves it
//!   byte-for-byte intact.
//! - **Extraction state is separate from download state.** Every run owns
//!   a row in the `extractions` table (`queued → extracting →
//!   completed/failed`, error details on failure). A download never
//!   flips back because extraction failed, and a failed run records
//!   its details before returning.
//! - **Retries do not touch the network.** The module has no HTTP
//!   surface; retrying an extraction re-runs [`extract_zip`] against the
//!   same on-disk archive and reuses the same extraction row.
//!
//! Logs never contain URLs — extraction works on local paths only.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use tokio::task::spawn_blocking;
use zip::read::ZipArchive;
use zip::result::ZipError;

use crate::error::{AppError, ErrorCategory};
use crate::storage::file_writer::ensure_output_dir;
use crate::storage::repository::{ExtractionState, Repository};

/// Maximum size of a single extracted entry (1 GiB).
///
/// Checked against the archive's claimed size up front *and* against the
/// actually written total, because the claim is unverified remote-side
/// metadata while the written total is the fact on disk.
const MAX_ENTRY_SIZE: u64 = 1_073_741_824;

/// Maximum total extracted size for one archive (10 GiB).
const MAX_TOTAL_EXTRACTED_SIZE: u64 = 10_737_418_240;

/// Buffer size for streaming extraction (1 MiB).
const EXTRACT_BUFFER_SIZE: usize = 1_048_576;

/// Resource limits of one run. The engine defaults are
/// [`MAX_ENTRY_SIZE`]/[`MAX_TOTAL_EXTRACTED_SIZE`]; tests use smaller
/// values so the boundaries stay reachable without multi-gigabyte writes.
#[derive(Debug, Clone, Copy)]
struct ExtractionLimits {
    max_entry_size: u64,
    max_total_size: u64,
}

fn engine_limits() -> ExtractionLimits {
    ExtractionLimits {
        max_entry_size: MAX_ENTRY_SIZE,
        max_total_size: MAX_TOTAL_EXTRACTED_SIZE,
    }
}

/// Policy controls of one extraction.
///
/// The derived default is the documented contract: extraction to a
/// sibling folder of the archive, no silent overwrites.
#[derive(Debug, Clone, Default)]
pub struct ExtractionConfig {
    /// Override the extraction destination directory. `None` extracts to
    /// a sibling folder named after the archive (without the `.zip`
    /// extension) — a dedicated folder alongside the archive.
    pub extract_dir: Option<PathBuf>,
    /// Whether to replace existing files instead of skipping them.
    /// The default (`false`) is the documented no-silent-overwrite
    /// behavior: existing files are skipped and reported.
    pub overwrite_existing: bool,
}

/// Errors that can occur during ZIP extraction.
#[derive(Debug)]
pub enum ExtractionError {
    /// The run's own preconditions failed (archive missing, the
    /// destination could not be created as a directory…).
    ValidationFailed(String),
    /// An entry name tried to write outside the extraction root.
    PathTraversal(String),
    /// An entry exceeded the per-entry limit (claim or written total).
    EntryTooLarge(u64, u64),
    /// The archive's entries together exceed the total limit.
    TotalSizeExceeded(u64, u64),
    /// Local filesystem failure during the run.
    Io(io::Error),
    /// The archive is not a readable ZIP (bad magic, truncation, corrupt
    /// central directory or entry).
    ZipFormat(ZipError),
    /// Extraction was cancelled before it could run.
    Cancelled,
}

impl std::fmt::Display for ExtractionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExtractionError::ValidationFailed(msg) => {
                write!(f, "archive validation failed: {msg}")
            }
            ExtractionError::PathTraversal(msg) => {
                write!(f, "unsafe archive path refused: {msg}")
            }
            ExtractionError::EntryTooLarge(size, max) => {
                write!(f, "entry size {size} bytes exceeds the {max} byte limit")
            }
            ExtractionError::TotalSizeExceeded(size, max) => {
                write!(
                    f,
                    "total extracted size {size} bytes would exceed the {max} byte limit"
                )
            }
            ExtractionError::Io(e) => write!(f, "I/O error during extraction: {e}"),
            ExtractionError::ZipFormat(e) => {
                write!(f, "the archive is not a readable ZIP file: {e}")
            }
            ExtractionError::Cancelled => write!(f, "extraction was cancelled"),
        }
    }
}

impl std::error::Error for ExtractionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ExtractionError::Io(e) => Some(e),
            ExtractionError::ZipFormat(e) => Some(e),
            _ => None,
        }
    }
}

/// Result of a ZIP extraction operation.
#[derive(Debug)]
pub struct ExtractionOutcome {
    /// The destination directory where files were extracted.
    pub destination: PathBuf,
    /// Number of files successfully extracted.
    pub files_extracted: usize,
    /// Total bytes written.
    pub bytes_written: u64,
    /// Files skipped by the no-silent-overwrite policy.
    pub skipped_files: Vec<PathBuf>,
}

/// Validates one entry name against the extraction root.
fn validate_extracted_path(
    entry_name: &str,
    extract_root: &Path,
) -> Result<PathBuf, ExtractionError> {
    // A name that starts at a filesystem root ("/..." on every platform,
    // "\..." rooted at a drive on Windows) can resolve outside the root;
    // it is refused lexically, independent of the host path parser.
    if entry_name.starts_with('/') || entry_name.starts_with('\\') {
        return Err(ExtractionError::PathTraversal(format!(
            "absolute/rooted path not allowed: {entry_name}"
        )));
    }

    // Normalize: drop '.' components; later checks are component-wise.
    let normalized = Path::new(entry_name)
        .components()
        .filter(|component| !matches!(component, std::path::Component::CurDir))
        .collect::<PathBuf>();

    // '..' traversal is refused before anything else can reinterpret it.
    for component in normalized.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(ExtractionError::PathTraversal(format!(
                "parent directory traversal not allowed: {entry_name}"
            )));
        }
    }

    // Windows drive prefixes, lexically: "X:" and "X:dir" (drive-relative)
    // refuse to be understood as names — the check does not depend on the
    // host parser, so it fires identically everywhere.
    let first = normalized
        .components()
        .next()
        .map_or_else(String::new, |component| {
            component.as_os_str().to_string_lossy().into_owned()
        });
    let is_drive_prefixed = first.len() >= 2
        && first.as_bytes()[0].is_ascii_alphabetic()
        && first.as_bytes()[1] == b':';
    if is_drive_prefixed || first.starts_with("\\\\") {
        return Err(ExtractionError::PathTraversal(format!(
            "drive/UNC prefix not allowed: {entry_name}"
        )));
    }

    // Windows reserved device names: every component ("stem.ext") whose
    // stem is a device name can end up on the device instead of in the
    // destination ("CON", "CON.txt"). The stem check is unconditional so
    // Windows-first behavior does not depend on where extraction runs.
    for component in normalized.components() {
        let Some(name) = component.as_os_str().to_str() else {
            return Err(ExtractionError::PathTraversal(format!(
                "non-UTF-8 entry path not allowed: {entry_name}"
            )));
        };
        let stem = name.split('.').next().unwrap_or(name);
        if is_windows_reserved(stem) {
            return Err(ExtractionError::PathTraversal(format!(
                "reserved Windows device name not allowed: {entry_name}"
            )));
        }
    }

    // Join with the extract root and verify containment after resolution,
    // so a mount point or symlink already inside the root cannot redirect
    // the write outside it. The root must exist by this point; the caller
    // creates it before any entry is written.
    let full_path = extract_root.join(&normalized);
    let canonical_root = extract_root.canonicalize().map_err(|e| {
        ExtractionError::Io(io::Error::other(format!(
            "failed to canonicalize extract root: {e}"
        )))
    })?;
    // Resolve the joined path through the filesystem so a symlink or
    // mount point already inside the root cannot redirect the write
    // outside it. A fresh leaf cannot canonicalize by itself; the
    // deepest existing ancestor is canonicalized and the missing names
    // are re-appended lexically. Mixing a verbatim `\\?\` root (what
    // `canonicalize` returns on Windows) with a plain join would
    // false-positive every fresh entry as an escape.
    let canonical_full = match full_path.canonicalize() {
        Ok(resolved) => resolved,
        Err(_) => {
            let mut missing: Vec<std::ffi::OsString> = Vec::new();
            let mut probe = full_path.clone();
            let resolved_existing = loop {
                match probe.canonicalize() {
                    Ok(resolved) => break resolved,
                    Err(_) => match probe.file_name() {
                        Some(name) => {
                            missing.push(name.to_owned());
                            probe = match probe.parent() {
                                Some(parent) => parent.to_path_buf(),
                                None => {
                                    return Err(ExtractionError::ValidationFailed(format!(
                                        "the extraction root vanished during validation: \
                                         {entry_name}"
                                    )));
                                }
                            };
                        }
                        None => {
                            return Err(ExtractionError::ValidationFailed(format!(
                                "the extraction root vanished during validation: {entry_name}"
                            )));
                        }
                    },
                }
            };
            let mut resolved = resolved_existing;
            for name in missing.iter().rev() {
                resolved.push(name);
            }
            resolved
        }
    };
    if !canonical_full.starts_with(&canonical_root) {
        return Err(ExtractionError::PathTraversal(format!(
            "extraction would escape the root directory: {entry_name}"
        )));
    }

    Ok(canonical_full)
}

/// The Windows reserved device stem set (checked case-insensitively).
fn is_windows_reserved(stem: &str) -> bool {
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let upper = stem.to_ascii_uppercase();
    RESERVED.contains(&upper.as_str())
}

/// Determines the extraction destination directory.
pub(crate) fn determine_extract_dir(archive_path: &Path, config: &ExtractionConfig) -> PathBuf {
    if let Some(dir) = &config.extract_dir {
        dir.clone()
    } else {
        let parent = archive_path.parent().unwrap_or_else(|| Path::new("."));
        let stem = archive_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("extracted");
        parent.join(stem)
    }
}

/// Extracts a ZIP archive under the engine's default limits.
///
/// `repo` + `extraction_id` persist the run; `archive_path` must already
/// be a finalized download on disk.
pub async fn extract_zip(
    repo: &Repository,
    extraction_id: i64,
    archive_path: PathBuf,
    config: ExtractionConfig,
) -> Result<ExtractionOutcome, ExtractionError> {
    extract_zip_with_limits(repo, extraction_id, archive_path, config, engine_limits()).await
}

/// The limit-parameterized core; extract_zip seeds the engine limits.
async fn extract_zip_with_limits(
    repo: &Repository,
    extraction_id: i64,
    archive_path: PathBuf,
    config: ExtractionConfig,
    limits: ExtractionLimits,
) -> Result<ExtractionOutcome, ExtractionError> {
    if !archive_path.is_file() {
        let error = ExtractionError::ValidationFailed(format!(
            "the archive was not found at {}",
            archive_path.display()
        ));
        record_failure(repo, extraction_id, &error);
        return Err(error);
    }

    let extract_dir = determine_extract_dir(&archive_path, &config);
    if let Err(err) = ensure_output_dir(&extract_dir) {
        let error = ExtractionError::from_app_error(err);
        record_failure(repo, extraction_id, &error);
        return Err(error);
    }

    // Entering `extracting` marks the record active before any write; a
    // crash between this write and the extraction work leaves a
    // recoverable (retryable) record, not a false success.
    if let Err(err) = repo.update_extraction(extraction_id, ExtractionState::Extracting, None) {
        return Err(ExtractionError::from_app_error(err));
    }

    let run =
        spawn_blocking(move || run_extraction(&archive_path, &extract_dir, &config, &limits)).await;

    match run {
        Ok(Ok(outcome)) => {
            repo.update_extraction(extraction_id, ExtractionState::Completed, None)
                .map_err(ExtractionError::from_app_error)?;
            Ok(outcome)
        }
        Ok(Err(e)) => {
            record_failure(repo, extraction_id, &e);
            Err(e)
        }
        Err(join) => {
            let error = ExtractionError::Io(io::Error::other(format!(
                "the extraction worker did not finish: {join}"
            )));
            record_failure(repo, extraction_id, &error);
            Err(error)
        }
    }
}

/// Marks the run `failed` with the typed category and message. The
/// download record is never touched — a failed extraction is not a
/// failed download.
fn record_failure(repo: &Repository, extraction_id: i64, error: &ExtractionError) {
    let app = error.to_app_error();
    let details = (app.category(), app.message().to_owned());
    if let Err(record) =
        repo.update_extraction(extraction_id, ExtractionState::Failed, Some(details))
    {
        tracing::warn!(
            "the extraction record {extraction_id} could not record its failure: {}",
            record.message()
        );
    }
}

/// The synchronous extraction core ([`spawn_blocking`] guest): open,
/// screen, write. Pure filesystem work; collisions (skips) are warned
/// and reported on the outcome.
fn run_extraction(
    archive_path: &Path,
    extract_dir: &Path,
    config: &ExtractionConfig,
    limits: &ExtractionLimits,
) -> Result<ExtractionOutcome, ExtractionError> {
    let file = fs::File::open(archive_path).map_err(|e| {
        ExtractionError::Io(io::Error::other(format!(
            "failed to open the archive {}: {e}",
            archive_path.display()
        )))
    })?;
    let mut archive = ZipArchive::new(file).map_err(ExtractionError::ZipFormat)?;

    // Screen every entry's claimed size before writing anything.
    let mut claimed_total: u64 = 0;
    for index in 0..archive.len() {
        let claimed = archive
            .by_index_raw(index)
            .map_err(ExtractionError::ZipFormat)?
            .size();
        claimed_total += claimed;
        if claimed > limits.max_entry_size {
            return Err(ExtractionError::EntryTooLarge(
                claimed,
                limits.max_entry_size,
            ));
        }
        if claimed_total > limits.max_total_size {
            return Err(ExtractionError::TotalSizeExceeded(
                claimed_total,
                limits.max_total_size,
            ));
        }
    }

    let mut files_extracted = 0usize;
    let mut bytes_written = 0u64;
    let mut skipped_files = Vec::new();
    let mut buffer = vec![0u8; EXTRACT_BUFFER_SIZE];

    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(ExtractionError::ZipFormat)?;
        let entry_name = entry.name().to_owned();

        let dest_path = validate_extracted_path(&entry_name, extract_dir)?;

        if entry_name.ends_with('/') {
            // Directory entry. Archives may also omit directory rows —
            // the parent creation of file entries covers that.
            fs::create_dir_all(&dest_path).map_err(|e| {
                ExtractionError::Io(io::Error::other(format!(
                    "failed to create the directory {}: {e}",
                    dest_path.display()
                )))
            })?;
            continue;
        }

        // No-silent-overwrite policy: by default an existing file is
        // skipped and reported; replacement must be configured explicitly.
        if dest_path.exists() {
            if !config.overwrite_existing {
                tracing::warn!(
                    "extraction skipped an existing file per the no-silent-overwrite \
                     policy: {}",
                    dest_path.display()
                );
                skipped_files.push(dest_path);
                continue;
            }
            fs::remove_file(&dest_path).map_err(|e| {
                ExtractionError::Io(io::Error::other(format!(
                    "failed to remove the existing file {} for configured \
                     replacement: {e}",
                    dest_path.display()
                )))
            })?;
        }

        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                ExtractionError::Io(io::Error::other(format!(
                    "failed to create the parent directory {}: {e}",
                    parent.display()
                )))
            })?;
        }

        let mut outfile = fs::File::create(&dest_path).map_err(|e| {
            ExtractionError::Io(io::Error::other(format!(
                "failed to create the file {}: {e}",
                dest_path.display()
            )))
        })?;

        // Write with live enforcement: the claim screen cannot bound what
        // decompression actually yields.
        let mut entry_written = 0u64;
        loop {
            let read = entry.read(&mut buffer).map_err(ExtractionError::Io)?;
            if read == 0 {
                break;
            }
            entry_written += read as u64;
            bytes_written += read as u64;
            if entry_written > limits.max_entry_size || bytes_written > limits.max_total_size {
                // Bound the damage of a bomb we started extracting: drop
                // the partial file we just created, then fail the run.
                let _ = fs::remove_file(&dest_path);
                if entry_written > limits.max_entry_size {
                    return Err(ExtractionError::EntryTooLarge(
                        entry_written,
                        limits.max_entry_size,
                    ));
                }
                return Err(ExtractionError::TotalSizeExceeded(
                    bytes_written,
                    limits.max_total_size,
                ));
            }
            std::io::Write::write_all(&mut outfile, &buffer[..read])
                .map_err(ExtractionError::Io)?;
        }

        files_extracted += 1;
    }

    Ok(ExtractionOutcome {
        destination: extract_dir.to_path_buf(),
        files_extracted,
        bytes_written,
        skipped_files,
    })
}

impl ExtractionError {
    /// Converts this extraction error to an `AppError`.
    ///
    /// Every extraction failure maps onto `zip-extraction` (cancellation
    /// stays `cancelled`); this funnel feeds the extraction record's error
    /// details and, later, the command surface.
    pub fn to_app_error(&self) -> AppError {
        match self {
            ExtractionError::ValidationFailed(msg) => {
                AppError::new(ErrorCategory::ZipExtraction, msg.clone())
            }
            ExtractionError::PathTraversal(msg) => {
                AppError::new(ErrorCategory::ZipExtraction, msg.clone())
            }
            ExtractionError::EntryTooLarge(size, max) => AppError::new(
                ErrorCategory::ZipExtraction,
                format!("entry size {size} bytes exceeds the {max} byte limit"),
            ),
            ExtractionError::TotalSizeExceeded(size, max) => AppError::new(
                ErrorCategory::ZipExtraction,
                format!("total extracted size {size} bytes would exceed the {max} byte limit"),
            ),
            ExtractionError::Io(e) => AppError::new(
                ErrorCategory::ZipExtraction,
                format!("I/O error during extraction: {e}"),
            ),
            ExtractionError::ZipFormat(e) => AppError::new(
                ErrorCategory::ZipExtraction,
                format!("the archive is not a readable ZIP file: {e}"),
            ),
            ExtractionError::Cancelled => {
                AppError::new(ErrorCategory::Cancelled, "extraction was cancelled")
            }
        }
    }

    /// Bridges the shared `AppError` taxonomy back into extraction errors
    /// (used where supporting layers — the filesystem family — report
    /// through the shared contract).
    pub fn from_app_error(err: AppError) -> Self {
        match err.category() {
            ErrorCategory::Cancelled => ExtractionError::Cancelled,
            ErrorCategory::Filesystem => {
                ExtractionError::Io(io::Error::other(err.message().to_owned()))
            }
            _ => ExtractionError::ValidationFailed(err.message().to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// A scratch directory for one test (engine tests use the same shape).
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new(tag: &str) -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos());
            let path = std::env::temp_dir().join(format!(
                "swiftfetch-zip-{tag}-{}-{stamp}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("scratch created");
            Self { path }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// Builds a ZIP archive from (relative-name, bytes) pairs with DEFLATE.
    fn make_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = fs::File::create(path).expect("archive created");
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in entries {
            zip.start_file(*name, options).expect("entry started");
            zip.write_all(bytes).expect("entry written");
        }
        zip.finish().expect("archive finished");
    }

    /// The blocking core extracted from the async wrapper, called directly.
    fn run(
        archive: &Path,
        extract_dir: &Path,
        config: &ExtractionConfig,
    ) -> Result<ExtractionOutcome, ExtractionError> {
        run_extraction(archive, extract_dir, config, &engine_limits())
    }

    fn read_file(path: &Path) -> Vec<u8> {
        fs::read(path).expect("file read")
    }

    fn make_out(dir: &Path) -> PathBuf {
        fs::create_dir_all(dir).expect("destination created");
        dir.canonicalize().expect("destination exists")
    }

    #[test]
    fn extracts_nested_paths_byte_for_byte() {
        let temp = Scratch::new("nested");
        let archive = temp.path.join("pkg.zip");
        make_zip(
            &archive,
            &[
                ("top.txt", b"top level"),
                ("nested/deep/payload.json", br#"{"ok":1}"#),
                ("nested/note.md", b"nested note"),
            ],
        );
        let extract_dir = make_out(&temp.path.join("out"));
        let outcome =
            run(&archive, &extract_dir, &ExtractionConfig::default()).expect("clean extraction");

        assert_eq!(outcome.files_extracted, 3);
        assert_eq!(outcome.bytes_written, 9 + 8 + 11);
        assert_eq!(
            read_file(
                &outcome
                    .destination
                    .join("nested")
                    .join("deep")
                    .join("payload.json")
            ),
            br#"{"ok":1}"#.to_vec()
        );
        assert_eq!(
            read_file(&outcome.destination.join("top.txt")),
            b"top level".to_vec()
        );
    }

    #[test]
    fn a_directory_entry_materializes_the_directory() {
        let temp = Scratch::new("dir-entry");
        let archive = temp.path.join("pkg.zip");
        {
            let file = fs::File::create(&archive).expect("archive created");
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            zip.add_directory("docs/notes/", options)
                .expect("dir added");
            zip.start_file("docs/notes/keep.txt", options)
                .expect("file started");
            zip.write_all(b"kept").expect("file written");
            zip.finish().expect("archive finished");
        }
        let extract_dir = make_out(&temp.path.join("out"));
        let outcome = run(&archive, &extract_dir, &ExtractionConfig::default()).expect("clean");
        assert!(
            extract_dir.join("docs").join("notes").is_dir(),
            "the directory entry was materialized"
        );
        assert_eq!(
            outcome.files_extracted, 1,
            "directories do not count as files"
        );
    }

    #[test]
    fn unsafe_names_are_refused_before_any_write() {
        let temp = Scratch::new("unsafe");
        let extract_dir = make_out(&temp.path.join("out"));
        for entry in [
            "/etc/absolute.txt",
            "//x",
            "C:",
            "C:implicit",
            "\\\\server\\share\\e",
        ] {
            let error = validate_extracted_path(entry, &extract_dir)
                .expect_err("a rooted/drive/UNC name must be refused");
            assert!(
                matches!(error, ExtractionError::PathTraversal(_)),
                "expected a path-traversal refusal, got {error:?}"
            );
        }
    }

    #[test]
    fn parent_components_are_refused_at_every_step() {
        let temp = Scratch::new("parent");
        let extract_dir = make_out(&temp.path.join("out"));
        for entry in ["../../outside.txt", "good/../../outside.txt"] {
            let error = validate_extracted_path(entry, &extract_dir)
                .expect_err("traversal must be refused");
            assert!(
                matches!(&error, ExtractionError::PathTraversal(msg) if msg.contains("traversal")),
                "expected a traversal refusal, got {error:?}"
            );
        }
    }

    #[test]
    fn windows_reserved_device_names_are_refused() {
        let temp = Scratch::new("reserved");
        let extract_dir = make_out(&temp.path.join("out"));
        for entry in ["CON", "con.txt", "NUL", "COM1", "good/COM7", "prn.log"] {
            let error = validate_extracted_path(entry, &extract_dir)
                .expect_err("a reserved device name must be refused");
            assert!(
                matches!(&error, ExtractionError::PathTraversal(msg) if msg.contains("reserved")),
                "expected the reserved-name refusal, got {error:?}"
            );
        }
        // Ordinary names pass and are rooted inside the destination.
        let ok = validate_extracted_path("console.txt", &extract_dir).expect("normal name");
        assert!(ok.starts_with(&extract_dir));
    }

    #[test]
    fn existing_files_are_skipped_and_never_overwritten_by_default() {
        let temp = Scratch::new("collision");
        let archive = temp.path.join("pkg.zip");
        make_zip(&archive, &[("report.txt", b"from the archive")]);
        let extract_dir = make_out(&temp.path.join("out"));
        fs::write(extract_dir.join("report.txt"), b"user data").expect("user file placed");
        let user_file_before = read_file(&extract_dir.join("report.txt"));

        let outcome =
            run(&archive, &extract_dir, &ExtractionConfig::default()).expect("skip is not failure");

        assert_eq!(outcome.files_extracted, 0);
        assert_eq!(outcome.skipped_files, vec![extract_dir.join("report.txt")]);
        assert_eq!(
            read_file(&extract_dir.join("report.txt")),
            user_file_before,
            "the user file must be untouched"
        );

        // A repeated attempt behaves the same: user data stays.
        let repeated = run(&archive, &extract_dir, &ExtractionConfig::default())
            .expect("repeated extraction still succeeds");
        assert_eq!(repeated.skipped_files.len(), 1);
        assert_eq!(
            read_file(&extract_dir.join("report.txt")),
            user_file_before,
            "the user file must remain intact after a repeated attempt"
        );
    }

    #[test]
    fn configured_replacement_replaces() {
        let temp = Scratch::new("replacement");
        let archive = temp.path.join("pkg.zip");
        make_zip(&archive, &[("report.txt", b"from the archive")]);
        let extract_dir = make_out(&temp.path.join("out"));
        fs::write(extract_dir.join("report.txt"), b"user data").expect("user file placed");

        let replacement = ExtractionConfig {
            overwrite_existing: true,
            ..ExtractionConfig::default()
        };
        let outcome =
            run(&archive, &extract_dir, &replacement).expect("configured replacement succeeds");
        assert_eq!(outcome.files_extracted, 1);
        assert!(outcome.skipped_files.is_empty());
        assert_eq!(
            read_file(&extract_dir.join("report.txt")),
            b"from the archive",
            "replacement must actually happen when configured"
        );
    }

    #[test]
    fn the_claim_screen_refuses_oversized_entries_before_any_write() {
        let temp = Scratch::new("claimed");
        let archive = temp.path.join("big.zip");
        make_zip(&archive, &[("payload.bin", &[0u8; 4096])]);
        let extract_dir = make_out(&temp.path.join("out"));
        let outcome = run_extraction(
            &archive,
            &extract_dir,
            &ExtractionConfig::default(),
            &ExtractionLimits {
                max_entry_size: 1024,
                max_total_size: 4096,
            },
        )
        .expect_err("an entry claim above the limit must be refused");
        assert!(
            matches!(outcome, ExtractionError::EntryTooLarge(claim, max) if claim == 4096 && max == 1024),
            "the claim screen fired, got {outcome:?}"
        );
        assert!(
            fs::read_dir(&extract_dir).expect("listed").next().is_none(),
            "nothing was written"
        );
    }

    #[test]
    fn the_total_limit_refuses_a_multi_entry_archive_before_any_write() {
        let temp = Scratch::new("total");
        let archive = temp.path.join("two.zip");
        make_zip(&archive, &[("a.bin", &[0u8; 512]), ("b.bin", &[0u8; 512])]);
        let extract_dir = make_out(&temp.path.join("out"));
        let outcome = run_extraction(
            &archive,
            &extract_dir,
            &ExtractionConfig::default(),
            &ExtractionLimits {
                max_entry_size: 1024,
                max_total_size: 768,
            },
        )
        .expect_err("a two-entry archive above the total limit must be refused");
        assert!(
            matches!(outcome, ExtractionError::TotalSizeExceeded(claimed, max) if claimed == 1024 && max == 768),
            "the total refusal fired before any write, got {outcome:?}"
        );
        assert!(
            fs::read_dir(&extract_dir).expect("listed").next().is_none(),
            "nothing was written"
        );
    }

    #[test]
    fn a_corrupt_archive_fails_and_stays_byte_for_byte_identical() {
        let temp = Scratch::new("corrupt");
        let archive = temp.path.join("corrupt.zip");
        make_zip(
            &archive,
            &[("keep.txt", b"at least this is a valid zip first")],
        );
        let ok_bytes = read_file(&archive);
        // Lose the central directory: the open succeeds (the local header
        // signature is intact), the archive read path fails.
        let truncated = ok_bytes[..ok_bytes.len().saturating_sub(4)].to_vec();
        fs::write(&archive, &truncated).expect("truncated copy written");
        let before = read_file(&archive);

        let err = run(
            &archive,
            &make_out(&temp.path.join("out")),
            &ExtractionConfig::default(),
        )
        .expect_err("a truncated archive must be refused");
        assert!(
            matches!(err, ExtractionError::ZipFormat(_)),
            "a central-directory failure is a format error, got {err:?}"
        );
        assert_eq!(read_file(&archive), before, "the archive stayed intact");
    }

    #[test]
    fn a_non_zip_file_fails_as_a_format_error() {
        let temp = Scratch::new("notzip");
        let archive = temp.path.join("not.zip");
        fs::write(&archive, b"just some text bytes").expect("plain file written");
        let error = run(
            &archive,
            &make_out(&temp.path.join("out")),
            &ExtractionConfig::default(),
        )
        .expect_err("non-ZIP content must be refused");
        assert!(
            matches!(error, ExtractionError::ZipFormat(_)),
            "expected a format failure, got {error:?}"
        );
    }

    /// A retry that succeeds without any download: the first run fails
    /// (the destination is blocked by a regular file), the second run uses
    /// the same extraction record and the same archive after the obstacle
    /// is gone. The download record stays `completed` the whole time.
    #[test]
    fn retry_completes_from_the_same_record_without_any_download() {
        let temp = Scratch::new("retry");
        let repo =
            Repository::open(&temp.path.join("queue.sqlite3")).expect("scratch repository opened");
        let archive = temp.path.join("pkg.zip");
        make_zip(&archive, &[("report.txt", b"archive payload")]);
        let extract_dir = temp.path.join("out");
        let obstacle = temp.path.join("out");
        fs::write(&obstacle, b"a plain file, not a dir").expect("obstacle placed");

        // The download record exists and is complete (engine-ordered).
        let download_record = {
            let record = repo
                .insert_download(
                    crate::storage::repository::NewDownload {
                        url: "https://example.test/pkg.zip".to_owned(),
                        destination: archive.clone(),
                        file_name: "pkg.zip".to_owned(),
                    },
                    0,
                )
                .expect("download queued");
            for state in [
                crate::storage::repository::DownloadState::Probing,
                crate::storage::repository::DownloadState::Downloading,
                crate::storage::repository::DownloadState::Finalizing,
                crate::storage::repository::DownloadState::Completed,
            ] {
                repo.update_status(record, state, 0, None)
                    .expect("download walked the ADR-0018 lifecycle");
            }
            record
        };

        let extraction_record = {
            repo.insert_extraction(
                download_record,
                crate::storage::repository::NewExtraction {
                    archive_path: archive.clone(),
                    destination: extract_dir.clone(),
                },
            )
            .expect("extraction queued");
            repo.list_extractions(download_record).expect("listed")[0].id
        };

        let before = read_file(&archive);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime built");

        let first = runtime.block_on(extract_zip(
            &repo,
            extraction_record,
            archive.clone(),
            ExtractionConfig {
                extract_dir: Some(extract_dir.clone()),
                ..ExtractionConfig::default()
            },
        ));
        assert!(
            first.is_err(),
            "the first attempt cannot run while the destination is a plain file"
        );
        let failed = repo
            .get_extraction(extraction_record)
            .expect("extraction read")
            .expect("record exists");
        assert!(
            matches!(
                failed.state,
                crate::storage::repository::ExtractionState::Failed
            ),
            "the extraction record marks the failed attempt"
        );
        let download = repo
            .get_download(download_record)
            .expect("download read")
            .expect("record exists");
        assert_eq!(
            download.status,
            crate::storage::repository::DownloadState::Completed,
            "the download record never flips because extraction failed"
        );
        assert_eq!(read_file(&archive), before, "the archive stayed intact");

        // The obstacle is gone; the retry reuses the same record — no new
        // download, no new extraction row, no network anywhere.
        fs::remove_file(&obstacle).expect("obstacle removed");
        let outcome = runtime
            .block_on(extract_zip(
                &repo,
                extraction_record,
                archive,
                ExtractionConfig {
                    extract_dir: Some(extract_dir.clone()),
                    ..ExtractionConfig::default()
                },
            ))
            .expect("retry succeeds without any download");
        assert_eq!(outcome.files_extracted, 1);
        assert_eq!(
            read_file(&outcome.destination.join("report.txt")),
            b"archive payload",
            "the retry extracted the archive content"
        );
        let completed = repo
            .get_extraction(extraction_record)
            .expect("extraction read")
            .expect("record exists");
        assert!(
            matches!(
                completed.state,
                crate::storage::repository::ExtractionState::Completed
            ),
            "the same record reached `completed` on retry"
        );
        assert!(
            repo.list_extractions(download_record)
                .expect("listed")
                .len()
                == 1,
            "retrying does not duplicate extraction records"
        );
        let download = repo
            .get_download(download_record)
            .expect("download read")
            .expect("record exists");
        assert!(
            matches!(
                download.status,
                crate::storage::repository::DownloadState::Completed
            ),
            "the download record is still completed after the retry"
        );
    }
}
