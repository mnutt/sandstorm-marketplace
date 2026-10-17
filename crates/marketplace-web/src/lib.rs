mod assets;
pub mod publish;

/// Embedded marketplace branding and styles, also served by the private admin UI.
/// Only exact names in the static asset catalog are accepted.
pub fn static_asset(name: &str) -> Option<(&'static str, &'static [u8])> {
    assets::ASSETS
        .iter()
        .find(|asset| asset.name == name)
        .map(|asset| (asset.content_type, asset.bytes))
}

use std::collections::BTreeMap;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use askama::Template;
use axum::Router;
use clap::{Parser, Subcommand};
use marketplace_model::app_index::{AppDetailsForMarket, AppSummary, LoadedAppIndex};
use tower_http::compression::CompressionLayer;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(about = "Sandstorm marketplace preview and static publisher")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a complete static marketplace, then atomically replace output.
    Publish {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomeTemplate<'a> {
    show_welcome: bool,
    experimental: bool,
    search_action: String,
    tags: Vec<TagChip>,
    apps: Vec<ListingApp<'a>>,
    search: String,
}

#[derive(Template)]
#[template(path = "app.html")]
struct AppTemplate<'a> {
    tags: Vec<TagChip>,
    app: &'a AppSummary,
    details: &'a AppDetailsForMarket,
    icon_url: String,
    screenshots: Vec<ListingScreenshot>,
    preview: bool,
    experimental: bool,
    public_download: bool,
    root: &'static str,
    package_url: String,
    home_href: String,
    categories: String,
    updated_at: String,
    author_name: String,
    social_links: Vec<SocialLink>,
    report_issue_url: Option<String>,
    website_url: String,
    source_url: String,
    description_html: String,
    changelog_html: String,
    changelog_excerpt_html: Option<String>,
}

struct ListingScreenshot {
    url: String,
    width: u32,
    height: u32,
}

struct ListingApp<'a> {
    app: &'a AppSummary,
    package_url: String,
    categories_json: String,
    search_text: String,
}

struct SocialLink {
    label: String,
    href: String,
}

struct TagChip {
    name: String,
    count: usize,
    href: String,
    selected: bool,
}

#[tokio::main]
pub async fn run() -> Result<()> {
    init_tracing();
    if let Some(Command::Publish { source, output }) = Cli::parse().command {
        return publish::publish(&source, &output);
    }

    // Preview the exact static output used by the grain. The temporary tree lives
    // until the server exits; remote mirrors may link to upstream packages.
    let preview = tempfile::tempdir()?;
    let output = preview.path().join("public");
    let package_base = env::var("MARKETPLACE_PACKAGE_BASE_URL").ok();
    publish::publish_with_package_base(&fixture_root(), &output, package_base.as_deref())?;
    let app = Router::new()
        .fallback_service(ServeDir::new(output))
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http());

    let addr: SocketAddr = env::var("MARKETPLACE_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:9938".to_string())
        .parse()
        .context("failed to parse MARKETPLACE_LISTEN")?;

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("listening on http://{}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}

fn render_app(
    index: &LoadedAppIndex,
    app: &AppSummary,
    details: &AppDetailsForMarket,
    package_base_url: &str,
    experimental: bool,
) -> Result<String> {
    let mut template = app_template(app, details);
    template.tags = build_tag_chips(index, None, "");
    template.experimental = experimental;
    template.package_url = join_url(package_base_url, &app.package_id);
    Ok(template.render()?)
}

/// Render a submitted release using the public listing template, with private
/// downloads and embedded images. No files are published by this operation.
pub fn render_preview(
    app: &AppSummary,
    details: &AppDetailsForMarket,
    images: &BTreeMap<String, String>,
    public_download: bool,
) -> Result<String> {
    let mut template = app_template(app, details);
    template.preview = true;
    template.public_download = public_download;
    template.root = "../";
    template.home_href = format!("../{}/", app.app_id);
    template.package_url = format!("../download/{}", app.package_id);
    template.icon_url = images.get(&app.image_id).cloned().unwrap_or_default();
    for (shot, source) in template.screenshots.iter_mut().zip(&details.screenshots) {
        shot.url = images.get(&source.image_id).cloned().unwrap_or_default();
    }
    if app.created_at.is_empty() {
        template.updated_at = "Not published yet".into();
    }
    Ok(template.render()?)
}

fn app_template<'a>(app: &'a AppSummary, details: &'a AppDetailsForMarket) -> AppTemplate<'a> {
    AppTemplate {
        tags: Vec::new(),
        app,
        details,
        icon_url: format!("/images/{}", app.image_id),
        screenshots: details
            .screenshots
            .iter()
            .map(|shot| ListingScreenshot {
                url: format!("/images/{}", shot.image_id),
                width: shot.width,
                height: shot.height,
            })
            .collect(),
        preview: false,
        experimental: false,
        public_download: true,
        root: "/",
        package_url: format!("/packages/{}", app.package_id),
        home_href: "/".to_string(),
        categories: if app.categories.is_empty() {
            "Other".to_string()
        } else {
            app.categories.join(", ")
        },
        updated_at: format_display_date(&app.created_at),
        author_name: app
            .author
            .as_ref()
            .map(|author| {
                if author.name.trim().is_empty() {
                    author.keybase_username.trim().to_string()
                } else {
                    author.name.trim().to_string()
                }
            })
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "Unknown".to_string()),
        social_links: build_social_links(app),
        report_issue_url: report_issue_url(app),
        website_url: safe_web_url(&app.web_link),
        source_url: safe_web_url(&app.code_link),
        description_html: render_markdown(&details.description),
        changelog_html: render_markdown(&details.change_log),
        changelog_excerpt_html: changelog_excerpt(&details.change_log).map(render_markdown),
    }
}

