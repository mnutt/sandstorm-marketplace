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

fn render_app(index: &LoadedAppIndex, app: &AppSummary, package_base_url: &str) -> Result<String> {
    let details = index.load_details(&app.app_id)?;
    let mut template = app_template(app, &details);
    template.tags = build_tag_chips(index, None, "");
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
    template.home_href = format!("../#app/{}", app.app_id);
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
            .map(|author| author.name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "Unknown".to_string()),
        social_links: build_social_links(app),
        report_issue_url: report_issue_url(app),
        website_url: safe_web_url(&app.web_link),
        source_url: safe_web_url(&app.code_link),
        description_html: render_description_html(&details.description),
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
    for app in &index.index.apps {
        for category in &app.categories {
            *counts.entry(category.clone()).or_default() += 1;
        }
    }

    let mut tags = Vec::with_capacity(counts.len() + 1);
    tags.push(TagChip {
        name: "All".to_string(),
        count: index.index.apps.len(),
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

fn render_description_html(input: &str) -> String {
    let mut html = String::new();
    let mut paragraph = Vec::new();

    let flush_paragraph = |paragraph: &mut Vec<String>, html: &mut String| {
        if !paragraph.is_empty() {
            let text = paragraph.join(" ");
            html.push_str("<p>");
            html.push_str(&escape_html(&text));
            html.push_str("</p>");
            paragraph.clear();
        }
    };

    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            flush_paragraph(&mut paragraph, &mut html);
            continue;
        }

        if let Some(heading) = trimmed.strip_prefix("# ") {
            flush_paragraph(&mut paragraph, &mut html);
            html.push_str("<h3>");
            html.push_str(&escape_html(heading));
            html.push_str("</h3>");
            continue;
        }

        if let Some(heading) = trimmed.strip_prefix("## ") {
            flush_paragraph(&mut paragraph, &mut html);
            html.push_str("<h4>");
            html.push_str(&escape_html(heading));
            html.push_str("</h4>");
            continue;
        }

        paragraph.push(trimmed.to_string());
    }

    flush_paragraph(&mut paragraph, &mut html);
    html
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
