//! Rope-backed text document with LSP-compatible position math.
//!
//! ## Why a rope?
//!
//! Editors send incremental changes (`textDocument/didChange` with a `range`)
//! on every keystroke. Applying those to a flat `String` is O(n) — every
//! keystroke copies the entire file. A rope (`ropey::Rope`) makes it
//! O(log n + edit_size), which keeps us responsive on large files.
//!
//! ## Coordinates
//!
//! The LSP wire format uses [`Position`]s expressed as `(line, character)`
//! where `character` is a **UTF-16 code unit offset within the line**. We
//! store text as UTF-8 internally (it's what Rust strings are) and convert
//! at the boundary. `ropey` gives us cheap line indexing and helpfully also
//! tracks UTF-16 lengths per chunk, so the conversion is fast.

use std::ops::Range as StdRange;

use ropey::Rope;
use thiserror::Error;
use tracing::{debug, trace};

// --------------------------------------------------------------------------
// Errors
// --------------------------------------------------------------------------

/// Anything that can go wrong applying an edit or converting a position.
///
/// We use `thiserror` so callers can match on individual variants and so the
/// error renders nicely in `tracing` logs and LSP error responses.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum TextDocumentError {
    /// The supplied [`Position`] referred to a line beyond the end of the
    /// document. Returned by [`Position::to_byte_offset`] and friends.
    #[error("line {line} is out of bounds (document has {total_lines} lines)")]
    LineOutOfBounds {
        /// The line that was requested (0-indexed, as in LSP).
        line: u32,
        /// How many lines the document actually has.
        total_lines: usize,
    },

    /// The character offset is past the end of its line. We accept "one past
    /// the end" (cursor at end-of-line) and reject anything beyond that.
    #[error("character {character} is out of bounds on line {line} (length {line_length_utf16})")]
    CharacterOutOfBounds {
        /// The line being indexed into.
        line: u32,
        /// The requested character (UTF-16) offset.
        character: u32,
        /// Length of that line in UTF-16 code units, excluding the trailing newline.
        line_length_utf16: u32,
    },

    /// An incremental edit's `range` had `end < start`, which makes no sense.
    #[error("invalid range: end ({end_line}:{end_character}) precedes start ({start_line}:{start_character})")]
    InvertedRange {
        /// Start position line.
        start_line: u32,
        /// Start position character.
        start_character: u32,
        /// End position line.
        end_line: u32,
        /// End position character.
        end_character: u32,
    },
}

// --------------------------------------------------------------------------
// Position / Range
// --------------------------------------------------------------------------

/// A zero-based `(line, character)` coordinate in a text document.
///
/// `character` is in **UTF-16 code units**, matching the LSP wire format.
/// This is the same convention `lsp_types::Position` uses, but we keep our
/// own type to avoid leaking that dependency into every crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Position {
    /// Zero-based line number.
    pub line: u32,
    /// Zero-based UTF-16 character offset within the line.
    pub character: u32,
}

impl Position {
    /// Construct a position. Just a convenience.
    #[must_use]
    pub const fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }

    /// Convert this LSP `(line, utf16-char)` position into a byte offset
    /// inside `rope`. Returns an error if the line/character are out of
    /// bounds.
    ///
    /// We accept `character == line_length_utf16` (cursor at end of line).
    pub fn to_byte_offset(self, rope: &Rope) -> Result<usize, TextDocumentError> {
        let total_lines = rope.len_lines();
        // `len_lines()` counts the implicit empty line after a trailing newline,
        // so a file "abc\n" has 2 lines. A position on line N is valid as long
        // as N < total_lines.
        if (self.line as usize) >= total_lines {
            return Err(TextDocumentError::LineOutOfBounds {
                line: self.line,
                total_lines,
            });
        }

        let line_slice = rope.line(self.line as usize);

        // Walk UTF-16 code units inside the line until we've consumed
        // `self.character` of them, then return the byte offset of that point.
        // We must not count the trailing newline, hence the `chars()` over the
        // line slice (ropey's line slices include the newline if present).
        let mut utf16_consumed: u32 = 0;
        let mut byte_in_line: usize = 0;
        for ch in line_slice.chars() {
            // We're done counting at the newline — character offsets never
            // include it.
            if ch == '\n' {
                break;
            }
            if utf16_consumed == self.character {
                break;
            }
            utf16_consumed += ch.len_utf16() as u32;
            byte_in_line += ch.len_utf8();
            if utf16_consumed > self.character {
                // The character offset landed in the middle of a surrogate
                // pair (e.g. an emoji). LSP forbids this; we reject it the
                // same way as "out of bounds".
                return Err(TextDocumentError::CharacterOutOfBounds {
                    line: self.line,
                    character: self.character,
                    line_length_utf16: utf16_len_excl_newline(&line_slice),
                });
            }
        }

        if utf16_consumed < self.character {
            return Err(TextDocumentError::CharacterOutOfBounds {
                line: self.line,
                character: self.character,
                line_length_utf16: utf16_len_excl_newline(&line_slice),
            });
        }

        Ok(rope.line_to_byte(self.line as usize) + byte_in_line)
    }

    /// Convert a byte offset into an LSP `(line, utf16-char)` position.
    ///
    /// Used to translate parser node spans (which are byte ranges) back into
    /// something we can put on an LSP `Diagnostic`.
    #[must_use]
    pub fn from_byte_offset(rope: &Rope, byte: usize) -> Self {
        let byte = byte.min(rope.len_bytes());
        let line = rope.byte_to_line(byte);
        let line_start = rope.line_to_byte(line);
        let line_slice = rope.line(line);

        let mut utf16: u32 = 0;
        let mut bytes_seen: usize = 0;
        for ch in line_slice.chars() {
            if bytes_seen + ch.len_utf8() > byte - line_start {
                break;
            }
            utf16 += ch.len_utf16() as u32;
            bytes_seen += ch.len_utf8();
        }

        Self {
            line: line as u32,
            character: utf16,
        }
    }
}

