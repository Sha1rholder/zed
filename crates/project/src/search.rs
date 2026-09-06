use aho_corasick::{AhoCorasick, AhoCorasickBuilder};
use anyhow::{Ok, Result};
use client::proto;
use fancy_regex::{Captures, Regex, RegexBuilder};
use gpui::Entity;
use itertools::Itertools as _;
use language::{Buffer, BufferSnapshot, CharClassifier, CharKind, WhitespaceDelimited};
use smol::future::yield_now;
use std::{
    borrow::Cow,
    collections::BTreeSet,
    io::{BufRead, BufReader, Read},
    ops::Range,
    sync::{Arc, LazyLock},
};
use text::Anchor;
use util::{
    paths::{PathMatcher, PathStyle},
    rel_path::RelPath,
};

#[derive(Debug)]
pub enum SearchResult {
    Buffer {
        buffer: Entity<Buffer>,
        ranges: Vec<Range<Anchor>>,
    },
    LimitReached,
    WaitingForScan,
    Searching,
}

#[derive(Clone, Copy, PartialEq)]
pub enum SearchInputKind {
    Query,
    Include,
    Exclude,
}

#[derive(Clone, Debug)]
pub struct SearchInputs {
    query: Arc<str>,
    files_to_include: PathMatcher,
    files_to_exclude: PathMatcher,
    match_full_paths: bool,
    buffers: Option<Vec<Entity<Buffer>>>,
}

#[derive(Clone, Copy, Debug)]
pub enum MatchPositionHint {
    Line(u32),
    ByteOffset(usize),
}

impl Default for MatchPositionHint {
    fn default() -> Self {
        Self::Line(0)
    }
}

impl SearchInputs {
    pub fn as_str(&self) -> &str {
        self.query.as_ref()
    }
    pub fn files_to_include(&self) -> &PathMatcher {
        &self.files_to_include
    }
    pub fn files_to_exclude(&self) -> &PathMatcher {
        &self.files_to_exclude
    }
    pub fn buffers(&self) -> &Option<Vec<Entity<Buffer>>> {
        &self.buffers
    }
}
#[derive(Clone, Debug)]
pub enum SearchQuery {
    Text {
        search: AhoCorasick,
        replacement: Option<String>,
        whole_word: bool,
        case_sensitive: bool,
        include_ignored: bool,
        inner: SearchInputs,
    },
    Regex {
        regex: Regex,
        replacement: Option<String>,
        whole_word: bool,
        case_sensitive: bool,
        include_ignored: bool,
        one_match_per_line: bool,
        inner: SearchInputs,
        escaped: bool,
    },
}

static WORD_MATCH_TEST: LazyLock<Regex> = LazyLock::new(|| {
    RegexBuilder::new(r"\B")
        .build()
        .expect("Failed to create WORD_MATCH_TEST")
});

// Extend only the boundaries inserted by the whole-word option. Explicit `\b`
// assertions in the user's regex retain their regex-engine meaning.
static WHOLE_WORD_BOUNDARY: LazyLock<String> = LazyLock::new(|| {
    let scripts = WhitespaceDelimited::NO_SCRIPTS
        .iter()
        .map(|script| format!(r"\p{{sc={}}}", script.full_name()))
        .collect::<String>();
    let extensions = WhitespaceDelimited::NO_SCRIPTS
        .iter()
        .map(|script| format!(r"\p{{scx={}}}", script.full_name()))
        .collect::<String>();
    // Match CharClassifier's alphanumeric test and treatment of shared CJK
    // characters, including kana prolonged sound marks.
    let no = format!(
        r"[[\p{{Alphabetic}}\p{{Number}}]&&[{scripts}[[\p{{Common}}\p{{Inherited}}]&&[{extensions}]]]]"
    );
    let yes = format!(r"[[\p{{Alphabetic}}\p{{Number}}_]--{no}]");
    format!(r"(?:\b|(?<={no})(?={yes})|(?<={yes})(?={no}))")
});

fn is_whole_word_match(
    prev: Option<CharKind>,
    start: Option<CharKind>,
    end: Option<CharKind>,
    next: Option<CharKind>,
) -> bool {
    !(matches!(start, Some(CharKind::Word(_))) && start == prev
        || matches!(end, Some(CharKind::Word(_))) && end == next)
}

