//! Durable submission mail, sent through Sandstorm rather than an app-owned SMTP server.
use crate::{
    audit::Event, database::decode, hack_session_capnp::hack_session_context, package_capnp as p,
    registry::Registry,
};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    recipient: String,
    pending: u64,
    failed: u64,
    last_error: Option<String>,
    last_sent_at: Option<String>,
    sending: bool,
}

pub fn settings(registry: &Registry) -> Result<Settings> {
    registry.database.read(|db| {
        let (recipient, last_sent_at) = db.query_row(
            "SELECT recipient,last_sent_at FROM notification_settings", [],
            |r| Ok((r.get(0)?, r.get(1)?)))?;
        let (pending, failed) = db.query_row(
            "SELECT count(*),count(last_error) FROM notification_outbox", [],
            |r| Ok((r.get(0)?, r.get(1)?)))?;
        let last_error = db.query_row(
            "SELECT last_error FROM notification_outbox WHERE last_error IS NOT NULL ORDER BY id DESC LIMIT 1",
            [], |r| r.get(0)).optional()?;
        Ok(Settings { recipient, pending, failed, last_error, last_sent_at,
            sending: registry.notification_busy.load(Ordering::Acquire) })
    })
}

pub fn configure(registry: &Registry, recipient: &str) -> Result<()> {
    let recipient = recipient.trim();
    // One plain mailbox, including a distribution-list address; never accept header syntax.
    let parts: Vec<_> = recipient.split('@').collect();
    ensure!(
        recipient.is_empty()
            || (recipient.len() <= 254
                && recipient.is_ascii()
                && parts.len() == 2
                && !parts[0].is_empty()
                && !parts[1].is_empty()
                && recipient
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".!#$%&'*+-/=?^_`{|}~@".contains(&b))),
        "Enter one email address, without a display name or recipient list"
    );
    registry.database.transaction(|db| {
        // Pending messages belong to the old destination. Never redirect them silently.
        db.execute("DELETE FROM notification_outbox WHERE recipient != ?", [recipient])?;
        db.execute("UPDATE notification_settings SET last_sent_at=CASE WHEN recipient=? THEN last_sent_at ELSE NULL END,recipient=?",
            params![recipient, recipient])?;
        Ok(())
    })
}

pub fn retry(registry: &Registry) -> Result<()> {
    registry.database.transaction(|db| {
        db.execute("UPDATE notification_outbox SET next_attempt=0", [])?;
        Ok(())
    })
}

pub(crate) fn enqueue(db: &Connection, event: &Event) -> Result<()> {
    if !matches!(event.action.as_str(), "authorStateChanged" | "resubmitted")
        || !matches!(
            event.details["next"]["requestState"].as_str(),
            Some("review" | "publish")
        )
    {
        return Ok(());
    }
    let recipient: String =
        db.query_row("SELECT recipient FROM notification_settings", [], |r| {
            r.get(0)
        })?;
    if recipient.is_empty() {
        return Ok(());
    }
    let Some(package_id) = &event.package_id else {
        return Ok(());
    };
    let bytes: Vec<u8> = db.query_row(
        "SELECT verified_metadata FROM releases WHERE package_id=?",
        [package_id],
        |r| r.get(0),
    )?;
    let metadata = decode(&bytes)?;
    let info = metadata.get_root::<p::verified_info::Reader>()?;
    // Metadata is author-controlled. Keep it out of headers except for a short, single-line title.
    let title: String = info
        .get_title()?
        .get_default_text()?
        .to_str()?
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(120)
        .collect();
    let state = event.details["next"]["requestState"].as_str().unwrap();
    let subject = format!("App submitted: {title} (build {})", info.get_version());
    let body = format!(
        "An app was submitted to your Sandstorm Marketplace.\n\nApp: {title}\nBuild: {}\nApp ID: {}\nPackage ID: {package_id}\nRequested state: {state}\nSubmitted: {}\n\nOpen this marketplace grain to review the submission.\n",
        info.get_version(),
        event.app_id,
        event.timestamp
    );
    db.execute("INSERT INTO notification_outbox(app_id,event_number,recipient,subject,body) VALUES(?,?,?,?,?)",
        params![event.app_id, event.number, recipient, subject, body])?;
    Ok(())
}

struct DeliveryGuard(Arc<AtomicBool>);
impl Drop for DeliveryGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Separate from the registry operation guard: network delays never hold up submissions.
pub fn start(
    registry: Registry,
    context: hack_session_context::Client,
    platform: crate::rpc::PlatformSlot,
) {
    let ready = registry.database.read(|db| {
        Ok(db.query_row(
            "SELECT EXISTS(SELECT 1 FROM notification_outbox WHERE next_attempt<=?)",
            [chrono::Utc::now().timestamp()],
            |r| r.get::<_, bool>(0),
        )?)
    });
    match ready {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => {
            tracing::warn!(?error, "Could not read notification queue");
            return;
        }
    }

    if registry
        .notification_busy
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let guard = DeliveryGuard(registry.notification_busy.clone());
    tokio::task::spawn_local(async move {
        let _guard = guard;
        let platform = platform.borrow().clone();
        if let Err(error) = deliver(&registry, &context, platform.as_ref()).await {
            tracing::warn!(?error, "Submission notification delivery failed");
        }
    });
}

