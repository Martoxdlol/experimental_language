//! Source locations: byte offsets, spans, and a multi-file source map.
//!
//! The lexer records every token as a `Span` (a byte range inside one source
//! file). Higher-level information like line/column is derived on demand from
//! the owning `SourceFile`, so tokens stay small and cheap to copy.

use std::fmt;
use std::ops::Range;

/// A byte offset into a single source file.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Default)]
pub struct BytePos(pub u32);

impl BytePos {
    pub const ZERO: BytePos = BytePos(0);

    #[inline]
    pub fn to_usize(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Debug for BytePos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Identifies a source file inside a `SourceMap`.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct FileId(pub u32);

impl fmt::Debug for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "f{}", self.0)
    }
}

/// Half-open byte range `[lo, hi)` inside a specific file.
#[derive(Copy, Clone, Eq, PartialEq, Hash)]
pub struct Span {
    pub file: FileId,
    pub lo: BytePos,
    pub hi: BytePos,
}

impl Span {
    #[inline]
    pub fn new(file: FileId, lo: BytePos, hi: BytePos) -> Self {
        debug_assert!(lo.0 <= hi.0, "Span lo must be <= hi");
        Self { file, lo, hi }
    }

    /// A zero-length span at `pos` — useful for end-of-file or insertion points.
    #[inline]
    pub fn empty(file: FileId, pos: BytePos) -> Self {
        Self { file, lo: pos, hi: pos }
    }

    #[inline]
    pub fn len(self) -> u32 {
        self.hi.0 - self.lo.0
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        self.lo == self.hi
    }

    #[inline]
    pub fn range(self) -> Range<usize> {
        self.lo.to_usize()..self.hi.to_usize()
    }

    /// Smallest span containing both. Both spans must come from the same file.
    pub fn join(self, other: Span) -> Span {
        debug_assert_eq!(self.file, other.file, "cannot join spans across files");
        Span {
            file: self.file,
            lo: BytePos(self.lo.0.min(other.lo.0)),
            hi: BytePos(self.hi.0.max(other.hi.0)),
        }
    }
}

impl fmt::Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}:{}..{}", self.file, self.lo.0, self.hi.0)
    }
}

/// 1-based human-readable position.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub struct LineCol {
    pub line: u32,
    pub col: u32,
}

/// A single source file's text plus a precomputed table of line starts.
pub struct SourceFile {
    pub id: FileId,
    pub name: String,
    pub src: String,
    /// Byte offset of the start of each line. Always begins with 0.
    line_starts: Vec<BytePos>,
}

impl SourceFile {
    fn new(id: FileId, name: String, src: String) -> Self {
        let line_starts = compute_line_starts(&src);
        Self { id, name, src, line_starts }
    }

    /// The text covered by `span`. Panics if `span` belongs to another file.
    pub fn slice(&self, span: Span) -> &str {
        assert_eq!(span.file, self.id, "Span/SourceFile mismatch");
        &self.src[span.range()]
    }

    /// 1-based (line, column). Column counts UTF-8 bytes from the line start.
    pub fn line_col(&self, pos: BytePos) -> LineCol {
        let off = pos.to_usize().min(self.src.len());
        let line_idx = match self.line_starts.binary_search(&BytePos(off as u32)) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let line_start = self.line_starts[line_idx].to_usize();
        LineCol {
            line: (line_idx as u32) + 1,
            col: (off - line_start) as u32 + 1,
        }
    }
}

fn compute_line_starts(src: &str) -> Vec<BytePos> {
    let mut starts = Vec::with_capacity(src.len() / 32 + 1);
    starts.push(BytePos(0));
    for (i, b) in src.bytes().enumerate() {
        if b == b'\n' {
            starts.push(BytePos(i as u32 + 1));
        }
    }
    starts
}

/// Owns the source text of every file in the current compilation.
#[derive(Default)]
pub struct SourceMap {
    files: Vec<SourceFile>,
}

impl SourceMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_file(&mut self, name: impl Into<String>, src: impl Into<String>) -> FileId {
        let id = FileId(self.files.len() as u32);
        self.files.push(SourceFile::new(id, name.into(), src.into()));
        id
    }

    pub fn file(&self, id: FileId) -> &SourceFile {
        &self.files[id.0 as usize]
    }

    pub fn slice(&self, span: Span) -> &str {
        self.file(span.file).slice(span)
    }
}