fn listing<'a>(app: &'a AppSummary, package_base_url: &str) -> ListingApp<'a> {
    let author = app.author.as_ref();
    ListingApp {
        app,
        package_url: join_url(package_base_url, &app.package_id),
        categories_json: serde_json::to_string(&app.categories).expect("string serialization"),
        search_text: format!(
            "{} {} {} {} {} {} {} {}",
            app.name,
            app.short_description,
            app.upstream_author,
            app.version,
            app.categories.join(" "),
            author.map(|a| a.name.as_str()).unwrap_or_default(),
            author
                .map(|a| a.github_username.as_str())
                .unwrap_or_default(),
            author
                .map(|a| a.keybase_username.as_str())
                .unwrap_or_default()
        ),
    }
}

fn fixture_root() -> PathBuf {
    if let Ok(path) = env::var("MARKETPLACE_FIXTURE_ROOT") {
        return PathBuf::from(path);
    }

    let mirrored = PathBuf::from("data/app-index");
    if mirrored.exists() {
        mirrored
    } else {
        PathBuf::from("fixtures/app-index")
    }
}

fn join_url(base: &str, tail: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{tail}")
    } else {
        format!("{base}/{tail}")
    }
}

fn build_tag_chips(
    index: &LoadedAppIndex,
    current_tag: Option<&str>,
    search: &str,
) -> Vec<TagChip> {
    let mut counts = BTreeMap::<String, usize>::new();
    for app in index
        .index
        .apps
        .iter()
        .filter(|app| app.deprecation.is_none())
    {
        for category in &app.categories {
            *counts.entry(category.clone()).or_default() += 1;
        }
    }

    let mut tags = Vec::with_capacity(counts.len() + 1);
    tags.push(TagChip {
        name: "All".to_string(),
        count: index
            .index
            .apps
            .iter()
            .filter(|app| app.deprecation.is_none())
            .count(),
        href: home_href(None, search),
        selected: current_tag.is_none(),
    });

    for (name, count) in counts {
        tags.push(TagChip {
            href: home_href(Some(&name), search),
            selected: current_tag == Some(name.as_str()),
            name,
            count,
        });
    }

    tags
}

fn home_href(tag: Option<&str>, search: &str) -> String {
    let path = tag
        .map(|tag| format!("/genre/{}", encode_query_value(tag).replace('+', "%20")))
        .unwrap_or_else(|| "/".to_string());
    if search.is_empty() {
        path
    } else {
        format!("{path}?q={}", encode_query_value(search))
    }
}

fn encode_query_value(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(char::from(byte));
            }
            b' ' => encoded.push('+'),
            _ => {
                encoded.push('%');
                encoded.push_str(&format!("{byte:02X}"));
            }
        }
    }
    encoded
}