/// Compute the UTF-16 length of a line slice, *not counting* a trailing `\n`.
fn utf16_len_excl_newline(slice: &ropey::RopeSlice<'_>) -> u32 {
    let mut len = 0u32;
    for ch in slice.chars() {
        if ch == '\n' {
            break;
        }
        len += ch.len_utf16() as u32;
    }
    len
}

/// Half-open range `[start, end)` in LSP coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Range {
    /// Inclusive start.
    pub start: Position,
    /// Exclusive end.
    pub end: Position,
}

impl Range {
    /// Construct a range. Convenience.
    #[must_use]
    pub const fn new(start: Position, end: Position) -> Self {
        Self { start, end }
    }

    /// Convert this LSP range into a byte range in the rope.
    pub fn to_byte_range(self, rope: &Rope) -> Result<StdRange<usize>, TextDocumentError> {
        // Sanity check before doing any conversion — saves a confusing error
        // from one of the endpoint conversions.
        if self.end < self.start {
            return Err(TextDocumentError::InvertedRange {
                start_line: self.start.line,
                start_character: self.start.character,
                end_line: self.end.line,
                end_character: self.end.character,
            });
        }
        let start = self.start.to_byte_offset(rope)?;
        let end = self.end.to_byte_offset(rope)?;
        Ok(start..end)
    }
}

// --------------------------------------------------------------------------
// TextDocument
// --------------------------------------------------------------------------

/// A versioned, in-memory text document.
///
/// "Versioned" means we track the LSP `version` number sent in `didOpen` and
/// bumped on every `didChange`. Diagnostics we publish are tagged with this
/// version so the editor can drop stale results.
///
/// The document is stored as a [`Rope`] for fast incremental edits. We never
/// hand the rope out directly — callers go through [`Self::text`] (full
/// string snapshot) or [`Self::rope`] (read-only borrow) so we can switch
/// representations later if we ever want to.
#[derive(Debug, Clone)]
pub struct TextDocument {
    rope: Rope,
    version: i32,
}

impl TextDocument {
    /// Build a new document from full text and its initial version.
    #[must_use]
    pub fn new(text: &str, version: i32) -> Self {
        Self {
            rope: Rope::from_str(text),
            version,
        }
    }

    /// Current document version (LSP semantics: monotonically increasing).
    #[must_use]
    pub fn version(&self) -> i32 {
        self.version
    }

    /// Read-only access to the underlying rope. Useful when callers (the
    /// parser) want to feed the rope's chunks into something streaming.
    #[must_use]
    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    /// Snapshot the document as a single `String`. This *does* allocate; for
    /// the parser we currently just pay that cost, but tree-sitter has a
    /// chunk-callback API we can switch to later if it becomes a hot spot.
    #[must_use]
    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    /// Number of bytes in the document.
    #[must_use]
    pub fn len_bytes(&self) -> usize {
        self.rope.len_bytes()
    }

    /// Replace the entire document content. This is what we do on a
    /// "full sync" `didChange` (no `range` field).
    pub fn replace_all(&mut self, text: &str, new_version: i32) {
        debug!(
            old_bytes = self.rope.len_bytes(),
            new_bytes = text.len(),
            new_version,
            "TextDocument: full replace",
        );
        self.rope = Rope::from_str(text);
        self.version = new_version;
    }

