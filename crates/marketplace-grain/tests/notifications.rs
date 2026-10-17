mod common;
use anyhow::Result;
use capnp::{capability::Promise, traits::HasTypeId};
use marketplace_grain::{
    api_session_capnp::api_session,
    email_capnp::email_send_port,
    grain_capnp::{session_context, ui_view},
    hack_session_capnp::hack_session_context,
    registry::Registry,
    rpc::{View, cast},
    submit_capnp::SubmissionState,
    web_session_capnp::web_session as web,
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

#[derive(Default)]
struct Mailbox {
    fail: Cell<bool>,
    awake: Cell<usize>,
    messages: RefCell<Vec<(String, String, String)>>,
}
struct Platform(Rc<Mailbox>);
struct Wake {
    mailbox: Rc<Mailbox>,
    _notification: marketplace_grain::activity_capnp::ongoing_notification::Client,
}
impl marketplace_grain::util_capnp::handle::Server for Wake {}
impl Drop for Wake {
    fn drop(&mut self) {
        self.mailbox.awake.set(self.mailbox.awake.get() - 1);
    }
}
impl marketplace_grain::grain_capnp::sandstorm_api::Server<capnp::any_pointer::Owned> for Platform {
    fn stay_awake(
        &mut self,
        params: marketplace_grain::grain_capnp::sandstorm_api::StayAwakeParams<
            capnp::any_pointer::Owned,
        >,
        mut results: marketplace_grain::grain_capnp::sandstorm_api::StayAwakeResults<
            capnp::any_pointer::Owned,
        >,
    ) -> Promise<(), capnp::Error> {
        self.0.awake.set(self.0.awake.get() + 1);
        results.get().set_handle(capnp_rpc::new_client(Wake {
            mailbox: self.0.clone(),
            _notification: params.get().unwrap().get_notification().unwrap(),
        }));
        Promise::ok(())
    }
}
fn platform(mailbox: Rc<Mailbox>) -> marketplace_grain::rpc::PlatformSlot {
    Rc::new(RefCell::new(Some(capnp_rpc::new_client(Platform(mailbox)))))
}
struct Context(Rc<Mailbox>);
impl session_context::Server for Context {}
impl hack_session_context::Server for Context {
    fn get_public_id(
        &mut self,
        _: hack_session_context::GetPublicIdParams,
        mut results: hack_session_context::GetPublicIdResults,
    ) -> Promise<(), capnp::Error> {
        results.get().set_public_id("test-grain");
        results.get().set_hostname("example.invalid");
        Promise::ok(())
    }
}
impl email_send_port::Server for Context {
    fn send(
        &mut self,
        params: email_send_port::SendParams,
        _: email_send_port::SendResults,
    ) -> Promise<(), capnp::Error> {
        let result = (|| {
            assert_eq!(self.0.awake.get(), 1, "delivery must keep the grain awake");
            let email = params.get()?.get_email()?;
            assert_eq!(
                email.get_from()?.get_address()?.to_str()?,
                "test-grain@example.invalid"
            );
            self.0.messages.borrow_mut().push((
                email.get_to()?.get(0).get_address()?.to_str()?.into(),
                email.get_subject()?.to_str()?.into(),
                email.get_message_id()?.to_str()?.into(),
            ));
            assert!(email.get_text()?.to_str()?.contains("Build: 1"));
            if self.0.fail.get() {
                return Err(capnp::Error::failed("mail service unavailable".into()));
            }
            Ok(())
        })();
        Promise::from_future(async { result })
    }
}
async fn session(view: &ui_view::Client, mailbox: Rc<Mailbox>, api: bool) -> Result<web::Client> {
    let mut open = view.new_session_request();
    open.get().set_session_type(if api {
        api_session::Client::TYPE_ID
    } else {
        web::Client::TYPE_ID
    });
    let mut bits = open.get().init_user_info().init_permissions(3);
    for i in 0..3 {
        bits.set(i, true);
    }
    let context: hack_session_context::Client = capnp_rpc::new_client(Context(mailbox));
    open.get().set_context(cast(context));
    Ok(cast(open.send().promise.await?.get()?.get_session()?))
}
async fn post_ok(client: &web::Client, path: &str, body: &[u8]) -> Result<()> {
    let response = common::post(client, path, body).await?;
    assert!(matches!(
        response.get()?.which()?,
        web::response::NoContent(_) | web::response::Content(_)
    ));
    Ok(())
}
async fn settle() {
    // Let local capability calls and the delivery task complete without real network I/O.
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}
fn count(db: &rusqlite::Connection) -> i64 {
    db.query_row("SELECT count(*) FROM notification_outbox", [], |r| r.get(0))
        .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn submission_mail_defaults_permissions_delivery_and_retries() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let dir = tempfile::tempdir()?;
            let registry = Registry::new(dir.path().into())?;
            let id = common::upload(&registry, 1);
            let db = rusqlite::Connection::open(dir.path().join("marketplace.sqlite"))?;
            let mailbox = Rc::new(Mailbox::default());
            let view: ui_view::Client = capnp_rpc::new_client(View {
                registry: registry.clone(),
                platform: platform(mailbox.clone()),
            });
            let admin = session(&view, mailbox.clone(), false).await?;
            let api = session(&view, mailbox.clone(), true).await?;
            let reviewer = common::session(&view, &[false, true, false], false).await?;
            assert_eq!(
                db.query_row("SELECT recipient FROM notification_settings", [], |r| {
                    r.get::<_, String>(0)
                })?,
                ""
            );
            post_ok(
                &api,
                "status",
                &common::request(&id, Some(SubmissionState::Review), 0),
            )
            .await?;
            settle().await;
            assert!(mailbox.messages.borrow().is_empty());
            assert_eq!(count(&db), 0);
            for client in [&reviewer, &api] {
                let result = common::post(client, "notifications", b"admin@example.org").await?;
                assert!(!matches!(
                    result.get()?.which()?,
                    web::response::NoContent(_)
                ));
            }
            for invalid in [
                "one@example.org,two@example.org",
                "one@example.org\r\nBcc: victim@example.org",
            ] {
                let result = common::post(&admin, "notifications", invalid.as_bytes()).await?;
                assert!(!matches!(
                    result.get()?.which()?,
                    web::response::NoContent(_)
                ));
            }
            post_ok(&admin, "notifications", b"admin@example.org").await?;
            // Fresh signed intent sends once; identical retries and status polls do not.
            let request = common::request(&id, Some(SubmissionState::Review), 1);
            post_ok(&api, "status", &request).await?;
            settle().await;
            assert_eq!(mailbox.messages.borrow().len(), 1);
            assert_eq!(mailbox.messages.borrow()[0].0, "admin@example.org");
            assert!(mailbox.messages.borrow()[0].1.contains("Rust test app"));
            assert_eq!(count(&db), 0);
            post_ok(&api, "status", &request).await?;
            post_ok(&api, "status", &common::request(&id, None, 0)).await?;
            post_ok(
                &api,
                "status",
                &common::request(&id, Some(SubmissionState::Ignore), 2),
            )
            .await?;
            settle().await;
            assert_eq!(mailbox.messages.borrow().len(), 1);
            // Delivery failure does not fail the submission and is durably visible.
            mailbox.fail.set(true);
            post_ok(
                &api,
                "status",
                &common::request(&id, Some(SubmissionState::Publish), 3),
            )
            .await?;
            settle().await;
            assert_eq!(count(&db), 1);
            let error: String =
                db.query_row("SELECT last_error FROM notification_outbox", [], |r| {
                    r.get(0)
                })?;
            assert!(error.contains("mail service unavailable"));
            post_ok(&api, "status", &common::request(&id, None, 0)).await?;
            settle().await;
            assert_eq!(
                mailbox.messages.borrow().len(),
                2,
                "backoff prevents immediate automatic retry"
            );
            let restarted = Registry::new(dir.path().into())?;
            assert_eq!(restarted.package_ids()?.len(), 1);
            assert_eq!(count(&db), 1);
            let restarted_view: ui_view::Client = capnp_rpc::new_client(View {
                registry: restarted,
                platform: platform(mailbox.clone()),
            });
            let admin = session(&restarted_view, mailbox.clone(), false).await?;
            mailbox.fail.set(false);
            // Concurrent retry requests must not deliver the same queued message twice.
            let (first, second) = futures::join!(
                post_ok(&admin, "notifications/retry", b""),
                post_ok(&admin, "notifications/retry", b"")
            );
            first?;
            second?;
            settle().await;
            assert_eq!(count(&db), 0);
            assert_eq!(mailbox.messages.borrow().len(), 3);
            assert_eq!(
                mailbox.messages.borrow()[1].2,
                mailbox.messages.borrow()[2].2
            );
            // Neither re-saving the setting nor bulk upload creates historical mail.
            post_ok(&admin, "notifications", b"admin@example.org").await?;
            common::upload(&registry, 2);
            assert_eq!(count(&db), 0);
            mailbox.fail.set(true);
            post_ok(
                &api,
                "status",
                &common::request(&id, Some(SubmissionState::Publish), 4),
            )
            .await?;
            settle().await;
            assert_eq!(count(&db), 1);
            post_ok(&admin, "notifications", b"list@example.org").await?;
            assert_eq!(
                count(&db),
                0,
                "changing destination discards old pending mail"
            );
            post_ok(&admin, "notifications", b"").await?;
            post_ok(
                &api,
                "status",
                &common::request(&id, Some(SubmissionState::Publish), 5),
            )
            .await?;
            settle().await;
            assert_eq!(count(&db), 0);
            assert_eq!(
                mailbox.awake.get(),
                0,
                "delivery must release the wake handle"
            );
            Ok(())
        })
        .await
}

