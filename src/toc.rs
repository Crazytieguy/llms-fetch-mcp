//! Table of Contents generation for markdown documents.
//!
//! Extracts headings with line numbers, preserving original markdown syntax except
//! empty anchor links. Adaptively selects heading depth to fit within budget.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

pub const DEFAULT_TOC_BUDGET: usize = 4000;
pub const DEFAULT_TOC_THRESHOLD: usize = 8000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TocConfig {
    /// Maximum `ToC` size in bytes. Algorithm selects deepest heading level that fits.
    pub toc_budget: usize,
    /// Minimum document size to generate `ToC`. Smaller docs return `None`.
    pub full_content_threshold: usize,
}

impl Default for TocConfig {
    fn default() -> Self {
        Self {
            toc_budget: DEFAULT_TOC_BUDGET,
            full_content_threshold: DEFAULT_TOC_THRESHOLD,
        }
    }
}

/// Heading extracted from markdown.
///
/// Preserves original text except empty anchor links and setext underlines.
#[derive(Debug, Clone, PartialEq)]
pub struct Heading {
    /// Heading level from 1 (H1) to 6 (H6)
    pub level: u8,
    /// Line number where heading appears (1-indexed)
    pub line_number: usize,
    /// Heading text with formatting preserved
    pub text: String,
}

/// Check if text is empty or contains only whitespace/invisible/permalink characters.
///
/// Regular `trim()` doesn't remove zero-width spaces (U+200B), which are commonly
/// inserted by documentation generators in empty anchor links like `[​](#anchor)`.
/// We also exclude common permalink indicators like pilcrow (¶) which appear as `[¶](#anchor)`.
fn is_empty_or_invisible(text: &str) -> bool {
    text.chars().all(|c| {
        c.is_whitespace()
            || c == '\u{200B}' // ZERO WIDTH SPACE
            || c == '\u{FEFF}' // ZERO WIDTH NO-BREAK SPACE
            || c == '\u{200C}' // ZERO WIDTH NON-JOINER
            || c == '\u{200D}' // ZERO WIDTH JOINER
            || c == '\u{00B6}' // PILCROW SIGN (¶) - common permalink indicator
    })
}

