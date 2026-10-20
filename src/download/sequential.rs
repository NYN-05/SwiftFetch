//! Sequential transfer engine (Phase 4).
//!
//! Streams one HTTP/HTTPS resource to disk in bounded chunks, then
//! finalizes safely:
//!
//! 1. [`probe`] resolves the effective URL and records response-backed
//!    facts (size claim, validators, redirects).
//! 2. The file name is chosen deterministically from the *effective* URL
//!    and sanitized for Windows' rules ([`select_filename`]).
//! 3. The whole resource is requested through the shared client. Only a
//!    `200 OK` response may write file bytes — error statuses produce an
//!    error before any file exists, so HTTP error bodies can never become
//!    file content.
//! 4. Body chunks are read with the client's idle policy
//!    ([`HttpClient::read_chunk`]) and written to an engine-owned partial
//!    file, counting bytes **received** and bytes **written** separately.
//! 5. When a size claim exists (the transfer's own `Content-Length`, or the
//!    probe's recorded fact when the transfer stays silent), the written
//!    count must match it exactly before finalization. With no claim at
//!    all, the server-defined end of stream *is* the completion criterion —
//!    no length is invented. A contradiction between transfer and probe is
//!    refused, never silently resolved.
//! 6. Finalization ([`crate::storage::file_writer`]) syncs the partial
//!    file, claims the final name without overwriting anything, and
//!    same-volume-renames the validated partial file onto it.
//!
//! Failures preserve the partial file
//! (`<name>.swiftfetch-<pid>-<n>.part`) and its diagnostic value in place
//! for the later recovery phase; no final file name is ever produced for an
//! incomplete or failed transfer.
//!
//! Probe facts constrain transfer decisions here: a size claim contradicting
//! the live response is an error, validators recorded for later phases come
//! from the final transfer response when it supplies them (it is the
//! representation actually written), otherwise from the probe.
//!
//! The filename derivation rules and the no-overwrite finalization order
//! are policy, recorded as ADR-0016.

// Phase 4 wiring note: commands wire this engine in Phase 11 (phases.md),
// so the items are exercised by tests and stay `dead_code`-waived until
// then — the sanctioned narrow waiver pattern (see AGENTS.md).
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use crate::download::http::{
    bytes_look_like_html, content_type_is_html, http_status_error, landing_page_error, HttpClient,
};
use crate::download::probe::{
    probe, response_meta, validate_url, ContentEncoding, ContentLength, ProbeResult,
};
use crate::download::progress::Progress;
use crate::error::{AppError, ErrorCategory};
use crate::logging::redact_url;
use crate::storage::file_writer::{
    cap_units, finalize, reserve_destination, PartialFile, MAX_COMPONENT_UNITS,
};
use reqwest::{Method, StatusCode, Url};

/// Deterministic fallback for URLs with no usable final path segment.
///
/// A fixed constant (not a timestamp, not a random value) so naming stays
/// deterministic; repeated downloads of segment-less URLs land on
/// `(n)`-suffixed siblings instead of colliding.
const FALLBACK_FILE_NAME: &str = "download";

/// Reserved Windows device names, matched before any extension and
/// case-insensitively (per Microsoft's file-naming documentation,
/// `con.txt` is as unusable as `CON`). `COM0`/`LPT0` are **not** reserved
/// (only 1–9 are).
const WINDOWS_RESERVED_STEMS: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// The record of one completed sequential transfer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SequentialDownload {
    /// Final absolute path of the completed file (may carry a collision
    /// suffix different from `file_name`).
    pub destination: PathBuf,
    /// The sanitized file name chosen for the resource.
    pub file_name: String,
    /// Bytes that reached the finalized file.
    pub bytes_written: u64,
    /// The size claim the byte count was validated against, when one
    /// existed (never invented).
    pub expected_size: Option<u64>,
    /// Raw `ETag` of the final representation, else the probe's.
    pub etag: Option<String>,
    /// Raw `Last-Modified` of the final representation, else the probe's.
    pub last_modified: Option<String>,
    /// The effective URL after permitted redirects (already redact-safe
    /// only after [`crate::logging::redact_url`]; recorded raw for resume).
    pub effective_url: String,
}