impl SearchQuery {
    /// Create a text query
    ///
    /// If `match_full_paths` is true, include/exclude patterns will always be matched against fully qualified project paths beginning with a project root.
    /// If `match_full_paths` is false, patterns will be matched against worktree-relative paths.
    pub fn text(
        query: impl ToString,
        whole_word: bool,
        case_sensitive: bool,
        include_ignored: bool,
        files_to_include: PathMatcher,
        files_to_exclude: PathMatcher,
        match_full_paths: bool,
        buffers: Option<Vec<Entity<Buffer>>>,
    ) -> Result<Self> {
        let mut query = query.to_string();
        text::LineEnding::normalize(&mut query);
        if !case_sensitive && !query.is_ascii() {
            // AhoCorasickBuilder doesn't support case-insensitive search with unicode characters
            // Fallback to regex search as recommended by
            // https://docs.rs/aho-corasick/1.1/aho_corasick/struct.AhoCorasickBuilder.html#method.ascii_case_insensitive
            return Self::escaped_regex(
                query,
                whole_word,
                case_sensitive,
                include_ignored,
                files_to_include,
                files_to_exclude,
                match_full_paths,
                buffers,
            );
        }
        let search = AhoCorasickBuilder::new()
            .ascii_case_insensitive(!case_sensitive)
            .build([&query])?;
        let inner = SearchInputs {
            query: query.into(),
            files_to_exclude,
            files_to_include,
            match_full_paths,
            buffers,
        };
        Ok(Self::Text {
            search,
            replacement: None,
            whole_word,
            case_sensitive,
            include_ignored,
            inner,
        })
    }

    /// Create a regex query
    ///
    /// If `match_full_paths` is true, include/exclude patterns will be matched against fully qualified project paths
    /// beginning with a project root name. If false, they will be matched against project-relative paths (which don't start
    /// with their respective project root).
    pub fn regex(
        query: impl ToString,
        whole_word: bool,
        case_sensitive: bool,
        include_ignored: bool,
        one_match_per_line: bool,
        files_to_include: PathMatcher,
        files_to_exclude: PathMatcher,
        match_full_paths: bool,
        buffers: Option<Vec<Entity<Buffer>>>,
    ) -> Result<Self> {
        let query = query.to_string();
        let inner = SearchInputs {
            query: Arc::from(query.as_str()),
            files_to_include,
            files_to_exclude,
            match_full_paths,
            buffers,
        };
        Self::build_regex(
            query,
            whole_word,
            case_sensitive,
            include_ignored,
            one_match_per_line,
            inner,
            false,
        )
    }

    /// Create a regex query from a literal string, escaping any regex
    /// metacharacters so that the resulting query matches the literal text.
    ///
    /// Unlike `regex`, the query stored on the resulting `SearchQuery` is the
    /// original unescaped text, so `as_str` returns what the user typed.
    pub fn escaped_regex(
        query: impl ToString,
        whole_word: bool,
        case_sensitive: bool,
        include_ignored: bool,
        files_to_include: PathMatcher,
        files_to_exclude: PathMatcher,
        match_full_paths: bool,
        buffers: Option<Vec<Entity<Buffer>>>,
    ) -> Result<Self> {
        let mut query = query.to_string();
        text::LineEnding::normalize(&mut query);
        let inner = SearchInputs {
            query: Arc::from(query.as_str()),
            files_to_include,
            files_to_exclude,
            match_full_paths,
            buffers,
        };
        Self::build_regex(
            regex::escape(&query),
            whole_word,
            case_sensitive,
            include_ignored,
            false,
            inner,
            true,
        )
    }

    fn build_regex(
        mut pattern: String,
        whole_word: bool,
        mut case_sensitive: bool,
        include_ignored: bool,
        one_match_per_line: bool,
        inner: SearchInputs,
        escaped: bool,
    ) -> Result<Self> {
        if let Some((case_sensitive_from_pattern, new_pattern)) =
            Self::case_sensitive_from_pattern(&pattern)
        {
            case_sensitive = case_sensitive_from_pattern;
            pattern = new_pattern
        }

        // Literal queries using regex for Unicode case folding are filtered in
        // the same way as text queries, with the buffer's character classifier.
        if whole_word && !escaped {
            let mut word_pattern = String::new();
            if let Some(first) = pattern.get(0..1)
                && WORD_MATCH_TEST.is_match(first).is_ok_and(|x| !x)
            {
                word_pattern.push_str(&WHOLE_WORD_BOUNDARY);
            }
            word_pattern.push_str(&pattern);
            if let Some(last) = pattern.get(pattern.len() - 1..)
                && WORD_MATCH_TEST.is_match(last).is_ok_and(|x| !x)
            {
                word_pattern.push_str(&WHOLE_WORD_BOUNDARY);
            }
            pattern = word_pattern
        }

        let regex = RegexBuilder::new(&pattern)
            .case_insensitive(!case_sensitive)
            .multi_line(true)
            .crlf(true)
            .build()?;
        Ok(Self::Regex {
            regex,
            replacement: None,
            whole_word,
            case_sensitive,
            include_ignored,
            inner,
            one_match_per_line,
            escaped,
        })
    }

