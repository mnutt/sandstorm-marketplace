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
use std::sync::Arc;

use anyhow::{Context, Result};
use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use clap::{Parser, Subcommand};
use marketplace_model::app_index::{AppDetailsForMarket, AppSummary, LoadedAppIndex};
use serde::Deserialize;
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

#[derive(Clone)]
struct AppState {
    index: Arc<LoadedAppIndex>,
    package_base_url: String,
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

#[derive(Debug, Default, Deserialize)]
struct HomeParams {
    q: Option<String>,
}

#[tokio::main]
pub async fn run() -> Result<()> {
    init_tracing();
    if let Some(Command::Publish { source, output }) = Cli::parse().command {
        return publish::publish(&source, &output);
    }

    let fixture_root = fixture_root();
    let loaded = LoadedAppIndex::load(&fixture_root).with_context(|| {
        format!(
            "failed to load fixture data from {}",
            fixture_root.display()
        )
    })?;

    let state = AppState {
        index: Arc::new(loaded),
        package_base_url: package_base_url(),
    };

    let static_root = state.index.root.clone();
    let app = Router::new()
        .route("/", get(home))
        .route("/genre/{genre}", get(genre))
        .route("/genre/{genre}/", get(genre))
        .route("/app/{app_id}", get(app_detail))
        .route("/app/{app_id}/", get(app_detail))
        .route("/assets/{name}", get(assets::serve))
        .route(
            "/marketplace.js",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "text/javascript")],
                    include_str!("../static/marketplace.js"),
                )
            }),
        )
        .nest_service("/images", ServeDir::new(static_root.join("images")))
        .nest_service("/static", ServeDir::new(static_root))
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr: SocketAddr = env::var("MARKETPLACE_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:9938".to_string())
        .parse()
        .context("failed to parse MARKETPLACE_LISTEN")?;

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("listening on http://{}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn home(
    Query(params): Query<HomeParams>,
    State(state): State<AppState>,
) -> Result<Html<String>, AppError> {
    render_home(params, state, None)
}

async fn genre(
    Path(genre): Path<String>,
    Query(params): Query<HomeParams>,
    State(state): State<AppState>,
) -> Result<Html<String>, AppError> {
    if !state
        .index
        .index
        .apps
        .iter()
        .any(|app| app.categories.contains(&genre))
    {
        return Err(AppError::not_found(format!("unknown category: {genre}")));
    }
    render_home(params, state, Some(genre))
}

fn render_home(
    params: HomeParams,
    state: AppState,
    current_tag: Option<String>,
) -> Result<Html<String>, AppError> {
    let search = params.q.unwrap_or_default();
    let search_lower = search.to_ascii_lowercase();
    let tags = build_tag_chips(&state.index, current_tag.as_deref(), &search);
    let apps = state
        .index
        .index
        .apps
        .iter()
        .filter(|app| match current_tag.as_deref() {
            Some(tag) => app.categories.iter().any(|category| category == tag),
            None => true,
        })
        .filter(|app| {
            if search_lower.is_empty() {
                return true;
            }

            let mut haystacks = vec![
                app.name.as_str(),
                app.short_description.as_str(),
                app.upstream_author.as_str(),
                app.version.as_str(),
            ];

            if let Some(author) = &app.author {
                haystacks.push(author.name.as_str());
                haystacks.push(author.github_username.as_str());
                haystacks.push(author.keybase_username.as_str());
            }

            haystacks.extend(app.categories.iter().map(String::as_str));

            haystacks
                .into_iter()
                .any(|value| value.to_ascii_lowercase().contains(&search_lower))
        })
        .map(|app| listing(app, &state.package_base_url))
        .collect();
    let template = HomeTemplate {
        show_welcome: current_tag.is_none() && search.is_empty(),
        search_action: home_href(current_tag.as_deref(), ""),
        tags,
        apps,
        search,
    };
    render(template)
}

async fn app_detail(
    Path(app_id): Path<String>,
    State(state): State<AppState>,
) -> Result<Html<String>, AppError> {
    let app = state
        .index
        .app(&app_id)
        .ok_or_else(|| AppError::not_found(format!("unknown app id: {app_id}")))?;
    Ok(Html(render_app(
        &state.index,
        app,
        &state.package_base_url,
    )?))
}

fn render_app(index: &LoadedAppIndex, app: &AppSummary, package_base_url: &str) -> Result<String> {
    let details = index.load_details(&app.app_id)?;
    let template = AppTemplate {
        tags: build_tag_chips(index, None, ""),
        app,
        details: &details,
        icon_url: index.image_url(&app.image_id),
        package_url: join_url(package_base_url, &app.package_id),
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
    };
    Ok(template.render()?)
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

fn render(template: impl Template) -> Result<Html<String>, AppError> {
    Ok(Html(template.render()?))
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

fn package_base_url() -> String {
    env::var("MARKETPLACE_PACKAGE_BASE_URL").unwrap_or_else(|_| "/static/packages".to_string())
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

#[derive(Debug)]
struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    fn not_found(message: String) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message,
        }
    }
}

impl From<anyhow::Error> for AppError {
    fn from(error: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: error.to_string(),
        }
    }
}

impl From<askama::Error> for AppError {
    fn from(error: askama::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: error.to_string(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
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

    #[tokio::test]
    async fn category_route_filters_apps_and_rejects_unknown_categories() {
        let state = AppState {
            index: Arc::new(
                LoadedAppIndex::load(
                    &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/app-index"),
                )
                .unwrap(),
            ),
            package_base_url: "/packages".to_string(),
        };
        let Html(html) = genre(
            Path("Collaboration".into()),
            Query(HomeParams::default()),
            State(state.clone()),
        )
        .await
        .unwrap();
        assert!(html.contains("<h2 title=\"Etherpad\">"));
        assert!(!html.contains("<h2 title=\"Wekan\">"));
        assert!(html.contains("id=\"welcome\" hidden"));
        assert!(
            genre(
                Path("Missing".into()),
                Query(HomeParams::default()),
                State(state)
            )
            .await
            .is_err()
        );
    }
}
