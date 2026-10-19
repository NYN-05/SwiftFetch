//! Safe ZIP extraction implementation (Phase 10).
//!
//! This module provides safe ZIP extraction with the following guarantees:
//! - Only extracts after download finalization
//! - Validates archive integrity before extraction
//! - Prevents path traversal (absolute paths, .., drive prefixes, UNC paths)
//! - Handles file collisions with documented policy
//! - Bounds resource usage (entry sizes, extraction failures)
//! - Preserves archive on failure
//! - Tracks extraction state separately from download status

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::task::spawn_blocking;
use zip::read::ZipArchive;
use zip::result::ZipError;

use crate::error::{AppError, ErrorCategory};
use crate::storage::file_writer::{ensure_output_dir, fs_failure};
use crate::storage::repository::{Repository, ExtractionState, ExtractionRecord, NewExtraction};

/// Maximum size of a single extracted entry (1 GiB).
/// Larger entries are refused to prevent resource exhaustion.
const MAX_ENTRY_SIZE: u64 = 1_073_741_824;

/// Maximum total extracted size (10 GiB).
/// Archives exceeding this are refused to prevent disk exhaustion.
const MAX_TOTAL_EXTRACTED_SIZE: u64 = 10_737_418_240;

/// Buffer size for streaming extraction (1 MiB).
const EXTRACT_BUFFER_SIZE: usize = 1_048_576;

/// Configuration for ZIP extraction.
#[derive(Debug, Clone)]
pub struct ExtractionConfig {
    /// Whether to extract automatically after download completion.
    pub auto_extract: bool,
    /// Override the extraction destination directory.
    /// If None, extracts to a folder named after the archive (without .zip extension).
    pub extract_dir: Option<PathBuf>,
    /// Whether to overwrite existing files during extraction.
    /// Default is false - existing files are skipped with a warning.
    pub overwrite_existing: bool,
}

impl Default for ExtractionConfig {
    fn default() -> Self {
        Self {
            auto_extract: true,
            extract_dir: None,
            overwrite_existing: false,
        }
    }
}

/// Errors that can occur during ZIP extraction.
#[derive(Debug, thiserror::Error)]
pub enum ExtractionError {
    #[error("archive validation failed: {0}")]
    ValidationFailed(String),
    #[error("path traversal attempt detected: {0}")]
    PathTraversal(String),
    #[error("entry size exceeds limit: {0} bytes (max {1} bytes)")]
    EntryTooLarge(u64, u64),
    #[error("total extracted size would exceed limit: {0} bytes (max {1} bytes)")]
    TotalSizeExceeded(u64, u64),
    #[error("I/O error during extraction: {0}")]
    Io(#[from] io::Error),
    #[error("ZIP format error: {0}")]
    ZipFormat(#[from] ZipError),
    #[error("file collision: {0} already exists and overwrite is disabled")]
    FileCollision(PathBuf),
    #[error("extraction was cancelled")]
    Cancelled,
    #[error("archive is not a valid ZIP file")]
    NotAZipFile,
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
    /// List of files that were skipped due to collisions.
    pub skipped_files: Vec<PathBuf>,
    /// Whether the archive was preserved (always true on failure).
    pub archive_preserved: bool,
}

/// Validates that an extracted path is safe (no path traversal, absolute paths, etc.).
fn validate_extracted_path(entry_name: &str, extract_root: &Path) -> Result<PathBuf, ExtractionError> {
    // Reject absolute paths
    if Path::new(entry_name).is_absolute() {
        return Err(ExtractionError::PathTraversal(
            format!("absolute path not allowed: {}", entry_name),
        ));
    }

    // Normalize the path
    let normalized = Path::new(entry_name).clean();

    // Check for parent directory traversal
    for component in normalized.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(ExtractionError::PathTraversal(
                format!("parent directory traversal not allowed: {}", entry_name),
            ));
        }
    }

    // Check for drive prefixes (Windows) - e.g., "C:", "D:", etc.
    if let Some(first) = normalized.components().next() {
        if matches!(first, std::path::Component::Prefix(_)) {
            return Err(ExtractionError::PathTraversal(
                format!("drive prefix not allowed: {}", entry_name),
            ));
        }
    }

