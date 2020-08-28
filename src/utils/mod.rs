#![allow(missing_docs)] // FIXME: Document this

pub mod fs;
mod string;
pub(crate) mod toml_ext;
use crate::errors::Error;
use regex::Regex;

use pulldown_cmark::{html, CodeBlockKind, CowStr, Event, Options, Parser, Tag};

use std::borrow::Cow;
use std::fmt::Write;
use std::path::{Path, PathBuf};

pub use self::string::{
    take_anchored_lines, take_lines, take_rustdoc_include_anchored_lines,
    take_rustdoc_include_lines,
};

lazy_static! {
    static ref SCHEME_LINK: Regex = Regex::new(r"^[a-z][a-z0-9+.-]*:").unwrap();
    static ref MD_LINK: Regex = Regex::new(r"(?P<link>.*)\.md(?P<anchor>#.*)?").unwrap();
}

/// Replaces multiple consecutive whitespace characters with a single space character.
pub fn collapse_whitespace(text: &str) -> Cow<'_, str> {
    lazy_static! {
        static ref RE: Regex = Regex::new(r"\s\s+").unwrap();
    }
    RE.replace_all(text, " ")
}

/// Convert the given string to a valid HTML element ID.
/// The only restriction is that the ID must not contain any ASCII whitespace.
pub fn normalize_id(content: &str) -> String {
    content
        .chars()
        .filter_map(|ch| {
            if ch.is_alphanumeric() || ch == '_' || ch == '-' {
                Some(ch.to_ascii_lowercase())
            } else if ch.is_whitespace() {
                Some('-')
            } else {
                None
            }
        })
        .collect::<String>()
}

/// Generate an ID for use with anchors which is derived from a "normalised"
/// string.
pub fn id_from_content(content: &str) -> String {
    let mut content = content.to_string();

    // Skip any tags or html-encoded stuff
    const REPL_SUB: &[&str] = &[
        "<em>",
        "</em>",
        "<code>",
        "</code>",
        "<strong>",
        "</strong>",
        "&lt;",
        "&gt;",
        "&amp;",
        "&#39;",
        "&quot;",
    ];
    for sub in REPL_SUB {
        content = content.replace(sub, "");
    }

    // Remove spaces and hashes indicating a header
    let trimmed = content.trim().trim_start_matches('#').trim();

    normalize_id(trimmed)
}

fn md_to_html_link<'a>(dest: &CowStr<'a>, fixed_link: &mut String) {
    if let Some(caps) = MD_LINK.captures(&dest) {
        fixed_link.push_str(&caps["link"]);
        fixed_link.push_str(".html");
        if let Some(anchor) = caps.name("anchor") {
            fixed_link.push_str(anchor.as_str());
        }
    } else {
        fixed_link.push_str(&dest);
    };
}

fn fix<'a, P: AsRef<Path>>(
    dest: CowStr<'a>,
    path: Option<&Path>,
    src_dir: Option<&Path>,
    fallback_path: &Option<P>,
) -> CowStr<'a> {
    if dest.starts_with('#') {
        // Fragment-only link.
        if let Some(path) = path {
            let mut base = path.display().to_string();
            if base.ends_with(".md") {
                base.replace_range(base.len() - 3.., ".html");
            }
            return format!("{}{}", base, dest).into();
        } else {
            return dest;
        }
    }
    // Don't modify links with schemes like `https`.
    if !SCHEME_LINK.is_match(&dest) {
        // This is a relative link, adjust it as necessary.
        let mut fixed_link = String::new();

        // If this link is missing on the filesystem in the current directory,
        // but not in the fallback directory, use the fallback's page.
        let mut redirected_path = false;
        if let Some(src_dir) = src_dir {
            let mut dest_path = src_dir.to_str().unwrap().to_string();
            write!(dest_path, "/{}", dest).unwrap();
            trace!("Check existing: {:?}", dest_path);
            if !PathBuf::from(dest_path).exists() {
                if let Some(fallback_path) = fallback_path {
                    let mut fallback_file = src_dir.to_str().unwrap().to_string();
                    // Check if there is a Markdown or other file in the fallback.
                    write!(
                        fallback_file,
                        "/{}/{}",
                        fallback_path.as_ref().display(),
                        dest
                    )
                    .unwrap();
                    trace!("Check fallback: {:?}", fallback_file);
                    if PathBuf::from(fallback_file).exists() {
                        write!(fixed_link, "{}/", fallback_path.as_ref().display()).unwrap();
                        debug!(
                            "Redirect link to default translation: {:?} -> {:?}",
                            dest, fixed_link
                        );
                        redirected_path = true;
                    }
                }
            }
        }

        if let Some(path) = path {
            let base = path
                .parent()
                .expect("path can't be empty")
                .to_str()
                .expect("utf-8 paths only");
            trace!("Base: {:?}", base);

            if !redirected_path && !base.is_empty() {
                write!(fixed_link, "{}/", base).unwrap();
            }
        }

        md_to_html_link(&dest, &mut fixed_link);
        return CowStr::from(fixed_link);
    }
    dest
}

