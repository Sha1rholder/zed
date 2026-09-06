use std::io::BufReader;

use language::Buffer;
use project::search::SearchQuery;
use text::Rope;
use util::{
    paths::{PathMatcher, PathStyle},
    rel_path::RelPath,
};

#[gpui::test]
async fn test_whitespace_delimited_whole_word_search(cx: &mut gpui::TestAppContext) {
    for (query, case_sensitive, marked) in [
        ("abc", true, "中文«abc»中文 «abc» abcdef xabc abc_x"),
        ("abc", false, "中文«ABC»中文 «abc» abcdef xabc abc_x"),
        ("中文", true, "abc«中文»def 中文字 中文かな한글 «中文»"),
        ("中文", false, "abc«中文»def 中文字 中文かな한글 «中文»"),
        ("é", false, "中«É»文 xÉ «é»"),
        ("abc中文", true, "中文«abc中文»def abc中文多"),
        ("abc", true, "ไทยabc abcالعربية abc_123"),
        ("abc", true, "カタカナー«abc»ﾊﾝｶｸｰ «abc»𠀀"),
    ] {
        let (text, expected) = util::test::marked_text_ranges(marked, false);
        let search = SearchQuery::text(
            query,
            true,
            case_sensitive,
            false,
            Default::default(),
            Default::default(),
            false,
            None,
        )
        .unwrap();
        let snapshot =
            cx.update(|cx| Buffer::build_snapshot_sync(Rope::from(text.as_str()), None, None, cx));
        assert_eq!(
            search.search(&snapshot, None).await,
            expected,
            "{query:?}: {text}"
        );
        assert_eq!(search.search_str(&text), expected, "{query:?}: {text}");
        if !expected.is_empty() {
            assert!(
                search
                    .detect(BufReader::new(Box::new(std::io::Cursor::new(
                        text.into_bytes()
                    ))))
                    .await
                    .unwrap()
                    .is_some()
            );
        }
    }

    let search = SearchQuery::text(
        "文",
        false,
        false,
        false,
        Default::default(),
        Default::default(),
        false,
        None,
    )
    .unwrap();
    assert_eq!(search.search_str("中文abc中文"), [3..6, 12..15]);
}

#[gpui::test]
async fn test_whitespace_delimited_regex_word_boundaries(cx: &mut gpui::TestAppContext) {
    for (query, whole_word, marked) in [
        ("abc", true, "中文«abc»中文 xabc abc_x «abc»"),
        ("abc", false, "中文«abc»中文 x«abc» «abc»_x «abc»"),
        (r"\babc\b", true, "中文abc中文 xabc abc_x «abc»"),
        ("abc", true, "ー«abc»ｰ 々«abc»𠀀 한글«abc»"),
        ("abc", true, "ไทยabc abcالعربية"),
        ("edit\\(", true, "中文«edit(»arg) reedit(arg) «edit(»arg)"),
    ] {
        let (text, expected) = util::test::marked_text_ranges(marked, false);
        let search = SearchQuery::regex(
            query,
            whole_word,
            true,
            false,
            false,
            Default::default(),
            Default::default(),
            false,
            None,
        )
        .unwrap();
        let snapshot =
            cx.update(|cx| Buffer::build_snapshot_sync(Rope::from(text.as_str()), None, None, cx));
        assert_eq!(
            search.search(&snapshot, None).await,
            expected,
            "{query:?}: {text}"
        );
        assert_eq!(search.search_str(&text), expected, "{query:?}: {text}");
        if !expected.is_empty() {
            assert!(
                search
                    .detect(BufReader::new(Box::new(std::io::Cursor::new(
                        text.into_bytes()
                    ))))
                    .await
                    .unwrap()
                    .is_some()
            );
        }
    }

    let search = SearchQuery::regex(
        "a(b)c",
        true,
        true,
        false,
        false,
        Default::default(),
        Default::default(),
        false,
        None,
    )
    .unwrap()
    .with_replacement("B=$1".into());
    let text = "中文abc中文";
    assert_eq!(search.search_str(text), [6..9]);
    assert_eq!(search.replacement_for(text, 6..9).as_deref(), Some("B=b"));

    let (text, expected) = util::test::marked_text_ranges("中文«abc»中文 abc\n«abc»中文", false);
    let search = SearchQuery::regex(
        "abc",
        true,
        true,
        false,
        true,
        Default::default(),
        Default::default(),
        false,
        None,
    )
    .unwrap();
    let snapshot =
        cx.update(|cx| Buffer::build_snapshot_sync(Rope::from(text.as_str()), None, None, cx));
    assert_eq!(search.search(&snapshot, None).await, expected);
}

#[test]
fn path_matcher_creation_for_valid_paths() {
    for valid_path in [
        "file",
        "Cargo.toml",
        ".DS_Store",
        "~/dir/another_dir/",
        "./dir/file",
        "dir/[a-z].txt",
    ] {
        let path_matcher = PathMatcher::new(&[valid_path.to_owned()], PathStyle::local())
            .unwrap_or_else(|e| panic!("Valid path {valid_path} should be accepted, but got: {e}"));
        assert!(
            path_matcher.is_match(&RelPath::new(valid_path.as_ref(), PathStyle::local()).unwrap()),
            "Path matcher for valid path {valid_path} should match itself"
        )
    }
}