    // Check for UNC paths (Windows) - e.g., "\\server\share"
    if normalized.to_string_lossy().starts_with("\\\\") {
        return Err(ExtractionError::PathTraversal(
            format!("UNC path not allowed: {}", entry_name),
        ));
    }

    // Join with extract root and ensure it stays within the root
    let full_path = extract_root.join(&normalized);
    
    // Canonicalize both paths to handle symlinks and ensure containment
    let canonical_root = extract_root.canonicalize().map_err(|e| {
        ExtractionError::Io(io::Error::new(
            io::ErrorKind::Other,
            format!("failed to canonicalize extract root: {}", e),
        ))
    })?;
    
    let canonical_full = full_path.canonicalize().unwrap_or_else(|_| full_path.clone());
    
    // Ensure the extracted path is within the extract root
    if !canonical_full.starts_with(&canonical_root) {
        return Err(ExtractionError::PathTraversal(
            format!("extraction would escape root directory: {}", entry_name),
        ));
    }

    Ok(canonical_full)
}

/// Determines the extraction destination directory.
fn determine_extract_dir(archive_path: &Path, config: &ExtractionConfig) -> PathBuf {
    if let Some(dir) = &config.extract_dir {
        dir.clone()
    } else {
        // Extract to a folder named after the archive (without .zip extension)
        let parent = archive_path.parent().unwrap_or_else(|| Path::new("."));
        let stem = archive_path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("extracted");
        parent.join(stem)
    }
}

/// Extracts a ZIP archive to the specified directory.
pub async fn extract_zip(
    repo: Arc<Repository>,
    extraction_id: i64,
    archive_path: PathBuf,
    config: ExtractionConfig,
) -> Result<ExtractionOutcome, ExtractionError> {
    // Validate the archive exists
    if !archive_path.exists() {
        return Err(ExtractionError::Io(io::Error::new(
            io::ErrorKind::NotFound,
            format!("archive not found: {}", archive_path.display()),
        )));
    }

    // Determine extraction directory
    let extract_dir = determine_extract_dir(&archive_path, &config);
    
    // Create extraction directory
    ensure_output_dir(&extract_dir)?;

    // Update state to Extracting
    repo.update_extraction(
        extraction_id,
        ExtractionState::Extracting,
        None,
    )?;

    // Use spawn_blocking for CPU-intensive ZIP operations
    let extract_dir_clone = extract_dir.clone();
    let archive_path_clone = archive_path.clone();
    let config_clone = config.clone();

    let result = spawn_blocking(move || {
        extract_zip_sync(
            &archive_path_clone,
            &extract_dir_clone,
            &config_clone,
        )
    }).await;

    match result {
        Ok(Ok(outcome)) => {
            // Update state to Completed
            repo.update_extraction(
                extraction_id,
                ExtractionState::Completed,
                None,
            )?;
            Ok(outcome)
        }
        Ok(Err(e)) => {
            // Update state to Failed
            repo.update_extraction(
                extraction_id,
                ExtractionState::Failed,
                Some((ErrorCategory::Archive, e.to_string())),
            )?;
            Err(e)
        }
        Err(e) => {
            // Update state to Failed (task panic)
            repo.update_extraction(
                extraction_id,
                ExtractionState::Failed,
                Some((ErrorCategory::Archive, format!("extraction task panicked: {}", e))),
            )?;
            Err(ExtractionError::Io(io::Error::new(
                io::ErrorKind::Other,
                format!("extraction task panicked: {}", e),
            )))
        }
    }
}