fn format_display_date(value: &str) -> String {
    let mut parts = value.split('T').next().unwrap_or(value).split('-');
    let year = parts.next().unwrap_or_default();
    let month = parts.next().unwrap_or_default();
    let day = parts.next().unwrap_or_default();
    let month_name = match month {
        "01" => "January",
        "02" => "February",
        "03" => "March",
        "04" => "April",
        "05" => "May",
        "06" => "June",
        "07" => "July",
        "08" => "August",
        "09" => "September",
        "10" => "October",
        "11" => "November",
        "12" => "December",
        _ => return value.to_string(),
    };
    format!("{month_name} {}, {year}", day.trim_start_matches('0'))
}

fn build_social_links(app: &AppSummary) -> Vec<SocialLink> {
    let mut links = Vec::new();
    if let Some(author) = &app.author {
        if !author.github_username.is_empty() {
            links.push(SocialLink {
                label: "GitHub".to_string(),
                href: format!("https://github.com/{}", author.github_username),
            });
        }
        if !author.twitter_username.is_empty() {
            links.push(SocialLink {
                label: "Twitter".to_string(),
                href: format!("https://twitter.com/{}", author.twitter_username),
            });
        }
        if !author.keybase_username.is_empty() {
            links.push(SocialLink {
                label: "Keybase".to_string(),
                href: format!("https://keybase.io/{}", author.keybase_username),
            });
        }
    }
    links
}

fn safe_web_url(value: &str) -> String {
    let value = value.trim();
    if value.starts_with("https://") || value.starts_with("http://") {
        value.to_string()
    } else {
        String::new()
    }
}

fn report_issue_url(app: &AppSummary) -> Option<String> {
    if safe_web_url(&app.code_link).is_empty() {
        return None;
    }
    if app.code_link.contains("github.com") {
        Some(format!("{}/issues", app.code_link.trim_end_matches('/')))
    } else if app.code_link.contains("gitlab.com") {
        Some(format!("{}/-/issues", app.code_link.trim_end_matches('/')))
    } else {
        None
    }
}

// Bound both line count and characters: some packages use very long single-line paragraphs.
fn changelog_excerpt(input: &str) -> Option<&str> {
    let input = input.trim();
    let mut lines = 0;
    for (characters, (offset, character)) in input.char_indices().enumerate() {
        if characters >= 1200 || lines >= 12 {
            return Some(input[..offset].trim_end());
        }
        if character == '\n' {
            lines += 1;
        }
    }
    None
}

fn render_markdown(input: &str) -> String {
    use pulldown_cmark::{Event, HeadingLevel, Parser, Tag};
    // Limit parser work and rendered DOM size even for unusually large package metadata.
    const MAX_SOURCE_BYTES: usize = 512 * 1024;
    let mut end = input.len().min(MAX_SOURCE_BYTES);
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    let truncated = end < input.len();
    let input = &input[..end];
    let mut output = String::new();
    let mut endings = Vec::new();
    // Intentionally do not use the library's HTML renderer. Only these fixed tags
    // and one validated, escaped href can reach the template's `safe` insertion.
    // Optional Markdown extensions, author HTML/attributes and images are disabled.
    for event in Parser::new(input.trim()) {
        match event {
            Event::Start(tag) => {
                if endings.len() >= 64 {
                    endings.push("");
                    continue;
                }
                let (open, close) = match tag {
                    Tag::Paragraph => ("<p>", "</p>"),
                    Tag::Heading { level, .. } => match level {
                        HeadingLevel::H1 => ("<h3>", "</h3>"),
                        HeadingLevel::H2 => ("<h4>", "</h4>"),
                        HeadingLevel::H3 => ("<h5>", "</h5>"),
                        _ => ("<h6>", "</h6>"),
                    },
                    Tag::List(Some(_)) => ("<ol>", "</ol>"),
                    Tag::List(None) => ("<ul>", "</ul>"),
                    Tag::Item => ("<li>", "</li>"),
                    Tag::Emphasis => ("<em>", "</em>"),
                    Tag::Strong => ("<strong>", "</strong>"),
                    Tag::BlockQuote(_) => ("<blockquote>", "</blockquote>"),
                    Tag::CodeBlock(_) => ("<pre><code>", "</code></pre>"),
                    Tag::Link { dest_url, .. } => {
                        let url = safe_web_url(&dest_url);
                        if url.is_empty() || url.chars().any(char::is_control) {
                            ("", "")
                        } else {
                            output.push_str("<a href=\"");
                            output.push_str(&escape_html(&url));
                            output.push_str("\" rel=\"nofollow noreferrer noopener\">");
                            ("", "</a>")
                        }
                    }
                    _ => ("", ""),
                };
                output.push_str(open);
                endings.push(close);
            }
            Event::End(_) => output.push_str(endings.pop().unwrap_or_default()),
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                output.push_str(&escape_html(&text));
            }
            Event::Code(text) => {
                output.push_str("<code>");
                output.push_str(&escape_html(&text));
                output.push_str("</code>");
            }
            Event::SoftBreak => output.push('\n'),
            Event::HardBreak => output.push_str("<br>"),
            Event::Rule => output.push_str("<hr>"),
            _ => {}
        }
    }
    if truncated {
        output.push_str("<p>This content is too long to display in full.</p>");
    }
    output
}