/// Extracts headings with line numbers, filtering out empty anchor links.
#[allow(clippy::too_many_lines)]
fn extract_headings(markdown: &str) -> Vec<Heading> {
    use std::ops::Range;

    struct HeadingState {
        level: HeadingLevel,
        start: usize,
        line_number: usize,
        empty_link_ranges: Vec<Range<usize>>,
        current_link: Option<LinkState>,
    }

    struct LinkState {
        start: usize,
        text_content: String,
    }

    let mut headings = Vec::new();
    let mut current_heading: Option<HeadingState> = None;

    // Track line number incrementally to avoid O(n*h) rescanning
    let mut current_line = 1;
    let mut last_pos = 0;

    for (event, range) in Parser::new_ext(markdown, Options::all()).into_offset_iter() {
        // Update line number, handling overlapping/backward ranges
        if range.start > last_pos {
            current_line += markdown[last_pos..range.start]
                .chars()
                .filter(|&c| c == '\n')
                .count();
        }
        last_pos = last_pos.max(range.start);

        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                current_heading = Some(HeadingState {
                    level,
                    start: range.start,
                    line_number: current_line,
                    empty_link_ranges: Vec::new(),
                    current_link: None,
                });
            }
            Event::Start(Tag::Link { .. }) => {
                if let Some(heading) = &mut current_heading {
                    heading.current_link = Some(LinkState {
                        start: range.start,
                        text_content: String::new(),
                    });
                }
            }
            Event::Text(text) | Event::Code(text) => {
                // Collect text content from current link
                if let Some(heading) = &mut current_heading
                    && let Some(link) = &mut heading.current_link
                {
                    link.text_content.push_str(&text);
                }
            }
            Event::End(TagEnd::Link) => {
                if let Some(heading) = &mut current_heading
                    && let Some(link) = heading.current_link.take()
                {
                    // If link text is empty or only invisible chars, exclude it
                    if is_empty_or_invisible(&link.text_content) {
                        heading.empty_link_ranges.push(link.start..range.end);
                    }
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(mut heading) = current_heading.take() {
                    // Clear any unclosed link (defensive against malformed markdown)
                    heading.current_link = None;

                    // Extract full heading text
                    let full_text = markdown.get(heading.start..range.end).unwrap_or("");

                    // Build text excluding empty link ranges (convert absolute→relative offsets)
                    let mut text = String::new();
                    let mut last_end = 0;

                    for empty_range in &heading.empty_link_ranges {
                        let relative_start = empty_range.start.saturating_sub(heading.start);
                        let relative_end = empty_range.end.saturating_sub(heading.start);

                        if relative_start >= relative_end || relative_end > full_text.len() {
                            continue;
                        }

                        if last_end < relative_start
                            && let Some(slice) = full_text.get(last_end..relative_start)
                        {
                            text.push_str(slice);
                        }
                        last_end = relative_end;
                    }

                    if last_end < full_text.len()
                        && let Some(slice) = full_text.get(last_end..)
                    {
                        text.push_str(slice);
                    }

                    // Strip setext underlines (lines of = or - following the title)
                    let text = text.trim();
                    let text = if let Some(newline_pos) = text.rfind('\n') {
                        let after_newline = text[newline_pos + 1..].trim();
                        // Check if line after newline is all = or - (setext underline)
                        if !after_newline.is_empty()
                            && after_newline.chars().all(|c| c == '=' || c == '-')
                        {
                            &text[..newline_pos]
                        } else {
                            text
                        }
                    } else {
                        text
                    };

                    // Collapse consecutive spaces
                    let mut result = String::with_capacity(text.len());
                    let mut last_was_space = false;
                    for c in text.chars() {
                        if c == ' ' {
                            if !last_was_space {
                                result.push(c);
                                last_was_space = true;
                            }
                        } else {
                            result.push(c);
                            last_was_space = false;
                        }
                    }
                    let text = result.trim().to_string();

                    // Filter out headings that are only hashes/whitespace after empty link removal
                    let has_content = text.chars().any(|c| !c.is_whitespace() && c != '#');

                    if !text.is_empty() && has_content {
                        let level_num = match heading.level {
                            HeadingLevel::H1 => 1,
                            HeadingLevel::H2 => 2,
                            HeadingLevel::H3 => 3,
                            HeadingLevel::H4 => 4,
                            HeadingLevel::H5 => 5,
                            HeadingLevel::H6 => 6,
                        };

                        headings.push(Heading {
                            level: level_num,
                            line_number: heading.line_number,
                            text: text.to_string(),
                        });
                    }
                }
            }
            _ => {}
        }
    }

    headings
}

/// Returns deepest heading level that fits within budget, with rendered `ToC`.
fn find_optimal_level(headings: &[Heading], budget: usize) -> Option<(u8, String)> {
    if headings.is_empty() {
        return None;
    }

    let max_level = headings.iter().map(|h| h.level).max().unwrap_or(1);

    let mut best: Option<(u8, String)> = None;
    for level in 1..=max_level {
        let rendered = render_toc(headings, level);
        if rendered.is_empty() {
            continue; // Skip levels with no headings
        }

        let byte_size = rendered.len();
        if byte_size <= budget {
            best = Some((level, rendered));
        }
        // Don't break early - size may not increase monotonically
    }

    best
}

fn render_toc(headings: &[Heading], max_level: u8) -> String {
    use std::fmt::Write;

    let filtered: Vec<_> = headings.iter().filter(|h| h.level <= max_level).collect();

    if filtered.is_empty() {
        return String::new();
    }

    debug_assert!(!filtered.is_empty());
    let max_line_num = filtered.last().unwrap().line_number;

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let width = if max_line_num < 100 {
        3
    } else if max_line_num < 1000 {
        4
    } else if max_line_num < 10000 {
        5
    } else {
        ((max_line_num as f64).log10().floor() as usize + 1).max(3)
    };

    // Pre-allocate to reduce reallocations
    let estimated_size = filtered.len() * (width + 34);
    let mut result = String::with_capacity(estimated_size);

    for (i, h) in filtered.iter().enumerate() {
        if i > 0 {
            result.push('\n');
        }
        write!(result, "{:>width$}→{}", h.line_number, h.text).unwrap();
    }

    result
}

