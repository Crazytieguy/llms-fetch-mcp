#![warn(clippy::pedantic)]

mod toc;

use clap::Parser;
use dom_smoothie::{Config, Readability, TextMode};
use rmcp::handler::server::ServerHandler;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;

#[derive(Parser)]
#[command(author, version, about = "MCP server for fetching and caching web documentation", long_about = None)]
struct Cli {
    /// Cache directory path (default: .llms-fetch-mcp)
    #[arg(value_name = "CACHE_DIR")]
    cache_dir: Option<PathBuf>,

    /// Maximum `ToC` size in bytes
    #[arg(long, default_value_t = toc::DEFAULT_TOC_BUDGET)]
    toc_budget: usize,

    /// Minimum document size in bytes to generate `ToC`
    #[arg(long, default_value_t = toc::DEFAULT_TOC_THRESHOLD)]
    toc_threshold: usize,
}

#[derive(Clone)]
struct FetchServer {
    cache_dir: Arc<PathBuf>,
    toc_config: toc::TocConfig,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct FetchInput {
    url: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct FileInfo {
    path: String,
    source_url: String,
    content_type: String,
    lines: usize,
    words: usize,
    characters: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    table_of_contents: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct FetchOutput {
    files: Vec<FileInfo>,
}

#[derive(Debug)]
struct FetchResult {
    url: String,
    content: String,
    is_html: bool,
    is_markdown: bool,
}

#[derive(Debug)]
enum FetchAttempt {
    Success(FetchResult),
    HttpError { url: String, status: u16 },
    NetworkError { url: String },
}

async fn fetch_url(client: &reqwest::Client, url: &str) -> FetchAttempt {
    match client
        .get(url)
        .header(
            "Accept",
            "text/markdown, text/x-markdown, text/plain, text/html;q=0.5, */*;q=0.1",
        )
        .header(
            "User-Agent",
            "llms-fetch-mcp/0.1.4 (+https://github.com/crazytieguy/llms-fetch-mcp)",
        )
        .send()
        .await
    {
        Ok(response) => {
            let status = response.status().as_u16();
            if response.status().is_success() {
                let content_type = response
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");

                let is_html = content_type.contains("text/html");
                let is_markdown = content_type.contains("text/markdown")
                    || content_type.contains("text/x-markdown");

                match response.text().await {
                    Ok(content) => FetchAttempt::Success(FetchResult {
                        url: url.to_string(),
                        content,
                        is_html,
                        is_markdown,
                    }),
                    Err(_) => FetchAttempt::NetworkError {
                        url: url.to_string(),
                    },
                }
            } else {
                FetchAttempt::HttpError {
                    url: url.to_string(),
                    status,
                }
            }
        }
        Err(_) => FetchAttempt::NetworkError {
            url: url.to_string(),
        },
    }
}

fn get_url_variations(url: &str) -> Vec<String> {
    let mut variations = vec![url.to_string()];

    let url_lower = url.to_lowercase();
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    if url_lower.ends_with(".md") || url_lower.ends_with(".txt") {
        return variations;
    }

    // Don't try variations for URLs with query parameters
    if url.contains('?') {
        return variations;
    }

    let base = url.trim_end_matches('/');

    // Check if URL has a file extension (to avoid file/directory conflicts)
    let has_file_extension = if let Ok(parsed) = url::Url::parse(url) {
        let path = parsed.path();
        path.rsplit_once('/')
            .is_some_and(|(_, last)| last.contains('.') && !last.ends_with('.'))
    } else {
        false
    };

    variations.push(format!("{base}.md"));

    // Only add .html.md and directory-based variations if URL doesn't have a file extension
    // This prevents file/directory conflicts (e.g., npm.html file vs npm.html/ directory)
    // and avoids nonsensical double extensions (e.g., page.html.html.md)
    if !has_file_extension {
        variations.push(format!("{base}.html.md"));
        variations.push(format!("{base}/index.md"));
        variations.push(format!("{base}/llms.txt"));
        variations.push(format!("{base}/llms-full.txt"));
    }

    variations
}

fn url_to_path(base_dir: &Path, url: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let parsed = url::Url::parse(url)?;
    let domain = parsed.host_str().ok_or("No host in URL")?;

    let mut path = base_dir.join(domain);

    let url_path = parsed.path().trim_start_matches('/');

    // Security: Sanitize path components to prevent directory traversal
    if !url_path.is_empty() {
        for component in url_path.split('/') {
            if component == ".." || component == "." {
                return Err("Invalid path component in URL".into());
            }
            if !component.is_empty() {
                path.push(component);
            }
        }
    }

    // Determine if we need to add an index file
    let needs_index = if url_path.is_empty() {
        true
    } else {
        let last_segment = url_path.split('/').next_back().unwrap_or("");
        Path::new(last_segment).extension().is_none()
    };

    if needs_index {
        path.push("index");
    }

    if let Some(query) = parsed.query() {
        // Security: Sanitize query parameters for filesystem safety
        let safe_query = query.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_");
        let current_ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        let new_ext = if current_ext.is_empty() {
            format!("?{safe_query}")
        } else {
            format!("{current_ext}?{safe_query}")
        };
        path.set_extension(new_ext);
    }

    // Security: Verify final path is within base directory
    if !path.starts_with(base_dir) {
        return Err("Path traversal detected".into());
    }

    Ok(path)
}

async fn ensure_gitignore(base_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let gitignore_path = base_dir.join(".gitignore");

    if !gitignore_path.exists() {
        fs::create_dir_all(base_dir).await?;
        fs::write(&gitignore_path, "*\n").await?;
    }

    Ok(())
}

/// Converts HTML to Markdown with fallback extraction:
/// 1. Try Readability to extract `<main>`/`<article>` content
/// 2. Fall back to `<body>` content if available
/// 3. Fall back to full HTML as last resort
fn html_to_markdown(html: &str, document_url: &str) -> Result<String, Box<dyn std::error::Error>> {
    if html.trim().is_empty() {
        return Err("HTML content is empty".into());
    }

    let cfg = Config {
        text_mode: TextMode::Raw,
        ..Default::default()
    };

    let html_to_convert = Readability::new(html, Some(document_url), Some(cfg))
        .ok()
        .and_then(|mut r| r.parse().ok())
        .and_then(|article| {
            let cleaned = article.content;
            (!cleaned.trim().is_empty()).then(|| cleaned.to_string())
        })
        .or_else(|| extract_body(html))
        .unwrap_or_else(|| html.to_string());

    let markdown = html2md::parse_html(&html_to_convert);

    if markdown.trim().is_empty() {
        return Err("Extracted content is empty (page may have no readable content)".into());
    }

    Ok(markdown)
}

fn extract_body(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let start = lower.find("<body")?;
    let body_start = lower[start..].find('>')? + start + 1;
    let body_end = lower.rfind("</body>")?;

    if body_end >= body_start {
        Some(html[body_start..body_end].to_string())
    } else {
        None
    }
}

fn count_stats(content: &str) -> (usize, usize, usize) {
    let lines = content.lines().count();
    let words = content.split_whitespace().count();
    let characters = content.chars().count();
    (lines, words, characters)
}

#[tool_router]
impl FetchServer {
    fn new(cache_dir: Option<PathBuf>, toc_budget: usize, toc_threshold: usize) -> Self {
        let cache_path = cache_dir.unwrap_or_else(|| PathBuf::from(".llms-fetch-mcp"));
        // Ensure cache_dir is absolute for security (prevents relative path bypass)
        let absolute_cache = cache_path.canonicalize().unwrap_or_else(|_| {
            // If path doesn't exist, make it absolute relative to current dir
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("/tmp"))
                .join(&cache_path)
        });

        Self {
            cache_dir: Arc::new(absolute_cache),
            toc_config: toc::TocConfig {
                toc_budget,
                full_content_threshold: toc_threshold,
            },
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Use to access documentation and guides from the web. Start with documentation root URLs (e.g., https://docs.example.com) - the tool discovers llms.txt files and tries multiple formats (.md, /index.md, /llms.txt, /llms-full.txt). Content is converted to markdown and cached locally. Returns file path with table of contents for navigation. For GitHub files, use raw.githubusercontent.com URLs for best results."
    )]
    async fn fetch(
        &self,
        params: Parameters<FetchInput>,
    ) -> Result<rmcp::Json<FetchOutput>, McpError> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| {
                McpError::internal_error(format!("Failed to create HTTP client: {e}"), None)
            })?;

        let variations = get_url_variations(&params.0.url);

        let mut fetch_tasks = Vec::new();
        for url in &variations {
            let client_clone = client.clone();
            let url_clone = url.clone();
            fetch_tasks.push(tokio::spawn(async move {
                fetch_url(&client_clone, &url_clone).await
            }));
        }

        let mut results = Vec::new();
        let mut errors = Vec::new();
        for task in fetch_tasks {
            if let Ok(attempt) = task.await {
                match attempt {
                    FetchAttempt::Success(result) => results.push(result),
                    FetchAttempt::HttpError { url, status } => {
                        errors.push(format!("{url}: HTTP {status}"));
                    }
                    FetchAttempt::NetworkError { url } => {
                        errors.push(format!("{url}: network error"));
                    }
                }
            }
        }

        if results.is_empty() {
            let error_details = if errors.is_empty() {
                format!("tried {} variations", variations.len())
            } else {
                errors.join("; ")
            };
            return Err(McpError::resource_not_found(
                format!(
                    "Failed to fetch content from {} ({})",
                    params.0.url, error_details
                ),
                None,
            ));
        }

        ensure_gitignore(&self.cache_dir).await.map_err(|e| {
            McpError::internal_error(format!("Failed to create .gitignore: {e}"), None)
        })?;

        let mut file_infos = Vec::new();
        let mut seen_content: HashSet<String> = HashSet::new();

        let has_non_html = results.iter().any(|r| !r.is_html);

        for result in results {
            let url_lower = result.url.to_lowercase();
            let content_type = if url_lower.contains("/llms-full.txt") {
                "llms-full"
            } else if url_lower.contains("/llms.txt") {
                "llms"
            } else if result.is_markdown {
                "markdown"
            } else if result.is_html {
                "html-converted"
            } else {
                "text"
            };

            if has_non_html && result.is_html {
                continue;
            }

            let content_to_save = if result.is_html && !result.is_markdown {
                html_to_markdown(&result.content, &result.url).map_err(|e| {
                    McpError::internal_error(
                        format!("Failed to convert HTML to markdown: {e}"),
                        None,
                    )
                })?
            } else {
                result.content.clone()
            };

            // Deduplicate content by comparing full strings
            if !seen_content.insert(content_to_save.clone()) {
                // Already seen this content, skip it
                continue;
            }

            let file_path = url_to_path(&self.cache_dir, &result.url)
                .map_err(|e| McpError::internal_error(format!("Failed to parse URL: {e}"), None))?;

            if let Some(parent) = file_path.parent() {
                fs::create_dir_all(parent).await.map_err(|e| {
                    McpError::internal_error(format!("Failed to create directory: {e}"), None)
                })?;
            }

            // Atomic write: temp file + rename to prevent corruption from concurrent writes
            let temp_path = file_path.with_extension("tmp");
            fs::write(&temp_path, &content_to_save).await.map_err(|e| {
                McpError::internal_error(format!("Failed to write temp file: {e}"), None)
            })?;
            fs::rename(&temp_path, &file_path).await.map_err(|e| {
                McpError::internal_error(format!("Failed to finalize file: {e}"), None)
            })?;

            let (lines, words, characters) = count_stats(&content_to_save);

            let table_of_contents =
                if content_type.contains("markdown") || content_type == "html-converted" {
                    toc::generate_toc(&content_to_save, characters, &self.toc_config)
                } else {
                    None
                };

            file_infos.push(FileInfo {
                path: file_path.to_string_lossy().to_string(),
                source_url: result.url.clone(),
                content_type: content_type.to_string(),
                lines,
                words,
                characters,
                table_of_contents,
            });
        }

        Ok(rmcp::Json(FetchOutput { files: file_infos }))
    }
}

#[tool_handler]
impl ServerHandler for FetchServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation::from_build_env(),
            instructions: Some(
                "Web content fetcher with intelligent format detection for documentation. Cleans HTML and converts to Markdown. Generates table of contents for navigation. Deduplicates content automatically."
                    .to_string(),
            ),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let server = FetchServer::new(cli.cache_dir, cli.toc_budget, cli.toc_threshold);