/// Runs one sequential transfer of `url` into `output_dir`.
///
/// The caller owns `progress` (typically a [`Progress::shared`] handle) and
/// keeps observing it after failures, when the byte accounting is exactly
/// what diagnostics need. Returns the completed-transfer record.
pub(crate) async fn download_sequential(
    client: &HttpClient,
    url: &str,
    output_dir: &Path,
    progress: &Progress,
) -> Result<SequentialDownload, AppError> {
    let facts: ProbeResult = probe(client, url).await?;
    let effective_url = validate_url(&facts.effective_url)?;
    let redacted = redact_url(&facts.effective_url);
    tracing::info!(
        "sequential transfer of {}: probed size {}",
        redacted,
        facts
            .expected_size
            .map_or_else(|| "unknown".to_owned(), |size| size.to_string()),
    );

    let file_name = select_filename(&effective_url);
    tracing::debug!("destination file name: {file_name}");

    // The full resource, no Range header: a `200` is the only response
    // shape whose body is this resource's content. Anything else —
    // 404, 5xx, or an unsolicited `206` — never writes a byte.
    let mut response = client.send(Method::GET, &effective_url, None).await?;
    let status = response.status();
    if status != StatusCode::OK {
        return Err(http_status_error(
            status,
            format!("the remote server responded HTTP {status} for {redacted}"),
            Some(response.headers()),
        ));
    }

    // One header parse, one trust policy: reuse the probe's strict parser
    // for this transfer's own response.
    let meta = response_meta(&response);
    if meta.encoding == ContentEncoding::Transform {
        return Err(AppError::new(
            ErrorCategory::RangeValidation,
            format!(
                "the transfer of {redacted} declared a content transformation \
                 (Content-Encoding), which cannot be written as the resource's \
                 bytes; the transfer is refused"
            ),
        ));
    }
    let expected_size = expected_size_of(meta.cl, facts.expected_size, &redacted)?;

    // Bounded landing-page inspection: exactly one buffered read — no
    // unbounded sniffing, and it consumes nobody else's stream, because
    // these bytes are the transfer's own first content bytes and are
    // written as such. When they begin like HTML markup, the response was
    // a landing page and nothing is ever written to disk: the refusal
    // happens before the partial file exists. A server that merely
    // mislabels a real file with an HTML media type still passes (the body
    // does not begin like markup), because `Content-Type` alone is not
    // proof.
    let mut first_chunk = match client.read_chunk(&mut response).await {
        Ok(chunk) => chunk,
        Err(err) => {
            return Err(with_transfer_context(err, 0, expected_size, &redacted));
        }
    };
    if first_chunk
        .as_ref()
        .is_some_and(|bytes| bytes_look_like_html(bytes))
    {
        return Err(landing_page_error(
            &redacted,
            content_type_is_html(response.headers()),
        ));
    }

    // Only after the response proved to be content does anything exist on
    // disk: directories and the engine-owned partial file come now, so the
    // rejected paths above leave no filesystem trace at all.
    let mut partial = PartialFile::create(output_dir, &file_name).await?;

    let mut written: u64 = 0;
    // The buffered inspection read is real content: write it first, so the
    // byte account stays complete.
    if let Some(chunk) = first_chunk.take() {
        let size = chunk.len() as u64;
        progress.record_received(size);
        if let Err(err) = partial.write_chunk(chunk.as_ref()).await {
            return Err(with_transfer_context(
                err,
                written,
                expected_size,
                &redacted,
            ));
        }
        written += size;
        progress.record_written(size);
    }
    loop {
        // Reads and writes stay bounded to one chunk each and are counted
        // separately; both failure kinds report the byte accounting.
        let chunk = match client.read_chunk(&mut response).await {
            Ok(chunk) => chunk,
            Err(err) => {
                return Err(with_transfer_context(
                    err,
                    written,
                    expected_size,
                    &redacted,
                ));
            }
        };
        let Some(chunk) = chunk else {
            break;
        };
        let size = chunk.len() as u64;
        progress.record_received(size);
        if let Err(err) = partial.write_chunk(chunk.as_ref()).await {
            return Err(with_transfer_context(
                err,
                written,
                expected_size,
                &redacted,
            ));
        }
        written += size;
        progress.record_written(size);
    }

    if let Some(expected) = expected_size {
        if written != expected {
            // The partial data stays in place for diagnostics and the later
            // recovery phase; this is exactly the "no final name for
            // incomplete data" invariant.
            return Err(AppError::new(
                ErrorCategory::RangeValidation,
                format!(
                    "the transfer of {redacted} ended after {written} of the \
                     expected {expected} bytes; the file is not complete and \
                     will not be finalized"
                ),
            ));
        }
    }

    let reserved = reserve_destination(output_dir, &file_name)?;
    let destination = finalize(partial, reserved).await?;
    tracing::info!(
        "downloaded {written} bytes to {} from {}",
        destination.display(),
        redacted
    );

    Ok(SequentialDownload {
        destination,
        file_name,
        bytes_written: written,
        expected_size,
        // The final response's validators describe the bytes actually
        // written; the probe's values only fill an absent field.
        etag: meta.etag.or(facts.etag),
        last_modified: meta.last_modified.or(facts.last_modified),
        effective_url: facts.effective_url,
    })
}