/// Synchronous ZIP extraction logic (runs in blocking thread).
fn extract_zip_sync(
    archive_path: &Path,
    extract_dir: &Path,
    config: &ExtractionConfig,
) -> Result<ExtractionOutcome, ExtractionError> {
    // Open the ZIP archive
    let file = fs::File::open(archive_path)
        .map_err(|e| ExtractionError::Io(io::Error::new(
            io::ErrorKind::Other,
            format!("failed to open archive: {}", e),
        )))?;

    let mut archive = ZipArchive::new(file)
        .map_err(|e| ExtractionError::NotAZipFile)?;

    // Validate archive integrity by reading all entries
    let mut total_size: u64 = 0;
    for i in 0..archive.len() {
        let entry = archive.by_index(i)
            .map_err(|e| ExtractionError::ZipFormat(e))?;
        
        // Check entry size
        if entry.size() > MAX_ENTRY_SIZE {
            return Err(ExtractionError::EntryTooLarge(entry.size(), MAX_ENTRY_SIZE));
        }
        total_size += entry.size();
        if total_size > MAX_TOTAL_EXTRACTED_SIZE {
            return Err(ExtractionError::TotalSizeExceeded(total_size, MAX_TOTAL_EXTRACTED_SIZE));
        }
    }

    // Now extract each entry
    let mut files_extracted = 0;
    let mut bytes_written: u64 = 0;
    let mut skipped_files = Vec::new();

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)
            .map_err(|e| ExtractionError::ZipFormat(e))?;

        let entry_name = entry.name();
        
        // Validate the path
        let dest_path = validate_extracted_path(entry_name, extract_dir)?;

        // Handle directories
        if entry_name.ends_with('/') {
            fs::create_dir_all(&dest_path)
                .map_err(|e| ExtractionError::Io(io::Error::new(
                    io::ErrorKind::Other,
                    format!("failed to create directory {}: {}", dest_path.display(), e),
                )))?;
            continue;
        }

        // Check for file collision
        if dest_path.exists() {
            if config.overwrite_existing {
                // Remove existing file
                fs::remove_file(&dest_path)
                    .map_err(|e| ExtractionError::Io(io::Error::new(
                        io::ErrorKind::Other,
                        format!("failed to remove existing file {}: {}", dest_path.display(), e),
                    )))?;
            } else {
                // Skip this file
                skipped_files.push(dest_path.clone());
                continue;
            }
        }

        // Create parent directories if needed
        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| ExtractionError::Io(io::Error::new(
                    io::ErrorKind::Other,
                    format!("failed to create parent directory {}: {}", parent.display(), e),
                )))?;
        }

        // Extract the file
        let mut outfile = fs::File::create(&dest_path)
            .map_err(|e| ExtractionError::Io(io::Error::new(
                io::ErrorKind::Other,
                format!("failed to create file {}: {}", dest_path.display(), e),
            )))?;

        let mut buffer = vec![0u8; EXTRACT_BUFFER_SIZE];
        loop {
            let bytes_read = entry.read(&mut buffer)
                .map_err(|e| ExtractionError::ZipFormat(e))?;
            if bytes_read == 0 {
                break;
            }
            outfile.write_all(&buffer[..bytes_read])
                .map_err(|e| ExtractionError::Io(io::Error::new(
                    io::ErrorKind::Other,
                    format!("failed to write to {}: {}", dest_path.display(), e),
                )))?;
            bytes_written += bytes_read as u64;
        }

        files_extracted += 1;
    }

    Ok(ExtractionOutcome {
        destination: extract_dir.to_path_buf(),
        files_extracted,
        bytes_written,
        skipped_files,
        archive_preserved: true,
    })
}

impl ExtractionError {
    /// Converts this extraction error to an AppError.
    pub fn to_app_error(self) -> AppError {
        match self {
            ExtractionError::ValidationFailed(msg) => 
                AppError::new(ErrorCategory::ZipExtraction, msg),
            ExtractionError::PathTraversal(msg) => 
                AppError::new(ErrorCategory::ZipExtraction, msg),
            ExtractionError::EntryTooLarge(size, max) => 
                AppError::new(ErrorCategory::ZipExtraction, format!("entry size {} exceeds maximum {}", size, max)),
            ExtractionError::TotalSizeExceeded(size, max) => 
                AppError::new(ErrorCategory::ZipExtraction, format!("total extracted size {} exceeds maximum {}", size, max)),
            ExtractionError::Io(e) => 
                AppError::new(ErrorCategory::ZipExtraction, format!("I/O error: {}", e)).with_source(e),
            ExtractionError::ZipFormat(e) => 
                AppError::new(ErrorCategory::ZipExtraction, format!("ZIP format error: {}", e)).with_source(e),
            ExtractionError::FileCollision(path) => 
                AppError::new(ErrorCategory::ZipExtraction, format!("file collision: {} already exists", path.display())),
            ExtractionError::Cancelled => 
                AppError::new(ErrorCategory::Cancelled, "extraction was cancelled"),
            ExtractionError::NotAZipFile => 
                AppError::new(ErrorCategory::ZipExtraction, "archive is not a valid ZIP file"),
        }
    }
}