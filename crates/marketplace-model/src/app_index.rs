use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppIndexForMarket {
    pub apps: Vec<AppSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecation: Option<Deprecation>,
    pub app_id: String,
    pub name: String,
    pub version: String,
    pub package_id: String,
    #[serde(default)]
    pub image_id: String,
    #[serde(default)]
    pub web_link: String,
    #[serde(default)]
    pub code_link: String,
    pub is_open_source: bool,
    #[serde(default)]
    pub categories: Vec<String>,
    pub author: Option<AuthorIdentity>,
    #[serde(default)]
    pub upstream_author: String,
    #[serde(default)]
    pub short_description: String,
    #[serde(default)]
    pub created_at: String,
    pub version_number: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Deprecation {
    #[serde(default)]
    pub replacement_app_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorIdentity {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub keybase_username: String,
    #[serde(default)]
    pub picture: String,
    #[serde(default)]
    pub github_username: String,
    #[serde(default)]
    pub twitter_username: String,
    #[serde(default)]
    pub hackernews_username: String,
    #[serde(default)]
    pub reddit_username: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppDetailsForMarket {
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub change_log: String,
    #[serde(default)]
    pub screenshots: Vec<Screenshot>,
    #[serde(default)]
    pub license: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Screenshot {
    pub image_id: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone)]
pub struct LoadedAppIndex {
    pub root: PathBuf,
    pub index: AppIndexForMarket,
}

impl LoadedAppIndex {
    pub fn load(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let index_path = root.join("apps").join("index.json");
        let index_text = fs::read_to_string(&index_path)
            .with_context(|| format!("failed to read {}", index_path.display()))?;
        let mut index: AppIndexForMarket = serde_json::from_str(&index_text)
            .with_context(|| format!("failed to parse {}", index_path.display()))?;
        index.apps.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self { root, index })
    }

    pub fn load_details(&self, app_id: &str) -> Result<AppDetailsForMarket> {
        let path = self.root.join("apps").join(format!("{app_id}.json"));
        let text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
    }

    pub fn image_url(&self, image_id: &str) -> String {
        format!("/images/{image_id}")
    }
}

impl AppDetailsForMarket {
    pub fn image_ids(&self) -> impl Iterator<Item = &str> {
        self.screenshots
            .iter()
            .map(|screenshot| screenshot.image_id.as_str())
    }
}

impl AppIndexForMarket {
    pub fn image_ids(&self) -> BTreeSet<&str> {
        self.apps
            .iter()
            .filter_map(|app| (!app.image_id.is_empty()).then_some(app.image_id.as_str()))
            .collect()
    }
}
