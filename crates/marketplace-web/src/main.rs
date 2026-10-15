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
use marketplace_model::app_index::{AppDetailsForMarket, AppSummary, LoadedAppIndex};
use serde::Deserialize;
use tower_http::compression::CompressionLayer;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
struct AppState {
    index: Arc<LoadedAppIndex>,
    package_base_url: String,
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomeTemplate<'a> {
    tags: Vec<TagChip>,
    apps: Vec<ListingApp<'a>>,
    search: String,
}

#[derive(Template)]
#[template(path = "app.html")]
struct AppTemplate<'a> {
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
    description_html: String,
}

struct ListingApp<'a> {
    app: &'a AppSummary,
    package_url: String,
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
    tag: Option<String>,
    q: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

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
        .route("/app/{app_id}", get(app_detail))
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
    let current_tag = params.tag.filter(|tag| !tag.is_empty());
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
        .map(|app| ListingApp {
            app,
            package_url: join_url(&state.package_base_url, &app.package_id),
        })
        .collect();
    let template = HomeTemplate { tags, apps, search };
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
    let details = state.index.load_details(&app_id)?;
    let template = AppTemplate {
        app,
        details: &details,
        icon_url: state.index.image_url(&app.image_id),
        package_url: join_url(&state.package_base_url, &app.package_id),
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
        description_html: render_description_html(&details.description),
    };
    render(template)
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
    let mut parts = Vec::new();
    if let Some(tag) = tag {
        parts.push(format!("tag={}", encode_query_value(tag)));
    }
    if !search.is_empty() {
        parts.push(format!("q={}", encode_query_value(search)));
    }
    if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/?{}", parts.join("&"))
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

fn report_issue_url(app: &AppSummary) -> Option<String> {
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
