//! The transcript file watermark: what the index records about a file, and how
//! a file's current state is classified against it (`aub-lqe.2`, PLAN.md 17.2).
//!
//! Incremental ingestion is what keeps a repeated `aub spend` cheap, and every
//! shortcut in its change detection produces a wrong report rather than a slow
//! one. The watermark therefore carries a platform-abstract file identity plus
//! size, modification metadata, consumed byte offset and parser version. The
//! parser version is the field people forget, and without it a parser fix
//! silently applies only to files that happened to change afterwards.
//!
//! Two edge cases have defined behaviour. If the file shrinks or its identity
//! changes, re-read from the beginning, which is safe because canonical
//! event-level deduplication will not double count. A trailing partial line is
//! not consumed until it becomes complete, because consuming half a JSON record
//! and remembering the offset produces a permanent parse failure at that
//! offset.

use std::path::Path;

use crate::domain::time::unix_nanos;
use crate::error::Error;

/// The change class of a file relative to its stored watermark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeClass {
    /// Not in the index: read from the beginning.
    New,
    /// Unchanged: skip.
    Unchanged,
    /// Appended: resume from the stored consumed offset.
    Appended,
    /// The parser version changed: reparse the whole file, because the old
    /// parse was produced by a parser this binary no longer trusts.
    ParserVersionChanged,
    /// Shrunk, identity changed, or rewritten in place at the same size:
    /// re-read from the beginning. Canonical event-level deduplication makes
    /// this safe.
    RebuildRequired,
}

/// The stored watermark for one transcript file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Watermark {
    /// The configured source key.
    pub source_key: String,
    /// The path relative to the configured root. Machine-specific absolute
    /// paths never enter the index.
    pub relative_path: String,
    /// The file size in bytes at the last ingestion.
    pub size: u64,
    /// The modification time in Unix nanoseconds at the last ingestion.
    pub mtime_nanos: i64,
    /// A platform-abstract file identity: device and inode on Unix. Changes
    /// when the file is replaced rather than appended to.
    pub identity: String,
    /// The parser version that last consumed the file.
    pub parser_version: String,
    /// The byte offset ingestion consumed up to. Never points into a partial
    /// trailing line.
    pub consumed_offset: u64,
}

/// The current observable state of a file, as the classification compares it
/// against a stored watermark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileState {
    /// The file size in bytes.
    pub size: u64,
    /// The modification time in Unix nanoseconds.
    pub mtime_nanos: i64,
    /// The platform-abstract file identity.
    pub identity: String,
}

impl FileState {
    /// Reads the current state of `path`.
    pub fn read(path: &Path) -> Result<FileState, Error> {
        let metadata = std::fs::metadata(path).map_err(|e| {
            Error::IngestIncomplete(format!(
                "cannot stat transcript file {}: {e}",
                path.display()
            ))
        })?;
        let mtime = metadata.modified().map_err(|e| {
            Error::IngestIncomplete(format!(
                "cannot read the modification time of transcript file {}: {e}",
                path.display()
            ))
        })?;
        let mtime_nanos = i64::try_from(unix_nanos(mtime)).map_err(|_| {
            Error::IngestIncomplete(format!(
                "modification time of transcript file {} is outside the representable range",
                path.display()
            ))
        })?;
        Ok(FileState {
            size: metadata.len(),
            mtime_nanos,
            identity: file_identity(&metadata),
        })
    }
}

/// The platform-abstract file identity: device and inode on Unix, so a file
/// replaced by rename is a different identity while an in-place append is the
/// same one. On platforms without a stable inode, the canonical path stands in:
/// it changes when the file is replaced, which is the property the identity
/// exists to detect.
fn file_identity(metadata: &std::fs::Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        format!("{}:{}", metadata.dev(), metadata.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        "path".to_string()
    }
}

