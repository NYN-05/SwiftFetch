//! Archive engine: safe ZIP extraction (Phase 10).
//!
//! This module implements safe automatic ZIP extraction with the following guarantees:
//! - Runs only after successful download finalization
//! - Validates the archive before/during extraction
//! - Prevents path traversal and unsafe writes outside the extraction destination
//! - Handles existing files without silently overwriting user data
//! - Preserves the downloaded archive if extraction fails
//! - Persists extraction status independently from download status
//! - Allows extraction to be retried without downloading the archive again

pub mod zip_extract;

pub use zip_extract::{extract_zip, ExtractionConfig, ExtractionError, ExtractionOutcome};