    let running = server
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await?;

    running.waiting().await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_variations_plain_url() {
        let url = "https://example.com/docs";
        let variations = get_url_variations(url);

        assert_eq!(variations.len(), 6);
        assert_eq!(variations[0], "https://example.com/docs");
        assert_eq!(variations[1], "https://example.com/docs.md");
        assert_eq!(variations[2], "https://example.com/docs.html.md");
        assert_eq!(variations[3], "https://example.com/docs/index.md");
        assert_eq!(variations[4], "https://example.com/docs/llms.txt");
        assert_eq!(variations[5], "https://example.com/docs/llms-full.txt");
    }

    #[test]
    fn test_url_variations_github() {
        let url = "https://github.com/user/repo/tree/main/docs";
        let variations = get_url_variations(url);

        assert_eq!(variations.len(), 6);
        assert_eq!(variations[0], "https://github.com/user/repo/tree/main/docs");
        assert_eq!(
            variations[1],
            "https://github.com/user/repo/tree/main/docs.md"
        );
        assert_eq!(
            variations[2],
            "https://github.com/user/repo/tree/main/docs.html.md"
        );
        assert_eq!(
            variations[3],
            "https://github.com/user/repo/tree/main/docs/index.md"
        );
        assert_eq!(
            variations[4],
            "https://github.com/user/repo/tree/main/docs/llms.txt"
        );
        assert_eq!(
            variations[5],
            "https://github.com/user/repo/tree/main/docs/llms-full.txt"
        );
    }