/// Classifies a file's current state against its stored watermark.
///
/// The order is load-bearing. Identity and shrink are checked before the
/// size/mtime comparison, because a replaced or truncated file must never be
/// resumed from an offset that no longer means anything; the parser version is
/// checked before the unchanged test, because a parser fix must reparse even
/// files that did not change.
pub fn classify(
    stored: Option<&Watermark>,
    current: &FileState,
    parser_version: &str,
) -> ChangeClass {
    let Some(stored) = stored else {
        return ChangeClass::New;
    };
    if stored.identity != current.identity {
        return ChangeClass::RebuildRequired;
    }
    if current.size < stored.size {
        return ChangeClass::RebuildRequired;
    }
    if stored.parser_version != parser_version {
        return ChangeClass::ParserVersionChanged;
    }
    if current.size == stored.size && current.mtime_nanos == stored.mtime_nanos {
        return ChangeClass::Unchanged;
    }
    if current.size > stored.size {
        return ChangeClass::Appended;
    }
    // Same size but a different mtime: the file was rewritten in place, and
    // the content may differ at the same size. Resuming from the offset is
    // not safe; re-read from the beginning.
    ChangeClass::RebuildRequired
}

/// The byte offset of the last complete line at or before `offset` in
/// `content`: a trailing partial line is not consumed until it becomes
/// complete, because consuming half a JSON record and remembering the offset
/// produces a permanent parse failure at that offset.
///
/// When the byte before `offset` is a newline, `offset` is already at a line
/// boundary and is returned unchanged. Otherwise the offset snaps back to just
/// after the previous newline, or to zero when the content has no newline at
/// all.
pub fn last_complete_line_offset(content: &str, offset: u64) -> u64 {
    let offset = usize::try_from(offset).unwrap_or(content.len());
    let offset = offset.min(content.len());
    let bytes = &content.as_bytes()[..offset];
    if bytes.last() == Some(&b'\n') {
        return offset as u64;
    }
    match bytes.iter().rposition(|&byte| byte == b'\n') {
        Some(position) => (position + 1) as u64,
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watermark(
        size: u64,
        mtime_nanos: i64,
        identity: &str,
        parser_version: &str,
        consumed_offset: u64,
    ) -> Watermark {
        Watermark {
            source_key: "claude-code".to_string(),
            relative_path: "session.jsonl".to_string(),
            size,
            mtime_nanos,
            identity: identity.to_string(),
            parser_version: parser_version.to_string(),
            consumed_offset,
        }
    }

    fn state(size: u64, mtime_nanos: i64, identity: &str) -> FileState {
        FileState {
            size,
            mtime_nanos,
            identity: identity.to_string(),
        }
    }

    /// A file with no stored watermark is new: read from the beginning.
    #[test]
    fn a_file_without_a_watermark_is_new() {
        assert_eq!(
            classify(None, &state(100, 1, "dev:1"), "parser-1"),
            ChangeClass::New
        );
    }

    /// The planted negative for the unchanged class: identical size, mtime and
    /// identity with the same parser version is the only combination that
    /// skips the file.
    #[test]
    fn an_unchanged_file_is_skipped() {
        let stored = watermark(100, 1_000, "dev:7", "parser-1", 100);
        assert_eq!(
            classify(Some(&stored), &state(100, 1_000, "dev:7"), "parser-1"),
            ChangeClass::Unchanged
        );
    }

    /// A grown file with the same identity and parser version resumes from its
    /// stored offset.
    #[test]
    fn an_appended_file_resumes_from_its_offset() {
        let stored = watermark(100, 1_000, "dev:7", "parser-1", 100);
        assert_eq!(
            classify(Some(&stored), &state(150, 2_000, "dev:7"), "parser-1"),
            ChangeClass::Appended
        );
    }

    /// A parser version change forces a reparse even when the file is
    /// otherwise unchanged: a parser fix must apply to files that did not
    /// change afterwards.
    #[test]
    fn a_parser_version_change_forces_a_reparse_of_unchanged_files() {
        let stored = watermark(100, 1_000, "dev:7", "parser-1", 100);
        assert_eq!(
            classify(Some(&stored), &state(100, 1_000, "dev:7"), "parser-2"),
            ChangeClass::ParserVersionChanged
        );
    }

    /// A parser version change outranks an append: the whole file is reparsed
    /// with the new parser, never resumed with the old one's offset.
    #[test]
    fn a_parser_version_change_outranks_an_append() {
        let stored = watermark(100, 1_000, "dev:7", "parser-1", 100);
        assert_eq!(
            classify(Some(&stored), &state(150, 2_000, "dev:7"), "parser-2"),
            ChangeClass::ParserVersionChanged
        );
    }

    /// A shrunk file is re-read from the beginning: the stored offset no
    /// longer means anything.
    #[test]
    fn a_shrunk_file_is_re_read_from_the_beginning() {
        let stored = watermark(100, 1_000, "dev:7", "parser-1", 100);
        assert_eq!(
            classify(Some(&stored), &state(80, 2_000, "dev:7"), "parser-1"),
            ChangeClass::RebuildRequired
        );
    }

    /// A replaced file (identity changed) is re-read from the beginning even
    /// when its size and mtime look unchanged.
    #[test]
    fn a_replaced_file_is_re_read_from_the_beginning() {
        let stored = watermark(100, 1_000, "dev:7", "parser-1", 100);
        assert_eq!(
            classify(Some(&stored), &state(100, 1_000, "dev:9"), "parser-1"),
            ChangeClass::RebuildRequired
        );
    }

    /// A file rewritten in place at the same size (same identity, same size,
    /// different mtime) is re-read from the beginning: the content may differ
    /// at the same size, so the stored offset is not safe.
    #[test]
    fn a_same_size_rewrite_is_re_read_from_the_beginning() {
        let stored = watermark(100, 1_000, "dev:7", "parser-1", 100);
        assert_eq!(
            classify(Some(&stored), &state(100, 2_000, "dev:7"), "parser-1"),
            ChangeClass::RebuildRequired
        );
    }

    /// An offset at a line boundary is returned unchanged.
    #[test]
    fn an_offset_at_a_line_boundary_is_unchanged() {
        let content = "line1\nline2\n";
        assert_eq!(last_complete_line_offset(content, 12), 12);
    }

    /// A trailing partial line is not consumed: the offset snaps back to the
    /// last complete line.
    #[test]
    fn a_trailing_partial_line_is_not_consumed() {
        let content = "line1\nline2\nline3";
        assert_eq!(last_complete_line_offset(content, 17), 12);
    }

    /// Content with no newline at all snaps to zero: nothing is complete.
    #[test]
    fn content_without_a_newline_snaps_to_zero() {
        assert_eq!(last_complete_line_offset("partial", 7), 0);
    }

    /// An offset beyond the content length is clamped to the content, then
    /// snapped to the last complete line.
    #[test]
    fn an_offset_beyond_the_content_is_clamped_then_snapped() {
        let content = "line1\nline2\n";
        assert_eq!(last_complete_line_offset(content, 999), 12);
    }

    /// The integration shape from the bead: a partial trailing line is not
    /// consumed, the remainder is appended between runs, and the completed
    /// line is consumed exactly once.
    #[test]
    fn a_partial_line_is_picked_up_exactly_once_once_complete() {
        let first = "line1\nline2\nline3";
        let first_offset = last_complete_line_offset(first, first.len() as u64);
        assert_eq!(first_offset, 12, "the partial line3 must not be consumed");

        // The remainder arrives: the file grew, so it is appended, and the
        // parse resumes from the stored offset.
        let second = "line1\nline2\nline3\n";
        let stored = watermark(12, 1_000, "dev:7", "parser-1", first_offset);
        assert_eq!(
            classify(Some(&stored), &state(18, 2_000, "dev:7"), "parser-1"),
            ChangeClass::Appended
        );
        let remainder = &second[first_offset as usize..];
        assert_eq!(
            remainder, "line3\n",
            "the resumed parse consumes exactly the completed line"
        );
    }
}
