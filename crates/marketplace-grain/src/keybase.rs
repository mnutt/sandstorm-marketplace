use crate::{
    api_session_capnp::api_session,
    app_index_capnp::keybase_identity,
    grain_capnp::session_context,
    hack_session_capnp::hack_session_context,
    powerbox_capnp::powerbox_descriptor,
    registry::Registry,
    rpc::{PlatformSlot, cast, error},
    util_capnp::byte_stream,
    web_session_capnp::web_session as web,
};
use anyhow::{Context, Result, ensure};
use base64::Engine;
use capnp::{capability::Promise, message::Builder, serialize_packed, traits::HasTypeId};
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc};

pub fn descriptor() -> Result<String> {
    let mut message = Builder::new_default();
    let mut tag = message
        .init_root::<powerbox_descriptor::Builder>()
        .init_tags(1)
        .get(0);
    tag.set_id(api_session::Client::TYPE_ID);
    tag.init_value()
        .init_as::<api_session::powerbox_tag::Builder>()
        .set_canonical_url("https://keybase.io/_/api/1.0");
    let mut bytes = Vec::new();
    serialize_packed::write_message(&mut bytes, &message)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}
pub async fn claim(
    registry: &Registry,
    context: &hack_session_context::Client,
    platform: &PlatformSlot,
    token: &str,
) -> Result<()> {
    let context: session_context::Client = cast(context.clone());
    let mut request = context.claim_request_request();
    request.get().set_request_token(token);
    let claimed = request.send().promise.await?;
    let platform = platform
        .borrow()
        .clone()
        .context("platform not connected")?;
    let mut save = platform.save_request();
    save.get()
        .get_cap()
        .set_as::<capnp::any_pointer::Owned>(claimed.get()?.get_cap())?;
    save.get()
        .init_label()
        .set_default_text("Keybase API endpoint");
    let saved = save.send().promise.await?;
    registry.database.transaction(|db| {
        db.execute(
            "INSERT OR REPLACE INTO capabilities VALUES('keybase',?)",
            [saved.get()?.get_token()?],
        )?;
        Ok(())
    })
}
pub async fn lookup(
    registry: &Registry,
    platform: &PlatformSlot,
    fingerprint: &str,
) -> Result<Value> {
    ensure!(
        (32..=64).contains(&fingerprint.len())
            && fingerprint.bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid fingerprint"
    );
    let platform = platform
        .borrow()
        .clone()
        .context("platform not connected")?;
    let mut restore = platform.restore_request();
    restore.get().set_token(
        &registry
            .database
            .read(|db| {
                Ok(db.query_row(
                    "SELECT token FROM capabilities WHERE name='keybase'",
                    [],
                    |r| r.get::<_, Vec<u8>>(0),
                )?)
            })
            .context("Keybase is not connected; use Connect Keybase before looking up an author")?,
    );
    let restored = restore.send().promise.await?;
    let endpoint: web::Client = restored.get()?.get_cap().get_as()?;
    let bytes = Rc::new(RefCell::new(Vec::new()));
    let (sender, receiver) = futures::channel::oneshot::channel();
    let stream: byte_stream::Client = capnp_rpc::new_client(Collector {
        bytes: bytes.clone(),
        sender: Some(sender),
    });
    let mut request = endpoint.get_request();
    request.get().set_path(
        format!(
            "user/lookup.json?key_fingerprint={fingerprint}&fields=pictures,profile,proofs_summary"
        )
        .as_str(),
    );
    request.get().init_context().set_response_stream(stream);
    let response = request.send().promise.await?;
    let content = match response.get()?.which()? {
        web::response::Content(content) => content,
        _ => anyhow::bail!("Keybase request failed"),
    };
    let value: Value = match content.get_body().which()? {
        web::response::content::body::Bytes(data) => serde_json::from_slice(data?)?,
        web::response::content::body::Stream(_) => {
            receiver.await?;
            serde_json::from_slice(&bytes.borrow())?
        }
    };
    ensure!(
        value["status"]["code"] == 0 && value["status"]["name"] == "OK",
        "Keybase lookup failed"
    );
    let profile = value["them"]
        .as_array()
        .and_then(|v| v.first())
        .context("Keybase identity not found")?;
    let text = |pointer: &str| {
        profile
            .pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or("")
    };
    let mut result = json!({"keybaseHandle": text("/basics/username"), "name": text("/profile/full_name"), "picture": text("/pictures/primary/url")});
    let mut message = Builder::new_default();
    let mut identity = message.init_root::<keybase_identity::Builder>();
    identity.set_keybase_handle(text("/basics/username"));
    identity.set_name(text("/profile/full_name"));
    identity.set_picture(text("/pictures/primary/url"));
    for (kind, field) in [
        ("generic_web_site", "websites"),
        ("github", "githubHandles"),
        ("twitter", "twitterHandles"),
        ("hackernews", "hackernewsHandles"),
        ("reddit", "redditHandles"),
    ] {
        let handles: Vec<&str> = profile
            .pointer("/proofs_summary/all")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|proof| proof["proof_type"] == kind)
            .filter_map(|proof| proof["nametag"].as_str())
            .collect();
        let mut list = match kind {
            "generic_web_site" => identity.reborrow().init_websites(handles.len() as u32),
            "github" => identity
                .reborrow()
                .init_github_handles(handles.len() as u32),
            "twitter" => identity
                .reborrow()
                .init_twitter_handles(handles.len() as u32),
            "hackernews" => identity
                .reborrow()
                .init_hackernews_handles(handles.len() as u32),
            _ => identity
                .reborrow()
                .init_reddit_handles(handles.len() as u32),
        };
        for (i, handle) in handles.iter().enumerate() {
            list.set(i as u32, *handle);
        }
        result[field] = json!(handles);
    }
    registry.database.transaction(|db| {
        db.execute(
            "INSERT OR REPLACE INTO author_profiles VALUES(?,?)",
            rusqlite::params![fingerprint, crate::database::encode(&message)?],
        )?;
        db.execute(
            "UPDATE registry_state SET state_revision=state_revision+1",
            [],
        )?;
        Ok(())
    })?;
    registry.publish()?;
    Ok(result)
}
struct Collector {
    bytes: Rc<RefCell<Vec<u8>>>,
    sender: Option<futures::channel::oneshot::Sender<()>>,
}
impl byte_stream::Server for Collector {
    fn write(&mut self, params: byte_stream::WriteParams) -> Promise<(), capnp::Error> {
        let result: Result<()> = (|| {
            let params = params.get()?;
            let data = params.get_data()?;
            let mut bytes = self.bytes.borrow_mut();
            ensure!(
                bytes.len() + data.len() <= 4 * 1024 * 1024,
                "Keybase response too large"
            );
            bytes.extend_from_slice(data);
            Ok(())
        })();
        Promise::from_future(async { result.map_err(error) })
    }
    fn done(
        &mut self,
        _: byte_stream::DoneParams,
        _: byte_stream::DoneResults,
    ) -> Promise<(), capnp::Error> {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(());
        }
        Promise::ok(())
    }
}