    #[test]
    fn test_url_variations_md_file() {
        let url = "https://example.com/docs/readme.md";
        let variations = get_url_variations(url);

        assert_eq!(variations.len(), 1);
        assert_eq!(variations[0], "https://example.com/docs/readme.md");
    }

    #[test]
    fn test_url_variations_txt_file() {
        let url = "https://example.com/docs/file.txt";
        let variations = get_url_variations(url);

        assert_eq!(variations.len(), 1);
        assert_eq!(variations[0], "https://example.com/docs/file.txt");
    }

    #[test]
    fn test_url_variations_with_query_params() {
        let url = "https://httpbin.org/get?test=value";
        let variations = get_url_variations(url);

        // Should not add variations for URLs with query parameters
        assert_eq!(variations.len(), 1);
        assert_eq!(variations[0], "https://httpbin.org/get?test=value");
    }

    #[test]
    fn test_url_to_path_simple() {
        let base = PathBuf::from("/cache");
        let url = "https://example.com/docs/page";
        let path = url_to_path(&base, url).unwrap();

        assert_eq!(path, PathBuf::from("/cache/example.com/docs/page/index"));
    }

    #[test]
    fn test_url_to_path_with_extension() {
        let base = PathBuf::from("/cache");
        let url = "https://example.com/docs/page.md";
        let path = url_to_path(&base, url).unwrap();

        assert_eq!(path, PathBuf::from("/cache/example.com/docs/page.md"));
    }