    /// Apply a single incremental edit.
    ///
    /// `range` is in LSP coordinates (UTF-16, line+char). `new_text` replaces
    /// the contents of `range`. After the edit we bump `self.version` to
    /// `new_version`.
    pub fn apply_incremental_edit(
        &mut self,
        range: Range,
        new_text: &str,
        new_version: i32,
    ) -> Result<(), TextDocumentError> {
        let byte_range = range.to_byte_range(&self.rope)?;
        trace!(
            byte_start = byte_range.start,
            byte_end   = byte_range.end,
            new_text_len = new_text.len(),
            "TextDocument: incremental edit",
        );

        // Convert byte offsets → char offsets, since `Rope::remove` /
        // `Rope::insert` operate on char indices. (Yes, this is a wart in
        // ropey's API; the conversion is O(log n) so it's fine.)
        let start_char = self.rope.byte_to_char(byte_range.start);
        let end_char = self.rope.byte_to_char(byte_range.end);
        if start_char != end_char {
            self.rope.remove(start_char..end_char);
        }
        if !new_text.is_empty() {
            self.rope.insert(start_char, new_text);
        }
        self.version = new_version;
        Ok(())
    }
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// `position.to_byte_offset` should round-trip with `from_byte_offset`
    /// for plain ASCII.
    #[test]
    fn position_round_trip_ascii() {
        let rope = Rope::from_str("hello\nworld\n");
        for (line, ch, expected_byte) in [(0, 0, 0), (0, 5, 5), (1, 0, 6), (1, 5, 11)] {
            let pos = Position::new(line, ch);
            let byte = pos.to_byte_offset(&rope).unwrap();
            assert_eq!(byte, expected_byte, "to_byte_offset {pos:?}");
            assert_eq!(Position::from_byte_offset(&rope, byte), pos, "round-trip");
        }
    }

    /// UTF-8 multibyte characters take 1 UTF-16 code unit but >1 UTF-8 byte.
    /// Verify we count UTF-16 on the way in and bytes on the way out.
    #[test]
    fn position_handles_multibyte_utf8() {
        // "héllo": 'é' is U+00E9, 2 bytes in UTF-8, 1 code unit in UTF-16.
        let rope = Rope::from_str("héllo");
        // Char index 2 (after "hé") should be byte offset 3.
        let pos = Position::new(0, 2);
        assert_eq!(pos.to_byte_offset(&rope).unwrap(), 3);
    }

    /// Surrogate pairs (e.g. emoji) take 2 UTF-16 code units but 1 codepoint.
    /// LSP positions in the middle of a surrogate pair are illegal — we
    /// reject them.
    #[test]
    fn position_rejects_split_surrogate() {
        // U+1F600 grinning face: 4 bytes UTF-8, 2 code units UTF-16.
        let rope = Rope::from_str("a😀b");
        // Character 2 lands inside the surrogate pair → error.
        let pos = Position::new(0, 2);
        assert!(matches!(
            pos.to_byte_offset(&rope),
            Err(TextDocumentError::CharacterOutOfBounds { .. })
        ));
        // Character 3 lands cleanly after the emoji.
        let after = Position::new(0, 3);
        assert_eq!(after.to_byte_offset(&rope).unwrap(), 5); // 1 + 4
    }

    /// Out-of-bounds line and character produce the right error variant.
    #[test]
    fn position_out_of_bounds() {
        let rope = Rope::from_str("hi\n");
        assert!(matches!(
            Position::new(99, 0).to_byte_offset(&rope),
            Err(TextDocumentError::LineOutOfBounds { .. })
        ));
        assert!(matches!(
            Position::new(0, 99).to_byte_offset(&rope),
            Err(TextDocumentError::CharacterOutOfBounds { .. })
        ));
    }

    /// Replacing the entire document bumps the version and updates the text.
    #[test]
    fn full_replace() {
        let mut doc = TextDocument::new("module foo;\nendmodule\n", 1);
        assert_eq!(doc.version(), 1);
        doc.replace_all("module bar;\nendmodule\n", 2);
        assert_eq!(doc.version(), 2);
        assert!(doc.text().contains("bar"));
    }

    /// An incremental edit that inserts text in the middle of a line.
    #[test]
    fn incremental_insert() {
        let mut doc = TextDocument::new("module foo;\n", 1);
        // Insert " bar" right before the semicolon (column 10).
        let range = Range::new(Position::new(0, 10), Position::new(0, 10));
        doc.apply_incremental_edit(range, "_bar", 2).unwrap();
        assert_eq!(doc.text(), "module foo_bar;\n");
        assert_eq!(doc.version(), 2);
    }

    /// An incremental edit that deletes a span.
    #[test]
    fn incremental_delete() {
        let mut doc = TextDocument::new("module foo;\n", 1);
        // Delete "foo" (cols 7..10 on line 0).
        let range = Range::new(Position::new(0, 7), Position::new(0, 10));
        doc.apply_incremental_edit(range, "", 2).unwrap();
        assert_eq!(doc.text(), "module ;\n");
    }

    /// An incremental edit that spans a newline boundary.
    #[test]
    fn incremental_replace_across_lines() {
        let mut doc = TextDocument::new("a\nb\nc\n", 1);
        // Replace from line 0 col 1 to line 1 col 1 with "XYZ".
        let range = Range::new(Position::new(0, 1), Position::new(1, 1));
        doc.apply_incremental_edit(range, "XYZ", 2).unwrap();
        assert_eq!(doc.text(), "aXYZ\nc\n");
    }

    /// Inverted range is rejected.
    #[test]
    fn inverted_range_rejected() {
        let rope = Rope::from_str("abc\n");
        let bad = Range::new(Position::new(0, 2), Position::new(0, 1));
        assert!(matches!(
            bad.to_byte_range(&rope),
            Err(TextDocumentError::InvertedRange { .. })
        ));
    }
}