/// Chooses the trusted expected size of the current transfer:
///
/// - the transfer's own `Content-Length`, when present and valid (the
///   strongest claim about *this* body);
/// - otherwise the probe's recorded size fact (HEAD/`206` metadata);
/// - a contradiction between the two is never silently resolved — the
///   transfer is refused, because at least one response lied.
pub(crate) fn expected_size_of(
    transfer: ContentLength,
    probed: Option<u64>,
    redacted_url: &str,
) -> Result<Option<u64>, AppError> {
    let transfer_size = match transfer {
        ContentLength::Valid(size) => Some(size),
        ContentLength::Missing | ContentLength::Invalid => None,
    };
    match (transfer_size, probed) {
        (Some(current), Some(earlier)) if current != earlier => Err(AppError::new(
            ErrorCategory::RangeValidation,
            format!(
                "the transfer of {redacted_url} contradicts the probed \
                 metadata: the current Content-Length is {current}, the probe \
                 recorded {earlier}; no length fact survives and the transfer \
                 is refused"
            ),
        )),
        (Some(current), _) => Ok(Some(current)),
        (None, earlier) => Ok(earlier),
    }
}

/// Composes a mid-transfer failure with the received/written accounting so
/// diagnostics always state how much data was actually durable.
pub(crate) fn with_transfer_context(
    err: AppError,
    written: u64,
    expected: Option<u64>,
    redacted_url: &str,
) -> AppError {
    let accounting = match expected {
        Some(expected) => {
            format!("{written} of {expected} expected bytes were written to the partial file")
        }
        None => format!("{written} bytes were written to the partial file"),
    };
    let message = format!(
        "the transfer of {redacted_url} stopped: {accounting}; {}",
        err.message()
    );
    AppError::new(err.category(), message).with_source(err)
}

/// Deterministically selects the destination file name for a resource.
///
/// The effective URL's final path segment is the name, percent-decoded;
/// query strings are dropped by construction. Anything unusable — missing
/// segment, a name sanctioned down to empty by the sanitizer, `.`/`..` —
/// falls back to [`FALLBACK_FILE_NAME`]. The caller owns `select_filename`'s
/// stability: it is the *effective* (post-redirect) URL that names the file.
pub(crate) fn select_filename(effective_url: &Url) -> String {
    let segment = effective_url
        .path()
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .map(percent_decode);
    segment
        .as_deref()
        .and_then(sanitize_windows_name)
        .unwrap_or_else(|| FALLBACK_FILE_NAME.to_owned())
}