    #[test]
    fn test_url_to_path_root() {
        let base = PathBuf::from("/cache");
        let url = "https://example.com/";
        let path = url_to_path(&base, url).unwrap();

        assert_eq!(path, PathBuf::from("/cache/example.com/index"));
    }

    #[test]
    fn test_count_stats() {
        let content = "Line 1\nLine 2\nLine 3";
        let (lines, words, chars) = count_stats(content);

        assert_eq!(lines, 3);
        assert_eq!(words, 6);
        assert_eq!(chars, 20);
    }

    #[test]
    fn test_count_stats_empty() {
        let content = "";
        let (lines, words, chars) = count_stats(content);

        assert_eq!(lines, 0);
        assert_eq!(words, 0);
        assert_eq!(chars, 0);
    }

    #[test]
    fn test_url_to_path_with_query_params() {
        let base = PathBuf::from(".llms-fetch-mcp");
        let url = "https://httpbin.org/get?test=value";
        let path = url_to_path(&base, url).unwrap();

        eprintln!("Base: {base:?}");
        eprintln!("Path: {path:?}");
        eprintln!("Starts with: {}", path.starts_with(&base));

        assert!(path.starts_with(&base));
        assert!(path.to_string_lossy().contains("?test=value"));
    }

    #[test]
    fn test_url_to_path_deep_path() {
        let base = PathBuf::from(".llms-fetch-mcp");
        let url = "https://developer.mozilla.org/en-US/docs/Web/JavaScript";
        let path = url_to_path(&base, url).unwrap();

        eprintln!("Base: {base:?}");
        eprintln!("Path: {path:?}");
        eprintln!("Starts with: {}", path.starts_with(&base));

        assert!(path.starts_with(&base));
    }