struct Mail {
    id: i64,
    recipient: String,
    subject: String,
    body: String,
    attempts: u32,
}
async fn deliver(
    registry: &Registry,
    context: &hack_session_context::Client,
    platform: Option<&crate::rpc::Platform>,
) -> Result<()> {
    // Bound each pass. Subsequent requests/admin refreshes continue any remaining queue.
    for _ in 0..20 {
        let mail = registry.database.read(|db| {
            Ok(db.query_row("SELECT id,recipient,subject,body,attempts FROM notification_outbox WHERE next_attempt<=? ORDER BY id LIMIT 1",
                [chrono::Utc::now().timestamp()], |r| Ok(Mail {
                    id: r.get(0)?, recipient: r.get(1)?, subject: r.get(2)?, body: r.get(3)?, attempts: r.get(4)?,
                })).optional()?)
        })?;
        let Some(mail) = mail else {
            break;
        };
        let result = tokio::time::timeout(Duration::from_secs(30), send(context, &mail, platform))
            .await
            .map_err(anyhow::Error::from)
            .and_then(|result| result);
        registry.database.transaction(|db| {
            match &result {
                Ok(()) => {
                    // A settings change may have discarded this message during the send.
                    if db.execute("DELETE FROM notification_outbox WHERE id=?", [mail.id])? > 0 {
                        db.execute("UPDATE notification_settings SET last_sent_at=?", [chrono::Utc::now().to_rfc3339()])?;
                    }
                }
                Err(error) => {
                    let delay = (60_i64 * 2_i64.pow(mail.attempts.min(6))).min(3600);
                    db.execute("UPDATE notification_outbox SET attempts=attempts+1,next_attempt=?,last_error=? WHERE id=?",
                        params![chrono::Utc::now().timestamp()+delay, error.to_string().chars().take(1000).collect::<String>(), mail.id])?;
                }
            }
            Ok(())
        })?;
        // Don't hammer an unavailable mail service with the rest of the backlog.
        result?;
    }
    Ok(())
}
struct CancelMail(Option<tokio::sync::oneshot::Sender<()>>);
impl crate::activity_capnp::ongoing_notification::Server for CancelMail {
    fn cancel(
        &mut self,
        _: crate::activity_capnp::ongoing_notification::CancelParams,
        _: crate::activity_capnp::ongoing_notification::CancelResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
        capnp::capability::Promise::ok(())
    }
}

async fn send(
    context: &hack_session_context::Client,
    mail: &Mail,
    platform: Option<&crate::rpc::Platform>,
) -> Result<()> {
    // Keep the grain awake if the submitting client disconnects before mail is handed off.
    // Tests use no platform; real sessions always have the Sandstorm bootstrap capability.
    let Some(platform) = platform else {
        return send_message(context, mail).await;
    };
    let (cancel, cancelled) = tokio::sync::oneshot::channel();
    let mut request = platform.stay_awake_request();
    request
        .get()
        .init_display_info()
        .init_caption()
        .set_default_text("Sending submission notification");
    request
        .get()
        .set_notification(capnp_rpc::new_client(CancelMail(Some(cancel))));
    let response = request.send().promise.await?;
    let _wake = response.get()?.get_handle()?;
    tokio::select! {
        _ = cancelled => anyhow::bail!("Notification delivery cancelled"),
        result = send_message(context, mail) => result,
    }
}

async fn send_message(context: &hack_session_context::Client, mail: &Mail) -> Result<()> {
    let response = context.get_public_id_request().send().promise.await?;
    let public = response.get()?;
    let public_id = public.get_public_id()?.to_str()?;
    let hostname = public.get_hostname()?.to_str()?;
    let sender = format!("{public_id}@{hostname}");
    let port: crate::email_capnp::email_send_port::Client = crate::rpc::cast(context.clone());
    let mut request = port.send_request();
    let mut email = request.get().init_email();
    email.set_date(chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default());
    email.reborrow().init_from().set_address(&sender);
    email
        .reborrow()
        .init_to(1)
        .get(0)
        .set_address(&mail.recipient);
    // Stable across retries; SMTP delivery itself cannot guarantee exactly-once delivery.
    email.set_message_id(format!("<marketplace-{public_id}-{}@{hostname}>", mail.id));
    email.set_subject(&mail.subject);
    email.set_text(&mail.body);
    request.send().promise.await?;
    Ok(())
}