#[test]
fn schema_upgrade_preserves_registry_and_defaults_to_disabled() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = rusqlite::Connection::open(dir.path().join("marketplace.sqlite"))?;
    db.execute_batch(include_str!("../src/schema.sql"))?;
    db.execute_batch("PRAGMA user_version=1; UPDATE registry_state SET state_revision=42")?;
    let _registry = Registry::new(dir.path().into())?;
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?,
        2
    );
    assert_eq!(
        db.query_row("SELECT state_revision FROM registry_state", [], |r| r
            .get::<_, i64>(0))?,
        42
    );
    assert_eq!(
        db.query_row("SELECT recipient FROM notification_settings", [], |r| {
            r.get::<_, String>(0)
        })?,
        ""
    );
    assert_eq!(count(&db), 0);
    Ok(())
}

#[test]
fn notification_queue_commits_atomically_with_submission() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let registry = Registry::new(dir.path().into())?;
    let id = common::upload(&registry, 1);
    let db = rusqlite::Connection::open(dir.path().join("marketplace.sqlite"))?;
    db.execute(
        "UPDATE notification_settings SET recipient='admin@example.org'",
        [],
    )?;
    db.execute_batch("CREATE TRIGGER fail_mail BEFORE INSERT ON notification_outbox BEGIN SELECT RAISE(ABORT,'simulated queue write failure'); END;")?;
    let request = common::request(&id, Some(SubmissionState::Review), 0);
    assert!(registry.submission(&request).is_err());
    assert_eq!(
        registry
            .status(&id)?
            .get_root::<marketplace_grain::submit_capnp::submission_status::Reader>()?
            .get_request_state()?,
        SubmissionState::Ignore
    );
    assert_eq!(count(&db), 0);
    assert_eq!(
        db.query_row("SELECT count(*) FROM audit_events", [], |r| r
            .get::<_, i64>(0))?,
        1
    );
    db.execute_batch("DROP TRIGGER fail_mail")?;
    registry.submission(&request)?;
    assert_eq!(count(&db), 1);
    assert_eq!(
        db.query_row("SELECT count(*) FROM audit_events", [], |r| r
            .get::<_, i64>(0))?,
        2
    );
    Ok(())
}