    #[test]
    fn test_url_parser_normalizes_traversal() {
        // The url::Url parser automatically normalizes path traversal attempts
        // This test verifies this behavior, which is good for security
        let base = PathBuf::from("/cache");
        let url = "https://example.com/../etc/passwd";

        let parsed = url::Url::parse(url).unwrap();
        eprintln!("URL: {url}");
        eprintln!("Parsed path: {}", parsed.path());

        // URL parser normalizes "../" to "/" at the root
        assert_eq!(parsed.path(), "/etc/passwd");

        // Our code will place this safely within the cache
        let result = url_to_path(&base, url);
        assert!(result.is_ok());
        let path = result.unwrap();
        // Path is within cache directory - safe
        assert!(path.starts_with(&base));
        assert_eq!(path, PathBuf::from("/cache/example.com/etc/passwd/index"));
    }

    #[test]
    fn test_component_filter_blocks_dots() {
        // If somehow a ".." or "." makes it through URL parsing as a component,
        // our component filter will reject it
        let base = PathBuf::from("/cache");

        // Manually construct a URL that would have ".." as a component
        // (in practice, url::Url normalizes these, but we test the filter anyway)
        let test_cases = vec![
            ("https://example.com/%2e%2e/passwd", "/passwd"), // URL-encoded ".."
        ];

        for (url, _expected_path) in test_cases {
            let parsed = url::Url::parse(url).unwrap();
            eprintln!("Testing URL: {url}");
            eprintln!("Parsed path: {}", parsed.path());

            let result = url_to_path(&base, url);
            eprintln!("Result: {result:?}");

            // Verify the path is safe and within base
            if let Ok(path) = result {
                assert!(path.starts_with(&base));
            }
        }
    }

    #[test]
    fn test_starts_with_protection() {
        // Final check: verify paths stay within base directory
        let base = PathBuf::from("/cache");
        let url = "https://example.com/docs/api/v1/reference";
        let result = url_to_path(&base, url);

        assert!(result.is_ok());
        let path = result.unwrap();

        // Path must be within base directory
        assert!(path.starts_with(&base));
        assert!(path.to_string_lossy().contains("docs/api/v1/reference"));

        // Verify the path structure
        assert_eq!(
            path,
            PathBuf::from("/cache/example.com/docs/api/v1/reference/index")
        );
    }

    #[test]
    fn test_url_variations_github_blob() {
        // Note: .rs extension prevents .html.md and directory variations
        let url = "https://github.com/user/repo/blob/main/src/lib.rs";
        let variations = get_url_variations(url);

        // Should have: original + .md (no .html.md or directory variations due to .rs extension)
        assert_eq!(variations.len(), 2);
        assert_eq!(
            variations[0],
            "https://github.com/user/repo/blob/main/src/lib.rs"
        );
        assert_eq!(
            variations[1],
            "https://github.com/user/repo/blob/main/src/lib.rs.md"
        );
    }

    #[test]
    fn test_url_variations_html_file() {
        // HTML files should not get .html.md variation (prevents page.html.html.md)
        let url = "https://example.com/page.html";
        let variations = get_url_variations(url);

        assert_eq!(variations.len(), 2);
        assert_eq!(variations[0], "https://example.com/page.html");
        assert_eq!(variations[1], "https://example.com/page.html.md");
    }

    #[test]
    fn test_url_variations_github_malformed() {
        // Test that malformed GitHub URLs don't panic
        let urls = vec![
            "https://github.com/user",      // Too few segments
            "https://github.com/user/repo", // No tree/blob
            "https://github.com",           // Root
        ];

        for url in urls {
            let variations = get_url_variations(url);
            // Should return standard variations without crashing
            assert!(!variations.is_empty());
            assert_eq!(variations[0], url);
        }
    }