fn fix_html<'a, P: AsRef<Path>>(
    html: CowStr<'a>,
    path: Option<&Path>,
    src_dir: Option<&Path>,
    fallback_path: &Option<P>,
) -> CowStr<'a> {
    // This is a terrible hack, but should be reasonably reliable. Nobody
    // should ever parse a tag with a regex. However, there isn't anything
    // in Rust that I know of that is suitable for handling partial html
    // fragments like those generated by pulldown_cmark.
    //
    // There are dozens of HTML tags/attributes that contain paths, so
    // feel free to add more tags if desired; these are the only ones I
    // care about right now.
    lazy_static! {
        static ref HTML_LINK: Regex =
            Regex::new(r#"(<(?:a|img) [^>]*?(?:src|href)=")([^"]+?)""#).unwrap();
    }

    HTML_LINK
        .replace_all(&html, move |caps: &regex::Captures<'_>| {
            let fixed = fix(caps[2].into(), path, src_dir, fallback_path);
            format!("{}{}\"", &caps[1], fixed)
        })
        .into_owned()
        .into()
}

/// Fix links to the correct location.
///
/// This adjusts links, such as turning `.md` extensions to `.html`.
///
/// `path` is the path to the page being rendered relative to the root of the
/// book. This is used for the `print.html` page so that links on the print
/// page go to the original location. Normal page rendering sets `path` to
/// None. Ideally, print page links would link to anchors on the print page,
/// but that is very difficult.
fn adjust_links<'a, P: AsRef<Path>>(
    event: Event<'a>,
    path: Option<&Path>,
    src_dir: Option<&Path>,
    fallback_path: &Option<P>,
) -> Event<'a> {
    match event {
        Event::Start(Tag::Link(link_type, dest, title)) => Event::Start(Tag::Link(
            link_type,
            fix(dest, path, src_dir, fallback_path),
            title,
        )),
        Event::Start(Tag::Image(link_type, dest, title)) => Event::Start(Tag::Image(
            link_type,
            fix(dest, path, src_dir, fallback_path),
            title,
        )),
        Event::Html(html) => Event::Html(fix_html(html, path, src_dir, fallback_path)),
        _ => event,
    }
}

/// Wrapper around the pulldown-cmark parser for rendering markdown to HTML.
pub fn render_markdown(text: &str, curly_quotes: bool) -> String {
    render_markdown_with_path(text, curly_quotes, None, None, &None::<PathBuf>)
}

pub fn new_cmark_parser(text: &str) -> Parser<'_> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_FOOTNOTES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    Parser::new_ext(text, opts)
}

pub fn render_markdown_with_path<P: AsRef<Path>>(
    text: &str,
    curly_quotes: bool,
    path: Option<&Path>,
    src_dir: Option<&Path>,
    fallback_path: &Option<P>,
) -> String {
    let mut s = String::with_capacity(text.len() * 3 / 2);
    let p = new_cmark_parser(text);
    let mut converter = EventQuoteConverter::new(curly_quotes);
    let events = p
        .map(clean_codeblock_headers)
        .map(|event| adjust_links(event, path, src_dir, fallback_path))
        .map(|event| converter.convert(event));

    html::push_html(&mut s, events);
    s
}

struct EventQuoteConverter {
    enabled: bool,
    convert_text: bool,
}

impl EventQuoteConverter {
    fn new(enabled: bool) -> Self {
        EventQuoteConverter {
            enabled,
            convert_text: true,
        }
    }