#[test]
fn path_matcher_creation_for_globs() {
    for invalid_glob in ["dir/[].txt", "dir/[a-z.txt", "dir/{file"] {
        match PathMatcher::new(&[invalid_glob.to_owned()], PathStyle::local()) {
            Ok(_) => panic!("Invalid glob {invalid_glob} should not be accepted"),
            Err(_expected) => {}
        }
    }

    for valid_glob in [
        "dir/?ile",
        "dir/*.txt",
        "dir/**/file",
        "dir/[a-z].txt",
        "{dir,file}",
    ] {
        match PathMatcher::new(&[valid_glob.to_owned()], PathStyle::local()) {
            Ok(_expected) => {}
            Err(e) => panic!("Valid glob should be accepted, but got: {e}"),
        }
    }
}

#[test]
fn test_case_sensitive_pattern_items() {
    let case_sensitive = false;
    let search_query = SearchQuery::regex(
        "test\\C",
        false,
        case_sensitive,
        false,
        false,
        Default::default(),
        Default::default(),
        false,
        None,
    )
    .expect("Should be able to create a regex SearchQuery");

    assert_eq!(
        search_query.case_sensitive(),
        true,
        "Case sensitivity should be enabled when \\C pattern item is present in the query."
    );

    let case_sensitive = true;
    let search_query = SearchQuery::regex(
        "test\\c",
        true,
        case_sensitive,
        false,
        false,
        Default::default(),
        Default::default(),
        false,
        None,
    )
    .expect("Should be able to create a regex SearchQuery");

    assert_eq!(
        search_query.case_sensitive(),
        false,
        "Case sensitivity should be disabled when \\c pattern item is present, even if initially set to true."
    );

    let case_sensitive = false;
    let search_query = SearchQuery::regex(
        "test\\c\\C",
        false,
        case_sensitive,
        false,
        false,
        Default::default(),
        Default::default(),
        false,
        None,
    )
    .expect("Should be able to create a regex SearchQuery");

    assert_eq!(
        search_query.case_sensitive(),
        true,
        "Case sensitivity should be enabled when \\C is the last pattern item, even after a \\c."
    );

    let case_sensitive = false;
    let search_query = SearchQuery::regex(
        "tests\\\\C",
        false,
        case_sensitive,
        false,
        false,
        Default::default(),
        Default::default(),
        false,
        None,
    )
    .expect("Should be able to create a regex SearchQuery");

    assert_eq!(
        search_query.case_sensitive(),
        false,
        "Case sensitivity should not be enabled when \\C pattern item is preceded by a backslash."
    );
}

#[gpui::test]
async fn test_multiline_regex_crlf(cx: &mut gpui::TestAppContext) {
    let search_query = SearchQuery::regex(
        "^hello$\r?\n",
        false,
        false,
        false,
        false,
        Default::default(),
        Default::default(),
        false,
        None,
    )
    .expect("Should be able to create a regex SearchQuery");

    let text = Rope::from("hello\r\nworld\r\nhello\r\nworld");
    let snapshot = cx
        .update(|app| Buffer::build_snapshot(text, None, None, None, app))
        .await;

    let results = search_query.search(&snapshot, None).await;
    assert_eq!(results, vec![0..7, 14..21]);
}

#[gpui::test]
async fn test_multiline_regex(cx: &mut gpui::TestAppContext) {
    let search_query = SearchQuery::regex(
        "^hello$\n",
        false,
        false,
        false,
        false,
        Default::default(),
        Default::default(),
        false,
        None,
    )
    .expect("Should be able to create a regex SearchQuery");

    let text = Rope::from("hello\nworld\nhello\nworld");
    let snapshot = cx
        .update(|app| Buffer::build_snapshot(text, None, None, None, app))
        .await;

    let results = search_query.search(&snapshot, None).await;
    assert_eq!(results, vec![0..6, 12..18]);
}

#[gpui::test]
async fn regex_with_eol_detects_lines() {
    let re = SearchQuery::regex(
        "Bool$",
        false,
        false,
        false,
        false,
        PathMatcher::default(),
        PathMatcher::default(),
        false,
        None,
    )
    .unwrap();
    let input = " Bool\nsomething else";
    let result = re
        .detect(BufReader::new(Box::new(input.as_bytes())))
        .await
        .unwrap();
    assert!(result.is_some());
}

#[gpui::test]
async fn multi_line_regex_detects_matches() {
    let re = SearchQuery::regex(
        "Bool$\nbool",
        false,
        false,
        false,
        false,
        PathMatcher::default(),
        PathMatcher::default(),
        false,
        None,
    )
    .unwrap();
    let input = " Bool\nbool";
    let result = re
        .detect(BufReader::new(Box::new(input.as_bytes())))
        .await
        .unwrap();
    assert!(result.is_some());
}