    /// Extracts case sensitivity settings from pattern items in the provided
    /// query and returns the same query, with the pattern items removed.
    ///
    /// The following pattern modifiers are supported:
    ///
    /// - `\c` (case_sensitive: false)
    /// - `\C` (case_sensitive: true)
    ///
    /// If no pattern item were found, `None` will be returned.
    fn case_sensitive_from_pattern(query: &str) -> Option<(bool, String)> {
        if !(query.contains("\\c") || query.contains("\\C")) {
            return None;
        }

        let mut was_escaped = false;
        let mut new_query = String::new();
        let mut is_case_sensitive = None;

        for c in query.chars() {
            if was_escaped {
                if c == 'c' {
                    is_case_sensitive = Some(false);
                } else if c == 'C' {
                    is_case_sensitive = Some(true);
                } else {
                    new_query.push('\\');
                    new_query.push(c);
                }
                was_escaped = false
            } else if c == '\\' {
                was_escaped = true
            } else {
                new_query.push(c);
            }
        }

        is_case_sensitive.map(|c| (c, new_query))
    }

    pub fn from_proto(message: proto::SearchQuery, path_style: PathStyle) -> Result<Self> {
        let files_to_include = if message.files_to_include.is_empty() {
            message
                .files_to_include_legacy
                .split(',')
                .map(str::trim)
                .filter(|&glob_str| !glob_str.is_empty())
                .map(|s| s.to_string())
                .collect()
        } else {
            message.files_to_include
        };

        let files_to_exclude = if message.files_to_exclude.is_empty() {
            message
                .files_to_exclude_legacy
                .split(',')
                .map(str::trim)
                .filter(|&glob_str| !glob_str.is_empty())
                .map(|s| s.to_string())
                .collect()
        } else {
            message.files_to_exclude
        };

        if message.regex {
            Self::regex(
                message.query,
                message.whole_word,
                message.case_sensitive,
                message.include_ignored,
                false,
                PathMatcher::new(files_to_include, path_style)?,
                PathMatcher::new(files_to_exclude, path_style)?,
                message.match_full_paths,
                None, // search opened only don't need search remote
            )
        } else {
            Self::text(
                message.query,
                message.whole_word,
                message.case_sensitive,
                message.include_ignored,
                PathMatcher::new(files_to_include, path_style)?,
                PathMatcher::new(files_to_exclude, path_style)?,
                message.match_full_paths,
                None, // search opened only don't need search remote
            )
        }
    }

    pub fn with_replacement(mut self, new_replacement: String) -> Self {
        match self {
            Self::Text {
                ref mut replacement,
                ..
            }
            | Self::Regex {
                ref mut replacement,
                ..
            } => {
                *replacement = Some(new_replacement);
                self
            }
        }
    }

    pub fn to_proto(&self) -> proto::SearchQuery {
        let mut files_to_include = self.files_to_include().sources();
        let mut files_to_exclude = self.files_to_exclude().sources();
        proto::SearchQuery {
            query: self.as_str().to_string(),
            regex: self.is_regex(),
            whole_word: self.whole_word(),
            case_sensitive: self.case_sensitive(),
            include_ignored: self.include_ignored(),
            files_to_include: files_to_include.clone().map(ToOwned::to_owned).collect(),
            files_to_exclude: files_to_exclude.clone().map(ToOwned::to_owned).collect(),
            match_full_paths: self.match_full_paths(),
            // Populate legacy fields for backwards compatibility
            files_to_include_legacy: files_to_include.join(","),
            files_to_exclude_legacy: files_to_exclude.join(","),
        }
    }