/// Sanitizes a raw name for Windows' file-system rules:
///
/// - characters invalid in NTFS names (`< > : " / \ | ? *`) and control
///   codes (`U+0000`–`U+001F`, DEL) become `_`;
/// - trailing dots and spaces are stripped (illegal at a component end);
/// - reserved device names ([`WINDOWS_RESERVED_STEMS`], before any
///   extension, case-insensitive) gain a `_` prefix;
/// - the result is truncated to at most [`MAX_COMPONENT_UNITS`] UTF-16 code
///   units — the Windows per-component limit — preserving the extension
///   whenever it fits;
/// - empty results are rejected (`None` → the caller's fallback).
fn sanitize_windows_name(name: &str) -> Option<String> {
    let replaced: String = name
        .chars()
        .map(|c| {
            if is_invalid_windows_character(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let stripped = replaced.trim_end_matches(['.', ' ']);
    if stripped.is_empty() {
        return None;
    }
    let neutralized = neutralize_reserved_name(stripped);
    Some(cap_to_windows_limit(&neutralized))
}

/// Windows-filename-invalid characters: the named set plus control codes.
/// Space *is* legal inside a component (only trailing spaces are not).
fn is_invalid_windows_character(c: char) -> bool {
    matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control()
}

/// Prefixes a `safe-`-style `_` in front of reserved device stems.
fn neutralize_reserved_name(name: &str) -> String {
    let stem = name.split('.').next().unwrap_or_default();
    let reserved = WINDOWS_RESERVED_STEMS
        .iter()
        .any(|candidate| stem.eq_ignore_ascii_case(candidate));
    if reserved {
        format!("_{name}")
    } else {
        name.to_owned()
    }
}

/// Caps a name at the Windows per-component limit, preserving the extension
/// when it leaves room for stem data, cutting whole characters only.
fn cap_to_windows_limit(name: &str) -> String {
    if name.encode_utf16().count() <= MAX_COMPONENT_UNITS {
        return name.to_owned();
    }
    if let Some((stem, ext)) = name.rsplit_once('.') {
        if !stem.is_empty() && ext.encode_utf16().count() + 1 < MAX_COMPONENT_UNITS {
            let extension_units = ext.encode_utf16().count() + 1;
            let capped = cap_units(stem, extension_units);
            if !capped.is_empty() {
                return format!("{capped}.{ext}");
            }
        }
    }
    cap_units(name, 0)
}

/// Decodes the percent-encoded bytes of one path segment (RFC 3986).
///
/// Malformed escapes stay literal; `+` is deliberately *not* decoded (it
/// has no space meaning in paths); the byte sequence becomes text lossily
/// (invalid UTF-8 → U+FFFD), which [`sanitize_windows_name`] then
/// normalizes — feeding the decoder's output to the sanitizer is the
/// safety order: a decoded `/` must never survive as a path separator.
fn percent_decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escape = bytes[index] == b'%'
            && index + 3 <= bytes.len()
            && (bytes[index + 1] as char).is_ascii_hexdigit()
            && (bytes[index + 2] as char).is_ascii_hexdigit();
        if escape {
            let high = (bytes[index + 1] as char)
                .to_digit(16)
                .expect("checked hex");
            let low = (bytes[index + 2] as char)
                .to_digit(16)
                .expect("checked hex");
            decoded.push(((high * 16) + low) as u8);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{download_sequential, expected_size_of, select_filename};
    use crate::common::fixture::Fixture;
    use crate::common::server::{Behavior, MockServer, RangeMode, Validators};
    use crate::download::http::HttpClient;
    use crate::download::probe::ContentLength;
    use crate::download::progress::Progress;
    use crate::error::ErrorCategory;
    use reqwest::Url;
    use std::path::{Path, PathBuf};

    const DEFAULT_LEN: u64 = 4_096;

    /// The default production client.
    fn client() -> HttpClient {
        HttpClient::default_client().expect("default client builds")
    }

    fn url_of(path: &str) -> Url {
        format!("http://127.0.0.1:1{path}")
            .parse()
            .expect("test url builds")
    }

    /// An isolated, auto-removed scratch output directory.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(tag: &str) -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos());
            let dir = std::env::temp_dir().join(format!(
                "swiftfetch-p4-seq-{tag}-{}-{stamp}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("scratch dir created");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(self.0.join("."))
                .expect("scratch dir listed")
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }

        fn partial_paths(&self) -> Vec<PathBuf> {
            self.names()
                .into_iter()
                .filter(|name| name.ends_with(".part"))
                .map(|name| self.0.join(name))
                .collect()
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // -- filename selection -----------------------------------------------------------

    #[test]
    fn the_effective_path_names_the_file_and_drops_the_query() {
        assert_eq!(
            select_filename(&url_of("/releases/app-v2.zip?issue=9")),
            "app-v2.zip"
        );
        assert_eq!(select_filename(&url_of("/files/report.bin")), "report.bin");
        assert_eq!(select_filename(&url_of("/plain")), "plain");
    }

    #[test]
    fn percent_encoded_names_decode_into_their_characters() {
        assert_eq!(select_filename(&url_of("/my%20file.zip")), "my file.zip");
        assert_eq!(
            select_filename(&url_of("/caf%C3%A9-notes.txt")),
            "café-notes.txt"
        );
    }

    #[test]
    fn decoded_path_separators_never_reach_the_filesystem() {
        // `%2F` decodes to `/`, and the sanitizer must neutralize it.
        assert_eq!(
            select_filename(&url_of("/plain%2Fname.txt")),
            "plain_name.txt"
        );
        assert_eq!(select_filename(&url_of("/a%5Cb.bin")), "a_b.bin");
    }

    #[test]
    fn windows_invalid_characters_are_replaced() {
        // Escaped before parsing so the URL keeps them in the path. Each of
        // `<`, `>`, `"`, `|` becomes one `_`, and the mock-encoding even
        // keeps the raw spaces between them.
        assert_eq!(
            select_filename(&url_of("/bad%3C%20%3E%20%22%20%7Cname.txt")),
            "bad_ _ _ _name.txt",
            "<, >, \", | each become one underscore"
        );
        assert_eq!(select_filename(&url_of("/drive%3Apath")), "drive_path");
        assert_eq!(select_filename(&url_of("/bad%3Fname.zip")), "bad_name.zip");
        assert_eq!(select_filename(&url_of("/ctrl%01name")), "ctrl_name");
        assert_eq!(select_filename(&url_of("/del%7Fname")), "del_name");
    }

    #[test]
    fn trailing_dots_and_spaces_are_stripped() {
        assert_eq!(select_filename(&url_of("/report.%20.%20")), "report");
        assert_eq!(select_filename(&url_of("/name.zip.")), "name.zip");
        assert_eq!(
            select_filename(&url_of("/name .zip")),
            "name .zip",
            "inner spaces stay"
        );
    }

    #[test]
    fn reserved_device_names_are_neutralized() {
        assert_eq!(select_filename(&url_of("/con")), "_con");
        assert_eq!(select_filename(&url_of("/CON")), "_CON");
        assert_eq!(select_filename(&url_of("/nul.zip")), "_nul.zip");
        assert_eq!(select_filename(&url_of("/Com8")), "_Com8");
        assert_eq!(select_filename(&url_of("/lpt9.bin")), "_lpt9.bin");
        assert_eq!(
            select_filename(&url_of("/aux")),
            "_aux",
            "a reserved stem without extension is neutralized too"
        );
    }

    #[test]
    fn lookalike_names_that_are_not_reserved_stay_untouched() {
        assert_eq!(select_filename(&url_of("/conx.txt")), "conx.txt");
        assert_eq!(select_filename(&url_of("/com0")), "com0");
        assert_eq!(select_filename(&url_of("/companion")), "companion");
        assert_eq!(select_filename(&url_of("/auxiliary")), "auxiliary");
        assert_eq!(select_filename(&url_of("/connectivity")), "connectivity");
    }

    #[test]
    fn missing_or_degenerate_names_fall_back_deterministically() {
        assert_eq!(select_filename(&url_of("/")), "download");
        assert_eq!(
            select_filename(&url_of("/%2E%2E")),
            "download",
            "`..` is rejected"
        );
        assert_eq!(select_filename(&url_of("/.")), "download");
        assert_eq!(select_filename(&url_of("//")), "download");
        assert_eq!(
            select_filename(&url_of("/%3F")),
            "_",
            "a name made only of replaced characters stays itself (`?`→`_`), \
             not the fallback"
        );
    }

    #[test]
    fn very_long_names_truncate_within_the_windows_component_limit() {
        let long = format!("/{}.zip", "a".repeat(400));
        let name = select_filename(&url_of(&long));
        assert_eq!(name.encode_utf16().count(), 255);
        assert!(
            name.starts_with("aaa"),
            "stem data is preserved from the front"
        );
        assert!(name.ends_with(".zip"), "the extension survives truncation");

        // Astral characters must never be cut in half.
        let astral = format!("/{}", "🎂".repeat(130));
        let name = select_filename(&url_of(&astral));
        assert!(name.encode_utf16().count() <= 255);
        assert!(
            name.chars().all(|c| c == '🎂'),
            "only whole characters survive"
        );
    }

    // -- expected size decision -------------------------------------------------------

    #[test]
    fn the_transfer_claim_wins_when_it_agrees_or_the_probe_was_silent() {
        assert_eq!(
            expected_size_of(ContentLength::Valid(4_096), None, "&x").expect("no conflict"),
            Some(4_096)
        );
        assert_eq!(
            expected_size_of(ContentLength::Valid(4_096), Some(4_096), "&x")
                .expect("agreement is not a conflict"),
            Some(4_096)
        );
        assert_eq!(
            expected_size_of(ContentLength::Invalid, Some(2_048), "&x").expect("probe still holds"),
            Some(2_048)
        );
        assert_eq!(
            expected_size_of(ContentLength::Missing, None, "&x").expect("no claim at all"),
            None
        );
    }

    #[test]
    fn a_length_contradiction_between_probe_and_transfer_is_refused() {
        let err = expected_size_of(ContentLength::Valid(4_096), Some(8_192), "&x")
            .expect_err("contradicting length facts cannot both be true");
        assert_eq!(err.category(), ErrorCategory::RangeValidation);
        assert!(
            err.message().contains("4_096") || err.message().contains("4096"),
            "both facts must be named: {}",
            err.message()
        );
    }

    // -- full transfers over the controlled mock server ---------------------------------

    #[tokio::test]
    async fn a_small_download_is_byte_exact_and_counters_match() {
        let server = MockServer::start().await;
        server.mount_route("/plain", Behavior::default());
        let url = server.url("/plain");
        let dir = ScratchDir::new("small");
        let progress = Progress::shared();

        let outcome = download_sequential(&client(), &url, dir.path(), &progress)
            .await
            .expect("the download completes");

        assert_eq!(outcome.destination, dir.path().join("plain"));
        assert_eq!(outcome.file_name, "plain");
        assert_eq!(outcome.expected_size, Some(DEFAULT_LEN));
        assert_eq!(outcome.bytes_written, DEFAULT_LEN);
        assert_eq!(outcome.effective_url, url);
        assert_eq!(outcome.etag, None);
        assert_eq!(outcome.last_modified, None);

        let reference = Fixture::default().bytes();
        let written = std::fs::read(dir.path().join("plain")).expect("final file exists");
        assert_eq!(written.as_slice(), reference.as_slice());

        assert_eq!(progress.received(), DEFAULT_LEN);
        assert_eq!(progress.written(), DEFAULT_LEN);
        // Empirically, reqwest may hand a whole small (length-delimited)
        // body over in a single poll, so a "many chunks" shape cannot be
        // asserted here — the streaming-not-buffering property is proven
        // by the 4 MiB transfer test, where one poll even hitting ~64 KiB
        // socket buffers cannot carry a whole body.

        // Exactly the final file: no partial file or reservation survives.
        assert_eq!(dir.names(), ["plain"]);
        // Probing (HEAD + ranged GET) plus the one full transfer.
        assert_eq!(server.route_hits("/plain"), 3);
    }

    #[tokio::test]
    async fn a_large_download_is_byte_exact_and_streamed_in_chunks() {
        let fixture = Fixture::new(9, 4_194_304);
        let server = MockServer::start().await;
        server.mount_route("/large.bin", Behavior::default().with_fixture(fixture));
        let dir = ScratchDir::new("large");
        let progress = Progress::shared();

        let outcome =
            download_sequential(&client(), &server.url("/large.bin"), dir.path(), &progress)
                .await
                .expect("the large download completes");

        assert_eq!(outcome.expected_size, Some(4_194_304));
        assert_eq!(outcome.bytes_written, 4_194_304);
        let reference = fixture.bytes();
        let written = std::fs::read(dir.path().join("large.bin")).expect("final file exists");
        assert_eq!(written.as_slice(), reference.as_slice());
        assert_eq!(progress.received(), 4_194_304);
        assert_eq!(progress.written(), 4_194_304);
        assert!(
            progress.largest_single_write() < 4_194_304,
            "streaming, not whole-file buffering (largest single write: {})",
            progress.largest_single_write()
        );
        assert_eq!(dir.names(), ["large.bin"]);
    }

    #[tokio::test]
    async fn a_transfer_without_any_length_completes_at_stream_end() {
        let server = MockServer::start().await;
        server.mount_route(
            "/fixture",
            Behavior::default()
                .with_range(RangeMode::Ignore)
                .with_omit_content_length(true)
                .with_head_length_omitted(),
        );
        let dir = ScratchDir::new("no-length");
        let progress = Progress::shared();

        let outcome =
            download_sequential(&client(), &server.url("/fixture"), dir.path(), &progress)
                .await
                .expect("a chunked transfer completes at the server-defined end");

        assert_eq!(
            outcome.expected_size, None,
            "no length was invented when no response declared one"
        );
        assert_eq!(outcome.bytes_written, DEFAULT_LEN);
        let reference = Fixture::default().bytes();
        let written = std::fs::read(dir.path().join("fixture")).expect("final file exists");
        assert_eq!(written.as_slice(), reference.as_slice());
        assert_eq!(progress.received(), DEFAULT_LEN);
        assert_eq!(progress.written(), DEFAULT_LEN);
    }

    #[tokio::test]
    async fn a_silent_transfer_is_validated_against_the_probed_size() {
        let server = MockServer::start().await;
        // The transfer body streams without `Content-Length`; the probe's
        // metadata (HEAD + Content-Range) supplies the size claim, and the
        // byte count must satisfy it before finalization.
        server.mount_route(
            "/fixture",
            Behavior::default().with_omit_content_length(true),
        );
        let dir = ScratchDir::new("probe-fallback");
        let progress = Progress::shared();

        let outcome =
            download_sequential(&client(), &server.url("/fixture"), dir.path(), &progress)
                .await
                .expect("the probe's size fact validates the silent transfer");

        assert_eq!(outcome.expected_size, Some(DEFAULT_LEN));
        assert_eq!(outcome.bytes_written, DEFAULT_LEN);
        assert_eq!(progress.written(), DEFAULT_LEN);
        assert_eq!(dir.names(), ["fixture"]);
    }

    #[tokio::test]
    async fn a_truncated_transfer_never_finalizes() {
        let server = MockServer::start().await;
        server.mount_route("/fixture", Behavior::default().with_truncate_after(2_000));
        let dir = ScratchDir::new("truncated");
        let progress = Progress::shared();

        let err = download_sequential(&client(), &server.url("/fixture"), dir.path(), &progress)
            .await
            .expect_err("a mid-transfer connection drop cannot produce a final file");

        assert_eq!(err.category(), ErrorCategory::Http);

        assert!(
            !dir.path().join("fixture").exists(),
            "no file may appear under the final name after a failed transfer"
        );
        let partials = dir.partial_paths();
        assert!(partials.len() <= 1, "at most one engine-owned partial file");
        // Two honest failure shapes: (a) the abrupt close poisoned the
        // pooled probe connection and the next request failed before any
        // byte or file existed; (b) the transfer started fine and the body
        // read failed mid-stream, leaving the partial file. Both are
        // bounded and finalization-free, and the accounting never drifts.
        if partials.is_empty() {
            assert_eq!(
                (progress.received(), progress.written()),
                (0, 0),
                "a failure before any body read leaves no bytes at all"
            );
        } else {
            let prefix_len = std::fs::metadata(&partials[0])
                .expect("partial file exists")
                .len();
            assert_eq!(
                prefix_len,
                progress.written(),
                "the counters and the disk agree exactly"
            );
            assert_eq!(
                progress.received(),
                progress.written(),
                "no write fell behind the network"
            );
            // Kernel buffers may drop in-flight data at an abrupt close, so
            // the prefix is bounded by — not guaranteed to reach — the
            // advertised length.
            assert!(progress.written() <= 2_000);
            if progress.written() > 0 {
                assert!(
                    err.message()
                        .contains(&format!("stopped: {} of 4096 expected", progress.written())),
                    "the error must state the durable byte count: {}",
                    err.message()
                );
            }
        }
    }

    #[tokio::test]
    async fn an_error_status_never_writes_the_error_body() {
        let server = MockServer::start().await;
        // The probe survives (HEAD facts kept, ranged evidence degraded);
        // the transfer must then refuse the 500 sentinel — and, seen from
        // the directory, nothing may exist at all.
        server.mount_route("/fixture", Behavior::default().with_get_status(500));
        let dir = ScratchDir::new("error-status");
        let progress = Progress::shared();

        let err = download_sequential(&client(), &server.url("/fixture"), dir.path(), &progress)
            .await
            .expect_err("a 500 transfer response is never file content");

        assert_eq!(err.category(), ErrorCategory::Http);
        assert!(
            err.message().contains("500"),
            "the status must be reported: {}",
            err.message()
        );
        assert_eq!(
            dir.names(),
            Vec::<String>::new(),
            "no file — let alone a sentinel body — was written"
        );
        assert_eq!(
            (progress.received(), progress.written()),
            (0, 0),
            "refusal happens before any body byte is read"
        );
    }

    #[tokio::test]
    async fn a_content_transformation_is_refused_before_anything_is_written() {
        let server = MockServer::start().await;
        server.mount_route(
            "/fixture",
            Behavior::default().with_content_encoding("gzip"),
        );
        let dir = ScratchDir::new("transform");
        let progress = Progress::shared();

        let err = download_sequential(&client(), &server.url("/fixture"), dir.path(), &progress)
            .await
            .expect_err("a transformed representation is not the resource's bytes");

        assert_eq!(err.category(), ErrorCategory::RangeValidation);
        assert!(
            err.message().contains("content transformation"),
            "the reason must be actionable: {}",
            err.message()
        );
        assert_eq!(
            dir.names(),
            Vec::<String>::new(),
            "the transfer refused before creating any file"
        );
        assert_eq!(
            (progress.received(), progress.written()),
            (0, 0),
            "refusal happens before any body byte is read"
        );
    }

    #[tokio::test]
    async fn existing_files_are_never_overwritten_by_a_download() {
        let server = MockServer::start().await;
        server.mount_route("/fixture", Behavior::default());
        let dir = ScratchDir::new("no-overwrite");
        std::fs::write(dir.path().join("fixture"), b"USER-ONE").expect("seed file");
        std::fs::write(dir.path().join("fixture (1)"), b"USER-TWO").expect("seed file");
        let progress = Progress::shared();

        let outcome =
            download_sequential(&client(), &server.url("/fixture"), dir.path(), &progress)
                .await
                .expect("the download finalizes under a sibling name");

        assert_eq!(
            outcome.destination,
            dir.path().join("fixture (2)"),
            "the deterministic collision policy picks the next free name"
        );
        assert_eq!(
            std::fs::read(dir.path().join("fixture")).expect("user file intact"),
            b"USER-ONE"
        );
        assert_eq!(
            std::fs::read(dir.path().join("fixture (1)")).expect("user file intact"),
            b"USER-TWO"
        );
        let reference = Fixture::default().bytes();
        let written = std::fs::read(dir.path().join("fixture (2)")).expect("final file");
        assert_eq!(written.as_slice(), reference.as_slice());
        assert_eq!(dir.names(), ["fixture", "fixture (1)", "fixture (2)"]);
    }

    #[tokio::test]
    async fn repeated_downloads_of_one_url_land_on_deterministic_siblings() {
        let server = MockServer::start().await;
        server.mount_route("/package.zip", Behavior::default());
        let dir = ScratchDir::new("deterministic-siblings");
        let progress = Progress::shared();

        let first = download_sequential(
            &client(),
            &server.url("/package.zip"),
            dir.path(),
            &progress,
        )
        .await
        .expect("first download completes");
        let second = download_sequential(
            &client(),
            &server.url("/package.zip"),
            dir.path(),
            &progress,
        )
        .await
        .expect("second download completes");

        assert_eq!(first.destination, dir.path().join("package.zip"));
        assert_eq!(
            second.destination,
            dir.path().join("package (1).zip"),
            "the policy extends the stem, keeping the extension"
        );
        let reference = Fixture::default().bytes();
        let second_bytes = std::fs::read(dir.path().join("package (1).zip")).expect("sibling file");
        assert_eq!(second_bytes.as_slice(), reference.as_slice());
        assert_eq!(dir.names(), ["package (1).zip", "package.zip"]);
    }

    #[tokio::test]
    async fn a_missing_output_directory_is_created() {
        let server = MockServer::start().await;
        server.mount_route("/fixture", Behavior::default());
        let dir = ScratchDir::new("created-dir");
        let nested = dir.path().join("downloads/nested");
        let progress = Progress::shared();

        let outcome = download_sequential(&client(), &server.url("/fixture"), &nested, &progress)
            .await
            .expect("the engine creates the requested directory");

        assert_eq!(outcome.destination, nested.join("fixture"));
        assert!(nested.is_dir(), "the output directory tree now exists");
    }

    #[tokio::test]
    async fn an_output_directory_path_held_by_a_file_is_a_filesystem_error() {
        let server = MockServer::start().await;
        server.mount_route("/fixture", Behavior::default());
        let dir = ScratchDir::new("occupied-dir");
        std::fs::write(dir.path().join("occupied"), b"this is not a directory").expect("seed file");
        let target = dir.path().join("occupied");
        let progress = Progress::shared();

        let err = download_sequential(&client(), &server.url("/fixture"), &target, &progress)
            .await
            .expect_err("a file cannot serve as the output directory");

        assert_eq!(err.category(), ErrorCategory::Filesystem);
        assert!(
            err.message().contains("occupied"),
            "the error names the failing path: {}",
            err.message()
        );
    }

    #[tokio::test]
    async fn validators_come_from_the_final_transfer_response() {
        let server = MockServer::start().await;
        server.mount_route(
            "/fixture",
            Behavior::default()
                .with_etag(Validators::Static)
                .with_last_modified(Validators::Static),
        );
        let dir = ScratchDir::new("validators");
        let progress = Progress::shared();

        let outcome =
            download_sequential(&client(), &server.url("/fixture"), dir.path(), &progress)
                .await
                .expect("the download completes");

        assert_eq!(outcome.etag.as_deref(), Some("\"fixture-1-4096\""));
        assert_eq!(
            outcome.last_modified.as_deref(),
            Some("Wed, 21 Oct 2015 07:28:00 GMT")
        );
    }

    #[tokio::test]
    async fn changing_remote_objects_are_observed_across_downloads() {
        let server = MockServer::start().await;
        server.mount_route(
            "/fixture",
            Behavior::default().with_etag(Validators::Changing),
        );
        let dir = ScratchDir::new("changing");
        let progress = Progress::shared();

        let first = download_sequential(&client(), &server.url("/fixture"), dir.path(), &progress)
            .await
            .expect("first download completes");
        let second = download_sequential(&client(), &server.url("/fixture"), dir.path(), &progress)
            .await
            .expect("second download completes");

        let first_etag = first.etag.expect("changing validators are always supplied");
        let second_etag = second
            .etag
            .expect("changing validators are always supplied");
        assert_ne!(
            first_etag, second_etag,
            "each download records the representation it actually received"
        );
        // Different objects may not consume each other's destination.
        assert_ne!(
            first.destination, second.destination,
            "the collision policy keeps both representations distinct"
        );
    }

    #[tokio::test]
    async fn the_effective_url_names_the_file_after_redirects() {
        let server = MockServer::start().await;
        server.mount_route(
            "/get-now",
            Behavior::default().with_redirect(301, server.url("/downloads/real-package.zip")),
        );
        server.mount_route("/downloads/real-package.zip", Behavior::default());
        let dir = ScratchDir::new("redirect-name");
        let progress = Progress::shared();

        let outcome =
            download_sequential(&client(), &server.url("/get-now"), dir.path(), &progress)
                .await
                .expect("the redirect resolves and the file downloads");

        assert_eq!(outcome.file_name, "real-package.zip");
        assert_eq!(outcome.destination, dir.path().join("real-package.zip"));
        assert_eq!(
            outcome.effective_url,
            server.url("/downloads/real-package.zip")
        );
        assert_eq!(dir.names(), ["real-package.zip"]);
    }
}