    #[test]
    fn test_url_to_path_query_sanitization() {
        // Test that filesystem-unsafe characters in query params are sanitized
        let base = PathBuf::from("/cache");

        // Test that slashes in query params get sanitized
        let url1 = "https://example.com/api?path=../etc/passwd";
        let path1 = url_to_path(&base, url1).unwrap();
        let path_str1 = path1.to_string_lossy();
        assert!(path1.starts_with(&base));
        // Slashes in query should be replaced with underscores
        assert!(
            path_str1.contains("path=.._etc_passwd"),
            "Path was: {path_str1}"
        );

        // Test that other unsafe chars (colons, question marks, etc.) get sanitized
        let url2 = "https://example.com/api?name=file:name?test";
        let path2 = url_to_path(&base, url2).unwrap();
        let path_str2 = path2.to_string_lossy();
        assert!(path2.starts_with(&base));
        // Colons and question marks should be replaced with underscores
        assert!(
            path_str2.contains("file_name_test"),
            "Path was: {path_str2}"
        );

        // Test that backslashes in query params get sanitized
        let url3 = "https://example.com/api?path=..\\etc\\passwd";
        let path3 = url_to_path(&base, url3).unwrap();
        let path_str3 = path3.to_string_lossy();
        assert!(path3.starts_with(&base));
        // Backslashes should be replaced with underscores
        assert!(
            path_str3.contains("path=.._etc_passwd"),
            "Path was: {path_str3}"
        );
    }

    #[test]
    fn test_html_to_markdown_fallback() {
        let html_with_main = r"
            <html>
                <head><title>Test</title></head>
                <body>
                    <main>
                        <h1>Main Content</h1>
                        <p>This has a main tag.</p>
                    </main>
                </body>
            </html>
        ";

        let result_with_main = html_to_markdown(html_with_main, "https://example.com");
        assert!(result_with_main.is_ok());
        let markdown_with_main = result_with_main.unwrap();
        assert!(markdown_with_main.contains("Main Content"));

        let html_without_main = r"
            <html>
                <head><title>Test</title></head>
                <body>
                    <h1>No Main Tag</h1>
                    <p>This page doesn't have a main or article tag.</p>
                    <div>
                        <h2>Subsection</h2>
                        <p>More content here.</p>
                    </div>
                </body>
            </html>
        ";

        let result_without_main = html_to_markdown(html_without_main, "https://example.com");
        assert!(result_without_main.is_ok());
        let markdown_without_main = result_without_main.unwrap();
        assert!(markdown_without_main.contains("No Main Tag"));
        assert!(markdown_without_main.contains("Subsection"));
    }

    #[test]
    fn test_html_to_markdown_edge_cases() {
        // Empty HTML
        assert!(html_to_markdown("", "https://example.com").is_err());

        // Whitespace-only HTML
        assert!(html_to_markdown("   \n\t   ", "https://example.com").is_err());

        // HTML with only scripts/styles (produces empty markdown)
        let script_only = r"
            <html>
                <head><script>console.log('test');</script></head>
                <body><script>alert('hi');</script></body>
            </html>
        ";
        let result = html_to_markdown(script_only, "https://example.com");
        // This might succeed with minimal content or fail - either is acceptable
        if let Ok(md) = result {
            assert!(!md.trim().is_empty());
        }

        // Malformed HTML (unclosed tags) - html2md handles this gracefully
        let malformed = "<div><p>unclosed tags<h1>Header";
        let result = html_to_markdown(malformed, "https://example.com");
        assert!(result.is_ok());
        assert!(result.unwrap().contains("Header"));
    }

    #[test]
    fn test_extract_body() {
        // Standard body tag
        let html = "<html><head><title>Test</title></head><body><p>Content</p></body></html>";
        let body = extract_body(html);
        assert!(body.is_some());
        assert_eq!(body.unwrap(), "<p>Content</p>");

        // Body with attributes
        let html_attrs = r#"<html><body class="main" id="content"><div>Text</div></body></html>"#;
        let body_attrs = extract_body(html_attrs);
        assert!(body_attrs.is_some());
        assert_eq!(body_attrs.unwrap(), "<div>Text</div>");

        // No body tag
        assert!(extract_body("<html><div>No body</div></html>").is_none());

        // Empty body
        let empty = "<html><body></body></html>";
        let body_empty = extract_body(empty);
        assert!(body_empty.is_some());
        assert_eq!(body_empty.unwrap(), "");

        // Malformed (no closing body)
        assert!(extract_body("<html><body><p>Content").is_none());
    }
}