    fn convert<'a>(&mut self, event: Event<'a>) -> Event<'a> {
        if !self.enabled {
            return event;
        }

        match event {
            Event::Start(Tag::CodeBlock(_)) => {
                self.convert_text = false;
                event
            }
            Event::End(Tag::CodeBlock(_)) => {
                self.convert_text = true;
                event
            }
            Event::Text(ref text) if self.convert_text => {
                Event::Text(CowStr::from(convert_quotes_to_curly(text)))
            }
            _ => event,
        }
    }
}

fn clean_codeblock_headers(event: Event<'_>) -> Event<'_> {
    match event {
        Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(ref info))) => {
            let info: String = info.chars().filter(|ch| !ch.is_whitespace()).collect();

            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(CowStr::from(info))))
        }
        _ => event,
    }
}

fn convert_quotes_to_curly(original_text: &str) -> String {
    // We'll consider the start to be "whitespace".
    let mut preceded_by_whitespace = true;

    original_text
        .chars()
        .map(|original_char| {
            let converted_char = match original_char {
                '\'' => {
                    if preceded_by_whitespace {
                        '‘'
                    } else {
                        '’'
                    }
                }
                '"' => {
                    if preceded_by_whitespace {
                        '“'
                    } else {
                        '”'
                    }
                }
                _ => original_char,
            };

            preceded_by_whitespace = original_char.is_whitespace();

            converted_char
        })
        .collect()
}

/// Prints a "backtrace" of some `Error`.
pub fn log_backtrace(e: &Error) {
    error!("Error: {}", e);

    for cause in e.chain().skip(1) {
        error!("\tCaused By: {}", cause);
    }
}

#[cfg(test)]
mod tests {
    mod render_markdown {
        use super::super::{render_markdown, render_markdown_with_path};

        #[test]
        fn preserves_external_links() {
            assert_eq!(
                render_markdown("[example](https://www.rust-lang.org/)", false),
                "<p><a href=\"https://www.rust-lang.org/\">example</a></p>\n"
            );
        }

        #[test]
        fn it_can_adjust_markdown_links() {
            assert_eq!(
                render_markdown("[example](example.md)", false),
                "<p><a href=\"example.html\">example</a></p>\n"
            );
            assert_eq!(
                render_markdown("[example_anchor](example.md#anchor)", false),
                "<p><a href=\"example.html#anchor\">example_anchor</a></p>\n"
            );

            // this anchor contains 'md' inside of it
            assert_eq!(
                render_markdown("[phantom data](foo.html#phantomdata)", false),
                "<p><a href=\"foo.html#phantomdata\">phantom data</a></p>\n"
            );
        }

        #[test]
        fn it_can_keep_quotes_straight() {
            assert_eq!(render_markdown("'one'", false), "<p>'one'</p>\n");
        }

        #[test]
        fn it_can_make_quotes_curly_except_when_they_are_in_code() {
            let input = r#"
'one'
```
'two'
```
`'three'` 'four'"#;
            let expected = r#"<p>‘one’</p>
<pre><code>'two'
</code></pre>
<p><code>'three'</code> ‘four’</p>
"#;
            assert_eq!(render_markdown(input, true), expected);
        }

        #[test]
        fn whitespace_outside_of_codeblock_header_is_preserved() {
            let input = r#"
some text with spaces
```rust
fn main() {
// code inside is unchanged
}
```
more text with spaces
"#;

            let expected = r#"<p>some text with spaces</p>
<pre><code class="language-rust">fn main() {
// code inside is unchanged
}
</code></pre>
<p>more text with spaces</p>
"#;
            assert_eq!(render_markdown(input, false), expected);
            assert_eq!(render_markdown(input, true), expected);
        }

        #[test]
        fn rust_code_block_properties_are_passed_as_space_delimited_class() {
            let input = r#"
```rust,no_run,should_panic,property_3
```
"#;

            let expected = r#"<pre><code class="language-rust,no_run,should_panic,property_3"></code></pre>
"#;
            assert_eq!(render_markdown(input, false), expected);
            assert_eq!(render_markdown(input, true), expected);
        }

        #[test]
        fn rust_code_block_properties_with_whitespace_are_passed_as_space_delimited_class() {
            let input = r#"
```rust,    no_run,,,should_panic , ,property_3
```
"#;

            let expected = r#"<pre><code class="language-rust,no_run,,,should_panic,,property_3"></code></pre>
"#;
            assert_eq!(render_markdown(input, false), expected);
            assert_eq!(render_markdown(input, true), expected);
        }