fn escape_html(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "marketplace_web=info,tower_http=info".into());
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[cfg(test)]
mod route_tests {
    use super::*;

    #[test]
    fn changelog_excerpts_are_bounded_and_unicode_safe() {
        assert!(changelog_excerpt("").is_none());
        assert!(changelog_excerpt("# 1.0\n\n- Short change").is_none());
        let long = "é".repeat(1201);
        assert_eq!(changelog_excerpt(&long).unwrap().chars().count(), 1200);
        let lines = (0..20)
            .map(|n| format!("- Change {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(changelog_excerpt(&lines).unwrap().lines().count(), 12);
    }

    #[test]
    fn empty_rendered_excerpt_keeps_long_changelog_collapsed() {
        let app: AppSummary = serde_json::from_value(serde_json::json!({
            "appId": "test", "name": "Test", "version": "1.0", "versionNumber": 1,
            "packageId": "test", "isOpenSource": true
        }))
        .unwrap();
        let details: AppDetailsForMarket = serde_json::from_value(serde_json::json!({
            "changeLog": format!("{}\nChanges below the definitions.", "[ref]: https://example.org\n".repeat(12))
        }))
        .unwrap();
        let template = app_template(&app, &details);
        assert_eq!(template.changelog_excerpt_html.as_deref(), Some(""));
        let html = template.render().unwrap();
        assert!(html.contains("<details class=\"changelog-full\">"));
        assert!(html.contains("Changes below the definitions."));
    }

    #[test]
    fn changelog_markdown_formats_changes_without_active_content() {
        let html = render_markdown(
            "# v2\n\n- **Fixed** sync\n\n## v1\n\n[Notes](https://example.org)\n\n<script>alert(1)</script>\n\n[bad](javascript:alert(1))\n\n![tracker](https://example.org/track.png)",
        );
        assert!(html.contains("<h3>v2</h3>"));
        assert!(html.contains("<h4>v1</h4>"));
        assert!(html.contains("<li><strong>Fixed</strong> sync</li>"));
        assert!(html.contains("href=\"https://example.org\""));
        assert!(!html.contains("<script"));
        assert!(!html.contains("javascript:"));
        assert!(!html.contains("<img"));
    }

    #[test]
    fn oversized_changelogs_are_bounded_without_splitting_unicode() {
        let html = render_markdown(&"é".repeat(400_000));
        assert!(html.contains("too long to display in full"));
        assert!(html.len() < 513 * 1024);
        let html = render_markdown(&format!("{}text", "> ".repeat(1000)));
        assert!(html.matches("<blockquote>").count() <= 64);
    }

    #[test]
    fn category_links_encode_path_segments_and_preserve_search() {
        assert_eq!(home_href(Some("Games"), ""), "/genre/Games");
        assert_eq!(
            home_href(Some("Office Tools"), "a+b"),
            "/genre/Office%20Tools?q=a%2Bb"
        );
        assert_eq!(home_href(None, "editor"), "/?q=editor");
    }
}

pub fn marketplace_script() -> &'static [u8] {
    include_bytes!("../static/marketplace.js")
}