/// Generates `ToC` with format `{line_number}→{heading_text}` per line.
/// Returns `None` if document too small or no headings fit within budget.
pub fn generate_toc(markdown: &str, total_bytes: usize, config: &TocConfig) -> Option<String> {
    if total_bytes < config.full_content_threshold {
        return None;
    }

    let headings = extract_headings(markdown);
    if headings.is_empty() {
        return None;
    }

    let (_level, toc) = find_optimal_level(&headings, config.toc_budget)?;

    if toc.is_empty() { None } else { Some(toc) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> TocConfig {
        TocConfig::default()
    }

    #[test]
    fn test_extract_simple_headings() {
        let md = "# H1\n## H2\n### H3";
        let headings = extract_headings(md);
        assert_eq!(headings.len(), 3);
        assert_eq!(headings[0].level, 1);
        assert_eq!(headings[0].line_number, 1);
        assert_eq!(headings[0].text, "# H1");
        assert_eq!(headings[1].level, 2);
        assert_eq!(headings[1].text, "## H2");
    }

    #[test]
    fn test_ignore_fenced_code_blocks() {
        let md = "# Real\n```\n# Fake\n```\n## Also Real";
        let headings = extract_headings(md);
        assert_eq!(headings.len(), 2);
        assert_eq!(headings[0].text, "# Real");
        assert_eq!(headings[1].text, "## Also Real");
    }

    #[test]
    fn test_ignore_indented_code_blocks() {
        let md = "# Real\n\n    # Not a heading (indented)\n\n## Real2";
        let headings = extract_headings(md);
        assert_eq!(headings.len(), 2);
        assert_eq!(headings[0].text, "# Real");
        assert_eq!(headings[1].text, "## Real2");
    }

    #[test]
    fn test_setext_headings() {
        let md = "H1\n==\n\nH2\n--";
        let headings = extract_headings(md);
        assert_eq!(headings.len(), 2);
        assert_eq!(headings[0].level, 1);
        assert_eq!(headings[1].level, 2);
    }

    #[test]
    fn test_empty_links_excluded() {
        // Empty anchor links should be excluded
        let md = "## Writing markup with JSX [](#writing-markup-with-jsx)";
        let headings = extract_headings(md);
        assert_eq!(headings.len(), 1);
        assert_eq!(headings[0].text, "## Writing markup with JSX");

        // Multiple empty links - all excluded
        let md2 = "### Title [](#anchor1) [](#anchor2)";
        let headings2 = extract_headings(md2);
        assert_eq!(headings2.len(), 1);
        assert_eq!(headings2[0].text, "### Title");

        // No link - full text preserved
        let md3 = "# Simple Heading";
        let headings3 = extract_headings(md3);
        assert_eq!(headings3.len(), 1);
        assert_eq!(headings3[0].text, "# Simple Heading");

        // Link with text - KEPT (not excluded)
        let md4 = "## Title [link](url) more text";
        let headings4 = extract_headings(md4);
        assert_eq!(headings4.len(), 1);
        assert_eq!(headings4[0].text, "## Title [link](url) more text");

        // Mix of empty and non-empty links
        let md5 = "## Check [docs](url) for details [](#anchor)";
        let headings5 = extract_headings(md5);
        assert_eq!(headings5.len(), 1);
        assert_eq!(headings5[0].text, "## Check [docs](url) for details");

        // Whitespace collapsing: empty link removal should not leave double spaces
        let md6 = "## [¶](#anchor) Title with text";
        let headings6 = extract_headings(md6);
        assert_eq!(headings6.len(), 1);
        assert_eq!(headings6[0].text, "## Title with text");
        assert!(!headings6[0].text.contains("  ")); // No double spaces

        // Heading with only empty links should be filtered out
        let md7 = "## [](#anchor) [¶](#another)";
        let headings7 = extract_headings(md7);
        assert_eq!(headings7.len(), 0); // Filtered out entirely

        // Heading with only hashes and empty link should be filtered
        let md8 = "### [\u{200B}](#anchor)";
        let headings8 = extract_headings(md8);
        assert_eq!(headings8.len(), 0);
    }

    #[test]
    fn test_unicode_headings() {
        let md = "# 你好世界\n## 🎉 Emoji Heading";
        let headings = extract_headings(md);
        assert_eq!(headings.len(), 2);
        assert!(headings[0].text.contains("你好世界"));
        assert!(headings[1].text.contains("🎉"));
    }

    #[test]
    fn test_crlf_line_endings() {
        // Windows-style CRLF line endings should be counted correctly
        let md = "# First\r\n## Second\r\n### Third";
        let headings = extract_headings(md);
        assert_eq!(headings.len(), 3);
        assert_eq!(headings[0].line_number, 1);
        assert_eq!(headings[1].line_number, 2);
        assert_eq!(headings[2].line_number, 3);
        assert_eq!(headings[0].text, "# First");
        assert_eq!(headings[1].text, "## Second");
        assert_eq!(headings[2].text, "### Third");
    }

    #[test]
    fn test_mixed_line_endings() {
        // Mix of LF and CRLF should still count correctly
        let md = "# First\n## Second\r\n### Third\n#### Fourth";
        let headings = extract_headings(md);
        assert_eq!(headings.len(), 4);
        assert_eq!(headings[0].line_number, 1);
        assert_eq!(headings[1].line_number, 2);
        assert_eq!(headings[2].line_number, 3);
        assert_eq!(headings[3].line_number, 4);
    }

    #[test]
    fn test_headings_with_inline_formatting() {
        // Headings with bold, italic, code, and links preserved exactly
        let md = r"## **Bold** heading
### Heading with `code`
#### Heading with *italic* text
##### Mix **bold** and `code` and [link](url)";
        let headings = extract_headings(md);
        assert_eq!(headings.len(), 4);
        assert_eq!(headings[0].text, "## **Bold** heading");
        assert_eq!(headings[1].text, "### Heading with `code`");
        assert_eq!(headings[2].text, "#### Heading with *italic* text");
        assert_eq!(
            headings[3].text,
            "##### Mix **bold** and `code` and [link](url)"
        );
    }

    #[test]
    fn test_empty_document() {
        let md = "";
        let headings = extract_headings(md);
        assert_eq!(headings.len(), 0);

        let toc = generate_toc(md, md.len(), &TocConfig::default());
        assert!(toc.is_none());
    }

    #[test]
    fn test_document_with_no_headings() {
        let md = "Just some paragraph text.\n\nAnd another paragraph.";
        let headings = extract_headings(md);
        assert_eq!(headings.len(), 0);

        let toc = generate_toc(md, md.len(), &TocConfig::default());
        assert!(toc.is_none());
    }

    #[test]
    fn test_level_selection() {
        let headings = vec![
            Heading {
                level: 1,
                line_number: 1,
                text: "# ".repeat(50),
            },
            Heading {
                level: 2,
                line_number: 2,
                text: "## ".repeat(50),
            },
            Heading {
                level: 3,
                line_number: 3,
                text: "### ".repeat(50),
            },
        ];

        let result = find_optimal_level(&headings, 400);
        assert!(result.is_some());
        let (level, _toc) = result.unwrap();
        assert!(level >= 1);
    }

    #[test]
    fn test_empty_headings() {
        let headings: Vec<Heading> = vec![];
        let toc = render_toc(&headings, 3);
        assert_eq!(toc, "");
    }

    #[test]
    fn test_budget_pressure_returns_none() {
        let headings = vec![
            Heading {
                level: 1,
                line_number: 1,
                text: "# ".to_string() + &"x".repeat(10000),
            },
            Heading {
                level: 1,
                line_number: 2,
                text: "# ".to_string() + &"x".repeat(10000),
            },
        ];

        let level = find_optimal_level(&headings, 10);
        assert!(level.is_none());
    }

    #[test]
    fn test_generate_toc_handles_budget_exceeded() {
        let md = format!(
            "{}# Very Long Heading {}\n{}",
            "content\n".repeat(1000),
            "x".repeat(10000),
            "more\n".repeat(1000)
        );
        let toc = generate_toc(&md, md.len(), &default_config());
        assert!(toc.is_none());
    }

    #[test]
    fn test_deeply_nested_levels() {
        // Verify all 6 heading levels are recognized
        let md = r"# Main

## Level 2

### Level 3

#### Level 4

##### Level 5

###### Level 6
";
        let headings = extract_headings(md);
        assert_eq!(headings.len(), 6);
        assert_eq!(headings[0].level, 1);
        assert_eq!(headings[1].level, 2);
        assert_eq!(headings[2].level, 3);
        assert_eq!(headings[3].level, 4);
        assert_eq!(headings[4].level, 5);
        assert_eq!(headings[5].level, 6);
    }

    // Snapshot tests with real-world documentation
    mod snapshots {
        use super::*;

        #[test]
        fn snapshot_astro_excerpt() {
            let md = include_str!("../test-fixtures/astro-excerpt.txt");
            let toc = generate_toc(md, md.len(), &default_config());
            insta::assert_snapshot!(toc.unwrap_or_default());
        }

        #[test]
        fn snapshot_convex_excerpt() {
            let md = include_str!("../test-fixtures/convex-excerpt.txt");
            let toc = generate_toc(md, md.len(), &default_config());
            insta::assert_snapshot!(toc.unwrap_or_default());
        }

        #[test]
        fn snapshot_react_learn() {
            let md = include_str!("../test-fixtures/react-learn.txt");
            let toc = generate_toc(md, md.len(), &default_config());
            insta::assert_snapshot!(toc.unwrap_or_default());
        }

        #[test]
        fn snapshot_vue_intro() {
            let md = include_str!("../test-fixtures/vue-intro.txt");
            let toc = generate_toc(md, md.len(), &default_config());
            insta::assert_snapshot!(toc.unwrap_or_default());
        }

        #[test]
        fn snapshot_python_tutorial() {
            let md = include_str!("../test-fixtures/python-tutorial.txt");
            let toc = generate_toc(md, md.len(), &default_config());
            insta::assert_snapshot!(toc.unwrap_or_default());
        }

        #[test]
        fn snapshot_vite_guide() {
            let md = include_str!("../test-fixtures/vite-guide.txt");
            let toc = generate_toc(md, md.len(), &default_config());
            insta::assert_snapshot!(toc.unwrap_or_default());
        }

        #[test]
        fn snapshot_nextjs_llms() {
            let md = include_str!("../test-fixtures/nextjs-llms.txt");
            let toc = generate_toc(md, md.len(), &default_config());
            insta::assert_snapshot!(toc.unwrap_or_default());
        }

        #[test]
        fn snapshot_remix_quickstart() {
            let md = include_str!("../test-fixtures/remix-quickstart.txt");
            let toc = generate_toc(md, md.len(), &default_config());
            insta::assert_snapshot!(toc.unwrap_or_default());
        }

        #[test]
        fn snapshot_go_tutorial() {
            // Go tutorial is 6.3KB - use lower threshold
            let md = include_str!("../test-fixtures/go-tutorial.txt");
            let config = TocConfig {
                toc_budget: 4000,
                full_content_threshold: 2000,
            };
            let toc = generate_toc(md, md.len(), &config);
            insta::assert_snapshot!(toc.unwrap_or_default());
        }

        #[test]
        fn snapshot_tailwind_install() {
            // Tailwind install is 2.6KB - use lower threshold
            let md = include_str!("../test-fixtures/tailwind-install.txt");
            let config = TocConfig {
                toc_budget: 4000,
                full_content_threshold: 1000,
            };
            let toc = generate_toc(md, md.len(), &config);
            insta::assert_snapshot!(toc.unwrap_or_default());
        }

        #[test]
        fn snapshot_solidjs_quickstart() {
            // SolidJS quickstart is 1.9KB - use minimal threshold
            let md = include_str!("../test-fixtures/solidjs-quickstart.txt");
            let config = TocConfig {
                toc_budget: 4000,
                full_content_threshold: 500,
            };
            let toc = generate_toc(md, md.len(), &config);
            insta::assert_snapshot!(toc.unwrap_or_default());
        }

        #[test]
        fn snapshot_laravel_install() {
            let md = include_str!("../test-fixtures/laravel-install.txt");
            let toc = generate_toc(md, md.len(), &default_config());
            insta::assert_snapshot!(toc.unwrap_or_default());
        }

        #[test]
        fn snapshot_qwik_getting_started() {
            let md = include_str!("../test-fixtures/qwik-getting-started.txt");
            let toc = generate_toc(md, md.len(), &default_config());
            insta::assert_snapshot!(toc.unwrap_or_default());
        }

        #[test]
        fn snapshot_fastapi_tutorial() {
            let md = include_str!("../test-fixtures/fastapi-tutorial.txt");
            let toc = generate_toc(md, md.len(), &default_config());
            insta::assert_snapshot!(toc.unwrap_or_default());
        }

        #[test]
        fn snapshot_angular_install() {
            // Angular install is 3.8KB - use lower threshold
            let md = include_str!("../test-fixtures/angular-install.txt");
            let config = TocConfig {
                toc_budget: 4000,
                full_content_threshold: 2000,
            };
            let toc = generate_toc(md, md.len(), &config);
            insta::assert_snapshot!(toc.unwrap_or_default());
        }

        #[test]
        fn snapshot_kotlin_getting_started() {
            // Kotlin getting started is 3.3KB - use lower threshold
            let md = include_str!("../test-fixtures/kotlin-getting-started.txt");
            let config = TocConfig {
                toc_budget: 4000,
                full_content_threshold: 2000,
            };
            let toc = generate_toc(md, md.len(), &config);
            insta::assert_snapshot!(toc.unwrap_or_default());
        }

        #[test]
        fn snapshot_django_install() {
            // Django install is 3.1KB - use lower threshold
            let md = include_str!("../test-fixtures/django-install.txt");
            let config = TocConfig {
                toc_budget: 4000,
                full_content_threshold: 2000,
            };
            let toc = generate_toc(md, md.len(), &config);
            insta::assert_snapshot!(toc.unwrap_or_default());
        }
    }

    mod config_snapshots {
        use super::*;

        #[test]
        fn snapshot_small_budget_react() {
            // React doc is small - H3 ToC fits in 1500 bytes (same as default)
            let md = include_str!("../test-fixtures/react-learn.txt");
            let config = TocConfig {
                toc_budget: 1500,
                full_content_threshold: 8000,
            };
            let toc = generate_toc(md, md.len(), &config);
            insta::assert_snapshot!(toc.unwrap_or_default());
        }

        #[test]
        fn snapshot_large_budget_react() {
            // React doc is small - even large budget produces same H3 ToC
            let md = include_str!("../test-fixtures/react-learn.txt");
            let config = TocConfig {
                toc_budget: 10000,
                full_content_threshold: 8000,
            };
            let toc = generate_toc(md, md.len(), &config);
            insta::assert_snapshot!(toc.unwrap_or_default());
        }

        #[test]
        fn snapshot_low_threshold_small_doc() {
            // With a low threshold (2000 bytes), should generate ToC for smaller docs
            let md = include_str!("../test-fixtures/convex-excerpt.txt");
            let config = TocConfig {
                toc_budget: 4000,
                full_content_threshold: 2000,
            };
            let toc = generate_toc(md, md.len(), &config);
            insta::assert_snapshot!(toc.unwrap_or_default());
        }

        #[test]
        fn snapshot_astro_full_large_budget() {
            // With a very large budget (50000 bytes), should generate H1-only ToC for astro-llms-full
            let md = include_str!("../test-fixtures/astro-llms-full.txt");
            let config = TocConfig {
                toc_budget: 50000,
                full_content_threshold: 8000,
            };
            let toc = generate_toc(md, md.len(), &config);
            insta::assert_snapshot!(toc.unwrap_or_default());
        }

        #[test]
        fn snapshot_convex_full_large_budget() {
            // With a very large budget (50000 bytes), should generate H1-only ToC for convex-llms-full
            let md = include_str!("../test-fixtures/convex-llms-full.txt");
            let config = TocConfig {
                toc_budget: 50000,
                full_content_threshold: 8000,
            };
            let toc = generate_toc(md, md.len(), &config);
            insta::assert_snapshot!(toc.unwrap_or_default());
        }

        #[test]
        fn snapshot_very_tight_budget_python() {
            // With a very tight budget (300 bytes), should fit only 2-3 headings
            let md = include_str!("../test-fixtures/python-tutorial.txt");
            let config = TocConfig {
                toc_budget: 300,
                full_content_threshold: 2000,
            };
            let toc = generate_toc(md, md.len(), &config);
            insta::assert_snapshot!(toc.unwrap_or_default());
        }

        #[test]
        fn snapshot_minimal_threshold_convex() {
            // With a minimal threshold (1000 bytes), small docs generate ToC
            let md = include_str!("../test-fixtures/convex-excerpt.txt");
            let config = TocConfig {
                toc_budget: 4000,
                full_content_threshold: 1000,
            };
            let toc = generate_toc(md, md.len(), &config);
            insta::assert_snapshot!(toc.unwrap_or_default());
        }

        #[test]
        fn snapshot_deep_nesting_convex_full() {
            // Convex full has H4/H5 nesting - test with budget allowing deeper levels
            let md = include_str!("../test-fixtures/convex-llms-full.txt");
            let config = TocConfig {
                toc_budget: 100_000,
                full_content_threshold: 8000,
            };
            let toc = generate_toc(md, md.len(), &config);
            insta::assert_snapshot!(toc.unwrap_or_default());
        }
    }

    // Regular unit tests for edge cases (not snapshots)
    mod large_files {
        use super::*;

        #[test]
        fn test_astro_llms_full_exceeds_budget() {
            // Full Astro docs: 2.4MB, 424+ H1 headings
            // Even H1-only would exceed 1000 token budget
            let md = include_str!("../test-fixtures/astro-llms-full.txt");
            let toc = generate_toc(md, md.len(), &default_config());
            assert!(
                toc.is_none(),
                "Should not generate ToC when even H1s exceed budget"
            );
        }

        #[test]
        fn test_convex_llms_full_exceeds_budget() {
            // Full Convex docs: 1.8MB, 296+ H1 headings
            let md = include_str!("../test-fixtures/convex-llms-full.txt");
            let toc = generate_toc(md, md.len(), &default_config());
            assert!(
                toc.is_none(),
                "Should not generate ToC when even H1s exceed budget"
            );
        }
    }

    mod config_tests {
        use super::*;

        #[test]
        fn test_custom_budget_allows_more_headings() {
            let md = include_str!("../test-fixtures/python-tutorial.txt");

            let small_budget = TocConfig {
                toc_budget: 500,
                full_content_threshold: 2000,
            };
            let large_budget = TocConfig {
                toc_budget: 10000,
                full_content_threshold: 2000,
            };

            let toc_small = generate_toc(md, md.len(), &small_budget);
            let toc_large = generate_toc(md, md.len(), &large_budget);

            assert!(toc_small.is_some());
            assert!(toc_large.is_some());

            let small_len = toc_small.unwrap().len();
            let large_len = toc_large.unwrap().len();
            assert!(
                large_len >= small_len,
                "Larger budget should allow same or more headings"
            );
        }

        #[test]
        fn test_higher_threshold_skips_more_docs() {
            let md = include_str!("../test-fixtures/vue-intro.txt");

            let low_threshold = TocConfig {
                toc_budget: 1000,
                full_content_threshold: 1000,
            };
            let high_threshold = TocConfig {
                toc_budget: 1000,
                full_content_threshold: 100_000,
            };

            let toc_low = generate_toc(md, md.len(), &low_threshold);
            let toc_high = generate_toc(md, md.len(), &high_threshold);

            assert!(toc_low.is_some(), "Low threshold should generate ToC");
            assert!(toc_high.is_none(), "High threshold should skip ToC");
        }

        #[test]
        fn test_zero_threshold_always_generates() {
            let small_md = "# Title\nContent.";

            let config = TocConfig {
                toc_budget: 1000,
                full_content_threshold: 0,
            };

            let toc = generate_toc(small_md, small_md.len(), &config);
            assert!(toc.is_some(), "Zero threshold should always generate ToC");
        }

        #[test]
        fn test_tiny_budget_returns_none() {
            let md = include_str!("../test-fixtures/react-learn.txt");

            let tiny_budget = TocConfig {
                toc_budget: 10,
                full_content_threshold: 2000,
            };

            let toc = generate_toc(md, md.len(), &tiny_budget);
            assert!(
                toc.is_none(),
                "Budget too small for even H1s should return None"
            );
        }

        #[test]
        fn test_config_default_values() {
            let config = TocConfig::default();
            assert_eq!(config.toc_budget, DEFAULT_TOC_BUDGET);
            assert_eq!(config.full_content_threshold, DEFAULT_TOC_THRESHOLD);
        }
    }
}