        #[test]
        fn rust_code_block_without_properties_has_proper_html_class() {
            let input = r#"
```rust
```
"#;

            let expected = r#"<pre><code class="language-rust"></code></pre>
"#;
            assert_eq!(render_markdown(input, false), expected);
            assert_eq!(render_markdown(input, true), expected);

            let input = r#"
```rust
```
"#;
            assert_eq!(render_markdown(input, false), expected);
            assert_eq!(render_markdown(input, true), expected);
        }

        use std::fs::{self, File};
        use std::io::Write;
        use std::path::PathBuf;
        use tempfile::{Builder as TempFileBuilder, TempDir};

        const DUMMY_SRC: &str = "
# Dummy Chapter

this is some dummy text.

And here is some \
more text.
";

        /// Create a dummy `Link` in a temporary directory.
        fn dummy_link() -> (PathBuf, TempDir) {
            let temp = TempFileBuilder::new().prefix("book").tempdir().unwrap();

            let chapter_path = temp.path().join("chapter_1.md");
            File::create(&chapter_path)
                .unwrap()
                .write_all(DUMMY_SRC.as_bytes())
                .unwrap();

            let path = chapter_path.to_path_buf();

            (path, temp)
        }

        #[test]
        fn links_are_rewritten_to_fallback_for_nonexistent_files() {
            let input = r#"
[Link](chapter_1.md)
"#;

            let (localized_file, localized_dir) = dummy_link();
            fs::remove_file(&localized_file).unwrap();

            let (_, fallback_dir) = dummy_link();
            let mut relative_fallback_dir =
                PathBuf::from(super::super::fs::path_to_root(localized_dir.path()));
            relative_fallback_dir.push(fallback_dir.path().file_name().unwrap());

            let expected_fallback = format!(
                "<p><a href=\"{}/chapter_1.html\">Link</a></p>\n",
                relative_fallback_dir.display()
            );
            assert_eq!(
                render_markdown_with_path(
                    input,
                    false,
                    None,
                    Some(localized_dir.path()),
                    &Some(&relative_fallback_dir)
                ),
                expected_fallback
            );
            assert_eq!(
                render_markdown_with_path(
                    input,
                    true,
                    None,
                    Some(localized_dir.path()),
                    &Some(&relative_fallback_dir)
                ),
                expected_fallback
            );
        }
    }

    mod html_munging {
        use super::super::{id_from_content, normalize_id};

        #[test]
        fn it_generates_anchors() {
            assert_eq!(
                id_from_content("## Method-call expressions"),
                "method-call-expressions"
            );
            assert_eq!(id_from_content("## **Bold** title"), "bold-title");
            assert_eq!(id_from_content("## `Code` title"), "code-title");
        }

        #[test]
        fn it_generates_anchors_from_non_ascii_initial() {
            assert_eq!(
                id_from_content("## `--passes`: add more rustdoc passes"),
                "--passes-add-more-rustdoc-passes"
            );
            assert_eq!(
                id_from_content("## 中文標題 CJK title"),
                "中文標題-cjk-title"
            );
            assert_eq!(id_from_content("## Über"), "Über");
        }

        #[test]
        fn it_normalizes_ids() {
            assert_eq!(
                normalize_id("`--passes`: add more rustdoc passes"),
                "--passes-add-more-rustdoc-passes"
            );
            assert_eq!(
                normalize_id("Method-call 🐙 expressions \u{1f47c}"),
                "method-call--expressions-"
            );
            assert_eq!(normalize_id("_-_12345"), "_-_12345");
            assert_eq!(normalize_id("12345"), "12345");
            assert_eq!(normalize_id("中文"), "中文");
            assert_eq!(normalize_id("にほんご"), "にほんご");
            assert_eq!(normalize_id("한국어"), "한국어");
            assert_eq!(normalize_id(""), "");
        }
    }

    mod convert_quotes_to_curly {
        use super::super::convert_quotes_to_curly;

        #[test]
        fn it_converts_single_quotes() {
            assert_eq!(convert_quotes_to_curly("'one', 'two'"), "‘one’, ‘two’");
        }

        #[test]
        fn it_converts_double_quotes() {
            assert_eq!(convert_quotes_to_curly(r#""one", "two""#), "“one”, “two”");
        }

        #[test]
        fn it_treats_tab_as_whitespace() {
            assert_eq!(convert_quotes_to_curly("\t'one'"), "\t‘one’");
        }
    }
}
