mod common;

use anyhow::Result;
use common::{post, session};
use marketplace_grain::{
    grain_capnp::ui_view, registry::Registry, rpc::View, web_session_capnp::web_session as web,
};
use std::{cell::RefCell, rc::Rc};

async fn snapshot(client: &web::Client) -> Result<serde_json::Value> {
    let mut request = client.get_request();
    request.get().set_path("admin");
    let response = request.send().promise.await?;
    match response.get()?.which()? {
        web::response::Content(content) => match content.get_body().which()? {
            web::response::content::body::Bytes(bytes) => Ok(serde_json::from_slice(bytes?)?),
            _ => anyhow::bail!("expected JSON bytes"),
        },
        _ => anyhow::bail!("expected admin snapshot"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn saved_keybase_connection_is_reported_only_to_approvers() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let dir = tempfile::tempdir()?;
            let registry = Registry::new(dir.path().into())?;
            let view: ui_view::Client = capnp_rpc::new_client(View {
                registry,
                platform: Rc::new(RefCell::new(None)),
            });
            let approver = session(&view, &[true, true, false], false).await?;
            let reviewer = session(&view, &[false, true, false], false).await?;
            let db = rusqlite::Connection::open(dir.path().join("marketplace.sqlite"))?;
            assert_eq!(snapshot(&approver).await?["keybaseConnected"], false);
            assert_eq!(snapshot(&reviewer).await?["keybaseConnected"], false);

            // Seed an opaque saved-capability token; detecting a configured connection
            // must not restore it or contact the external Keybase service.
            db.execute(
                "INSERT INTO capabilities(name, token) VALUES ('keybase', ?)",
                [b"saved-keybase-capability".as_slice()],
            )?;
            let connected = snapshot(&approver).await?;
            assert_eq!(connected["keybaseConnected"], true);
            assert!(!connected.to_string().contains("saved-keybase-capability"));
            assert_eq!(snapshot(&reviewer).await?["keybaseConnected"], false);
            for path in [
                "keybase-pb-token",
                "keybase/0123456789abcdef0123456789abcdef",
            ] {
                let response = post(&reviewer, path, b"test-token").await?;
                match response.get()?.which()? {
                    web::response::ClientError(error) => {
                        let message = error.get_description_html()?.to_str()?;
                        assert!(message.contains("approval permission denied"), "{message}");
                    }
                    _ => anyhow::bail!("reviewer was allowed to use {path}"),
                }
            }
            db.execute("DELETE FROM capabilities WHERE name='keybase'", [])?;
            assert_eq!(snapshot(&approver).await?["keybaseConnected"], false);
            Ok(())
        })
        .await
}