    pub async fn detect(
        &self,
        mut reader: BufReader<Box<dyn Read + Send + Sync>>,
    ) -> Result<Option<MatchPositionHint>> {
        let query_str = self.as_str();
        if query_str.is_empty() {
            return Ok(None);
        }

        // Yield from this function every 20KB scanned.
        const YIELD_THRESHOLD: usize = 20 * 1024;

        match self {
            Self::Text { search, .. } => {
                let mut text = String::new();
                if query_str.contains('\n') {
                    reader.read_to_string(&mut text)?;
                    text::LineEnding::normalize(&mut text);
                    if search.is_match(&text) {
                        Ok(Some(MatchPositionHint::default()))
                    } else {
                        Ok(None)
                    }
                } else {
                    let mut bytes_read = 0;
                    let mut line_number = u32::default();
                    while reader.read_line(&mut text)? > 0 {
                        if search.is_match(&text) {
                            return Ok(Some(MatchPositionHint::Line(line_number)));
                        }
                        bytes_read += text.len();
                        if bytes_read >= YIELD_THRESHOLD {
                            bytes_read = 0;
                            smol::future::yield_now().await;
                        }
                        text.clear();
                        line_number += 1;
                    }
                    Ok(None)
                }
            }
            Self::Regex { regex, .. } => {
                let mut text = String::new();

                reader.read_to_string(&mut text)?;
                text::LineEnding::normalize(&mut text);
                if let Some(m) = regex.find(&text)? {
                    Ok(Some(MatchPositionHint::ByteOffset(m.start())))
                } else {
                    Ok(None)
                }
            }
        }
    }
    /// Returns the replacement text for this `SearchQuery`.
    pub fn replacement(&self) -> Option<&str> {
        match self {
            SearchQuery::Text { replacement, .. } | SearchQuery::Regex { replacement, .. } => {
                replacement.as_deref()
            }
        }
    }
    /// Expands `hit` against its line so lookaround assertions retain context.
    pub fn replacement_for<'a>(&self, line: &'a str, hit: Range<usize>) -> Option<Cow<'a, str>> {
        match self {
            SearchQuery::Text { replacement, .. }
            | SearchQuery::Regex {
                replacement,
                escaped: true,
                ..
            } => replacement.clone().map(Cow::from),

            SearchQuery::Regex {
                regex,
                replacement: Some(replacement),
                escaped: false,
                ..
            } => {
                static TEXT_REPLACEMENT_SPECIAL_CHARACTERS_REGEX: LazyLock<Regex> =
                    LazyLock::new(|| Regex::new(r"\\\\|\\n|\\t").unwrap());
                let replacement = TEXT_REPLACEMENT_SPECIAL_CHARACTERS_REGEX.replace_all(
                    replacement,
                    |c: &Captures<str>| match c.get(0).unwrap().as_str() {
                        r"\\" => "\\",
                        r"\n" => "\n",
                        r"\t" => "\t",
                        x => unreachable!("Unexpected escape sequence: {}", x),
                    },
                );
                let captures = regex
                    .captures_from_pos(line, hit.start)
                    .ok()
                    .flatten()
                    .filter(|captures| captures.get(0).is_some_and(|m| m.range() == hit));
                let Some(captures) = captures else {
                    // The pattern is not guaranteed to match the whole line, for instance when
                    // searching within a selection that starts or ends mid-line, so fall back to
                    // matching the hit on its own.
                    return Some(regex.replace(line.get(hit)?, replacement));
                };
                let mut replaced = String::new();
                captures.expand(&replacement, &mut replaced);
                Some(Cow::Owned(replaced))
            }

            SearchQuery::Regex {
                replacement: None, ..
            } => None,
        }
    }

    pub async fn search(
        &self,
        buffer: &BufferSnapshot,
        subrange: Option<Range<usize>>,
    ) -> Vec<Range<usize>> {
        const YIELD_INTERVAL: usize = 20000;

        if self.as_str().is_empty() {
            return Default::default();
        }

        let range_offset = subrange.as_ref().map(|r| r.start).unwrap_or(0);
        let rope = if let Some(range) = subrange {
            buffer.as_rope().slice(range)
        } else {
            buffer.as_rope().clone()
        };

        let mut matches = Vec::new();
        let is_whole_word = |range: Range<usize>| {
            let classifier = buffer.char_classifier_at(range_offset + range.start);
            is_whole_word_match(
                rope.reversed_chars_at(range.start)
                    .next()
                    .map(|c| classifier.kind(c)),
                rope.chars_at(range.start)
                    .next()
                    .map(|c| classifier.kind(c)),
                rope.reversed_chars_at(range.end)
                    .next()
                    .map(|c| classifier.kind(c)),
                rope.chars_at(range.end).next().map(|c| classifier.kind(c)),
            )
        };
        match self {
            Self::Text {
                search, whole_word, ..
            } => {
                for (ix, mat) in search
                    .stream_find_iter(rope.bytes_in_range(0..rope.len()))
                    .enumerate()
                {
                    if (ix + 1) % YIELD_INTERVAL == 0 {
                        yield_now().await;
                    }

                    let mat = mat.unwrap();
                    if *whole_word && !is_whole_word(mat.start()..mat.end()) {
                        continue;
                    }
                    matches.push(mat.start()..mat.end())
                }
            }

            Self::Regex {
                regex,
                one_match_per_line,
                whole_word,
                escaped,
                ..
            } => {
                let text = rope.to_string();
                let mut seen_lines = BTreeSet::default();
                for (ix, mat) in regex.find_iter(&text).enumerate() {
                    if (ix + 1) % YIELD_INTERVAL == 0 {
                        yield_now().await;
                    }

                    if let std::result::Result::Ok(mat) = mat {
                        if *escaped && *whole_word && !is_whole_word(mat.start()..mat.end()) {
                            continue;
                        }
                        let should_push = if *one_match_per_line {
                            // ensure that only one match per line is returned.
                            let pos = buffer.offset_to_point(mat.start());
                            seen_lines.insert(pos.row)
                        } else {
                            true
                        };
                        if should_push {
                            matches.push(mat.start()..mat.end());
                        }
                    }
                }
            }
        }

        matches
    }

    pub fn is_empty(&self) -> bool {
        self.as_str().is_empty()
    }

    pub fn as_str(&self) -> &str {
        self.as_inner().as_str()
    }

    pub fn whole_word(&self) -> bool {
        match self {
            Self::Text { whole_word, .. } => *whole_word,
            Self::Regex { whole_word, .. } => *whole_word,
        }
    }

    pub fn case_sensitive(&self) -> bool {
        match self {
            Self::Text { case_sensitive, .. } => *case_sensitive,
            Self::Regex { case_sensitive, .. } => *case_sensitive,
        }
    }

    pub fn include_ignored(&self) -> bool {
        match self {
            Self::Text {
                include_ignored, ..
            } => *include_ignored,
            Self::Regex {
                include_ignored, ..
            } => *include_ignored,
        }
    }

    pub fn is_regex(&self) -> bool {
        matches!(self, Self::Regex { .. })
    }

    pub fn replacement_requires_context(&self) -> bool {
        matches!(self, Self::Regex { escaped: false, .. })
    }

    pub fn files_to_include(&self) -> &PathMatcher {
        self.as_inner().files_to_include()
    }

    pub fn files_to_exclude(&self) -> &PathMatcher {
        self.as_inner().files_to_exclude()
    }

    pub fn buffers(&self) -> Option<&Vec<Entity<Buffer>>> {
        self.as_inner().buffers.as_ref()
    }

    pub fn is_opened_only(&self) -> bool {
        self.as_inner().buffers.is_some()
    }

    pub fn filters_path(&self) -> bool {
        !(self.files_to_exclude().sources().next().is_none()
            && self.files_to_include().sources().next().is_none())
    }

    pub fn match_full_paths(&self) -> bool {
        self.as_inner().match_full_paths
    }

    /// Check match full paths to determine whether you're required to pass a fully qualified
    /// project path (starts with a project root).
    pub fn match_path(&self, file_path: &RelPath) -> bool {
        let mut path = file_path.to_rel_path_buf();
        loop {
            if self.files_to_exclude().is_match(&path) {
                return false;
            } else if self.files_to_include().sources().next().is_none()
                || self.files_to_include().is_match(&path)
            {
                return true;
            } else if !path.pop() {
                return false;
            }
        }
    }
    pub fn as_inner(&self) -> &SearchInputs {
        match self {
            Self::Regex { inner, .. } | Self::Text { inner, .. } => inner,
        }
    }

    pub fn search_str(&self, text: &str) -> Vec<Range<usize>> {
        if self.as_str().is_empty() {
            return Vec::new();
        }

        let classifier = CharClassifier::default();
        let is_whole_word = |range: Range<usize>| {
            is_whole_word_match(
                text[..range.start]
                    .chars()
                    .next_back()
                    .map(|c| classifier.kind(c)),
                text[range.clone()]
                    .chars()
                    .next()
                    .map(|c| classifier.kind(c)),
                text[range.clone()]
                    .chars()
                    .next_back()
                    .map(|c| classifier.kind(c)),
                text[range.end..].chars().next().map(|c| classifier.kind(c)),
            )
        };

        let mut matches = Vec::new();
        match self {
            Self::Text {
                search, whole_word, ..
            } => {
                for mat in search.find_iter(text.as_bytes()) {
                    if *whole_word && !is_whole_word(mat.start()..mat.end()) {
                        continue;
                    }
                    matches.push(mat.start()..mat.end());
                }
            }
            Self::Regex {
                regex,
                whole_word,
                escaped,
                ..
            } => {
                for mat in regex.find_iter(text).flatten() {
                    if *escaped && *whole_word && !is_whole_word(mat.start()..mat.end()) {
                        continue;
                    }
                    matches.push(mat.start()..mat.end());
                }
            }
        }
        matches
    }
}
