//! Private, indexed per-app history, committed with registry changes in SQLite.
use crate::{database::Change, registry::Registry};
use anyhow::Result;
use anyhow::ensure;
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Actor {
    pub kind: String,
    pub identity_id: Option<String>,
    pub display_name: String,
    pub signing_key: Option<String>,
}
impl Default for Actor {
    fn default() -> Self {
        Self {
            kind: "system".into(),
            identity_id: None,
            display_name: "Marketplace system".into(),
            signing_key: None,
        }
    }
}
impl Actor {
    pub fn session(user: crate::identity_capnp::user_info::Reader<'_>, api: bool) -> Result<Self> {
        let id = user.get_identity_id()?;
        ensure!(
            id.is_empty() || id.len() == 32,
            "invalid Sandstorm identity ID"
        );
        Ok(Self {
            kind: if id.is_empty() {
                "unidentifiedSession"
            } else {
                "sandstormIdentity"
            }
            .into(),
            identity_id: (!id.is_empty()).then(|| hex::encode(id)),
            display_name: if id.is_empty() {
                if api {
                    "Unidentified API session"
                } else {
                    "Unidentified web session"
                }
                .into()
            } else {
                user.get_display_name()?
                    .get_default_text()?
                    .to_str()?
                    .chars()
                    .take(256)
                    .collect()
            },
            signing_key: None,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub number: u64,
    pub timestamp: String,
    pub app_id: String,
    pub package_id: Option<String>,
    pub version: Option<u32>,
    pub action: String,
    pub actor: Actor,
    pub details: Value,
}
impl Event {
    pub fn new(
        app: &str,
        package: Option<(&str, u32)>,
        action: &str,
        actor: Actor,
        details: Value,
    ) -> Self {
        Self {
            number: 0,
            timestamp: chrono::Utc::now().to_rfc3339(),
            app_id: app.into(),
            package_id: package.map(|p| p.0.into()),
            version: package.map(|p| p.1),
            action: action.into(),
            actor,
            details,
        }
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub events: Vec<Event>,
    pub before: Option<u64>,
}

impl Registry {
    pub fn with_actor(&self, actor: Actor) -> Self {
        let mut registry = self.clone();
        registry.actor = actor;
        registry
    }
    pub(crate) fn audit_commit(&self, changes: Vec<Change>, events: Vec<Event>) -> Result<()> {
        self.database.commit(changes, events)
    }
    pub fn audit_page(&self, app: &str, before: Option<u64>) -> Result<Page> {
        self.database.page(app, before)
    }
}
