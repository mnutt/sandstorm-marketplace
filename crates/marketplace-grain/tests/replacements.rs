mod common;
use anyhow::Result;
use common::*;
use ed25519_dalek::{Signer, SigningKey};
use marketplace_grain::{
    grain_capnp::ui_view,
    ids, package_capnp as p,
    registry::Registry,
    replacements::{Rule, Settings},
    rpc::View,
    submit_capnp::SubmissionState,
    web_session_capnp::web_session as web,
};
use std::{cell::RefCell, io::Write, rc::Rc};

fn rule(a: &SigningKey, b: Option<&SigningKey>, allowed: Option<Vec<String>>) -> Rule {
    Rule {
        original: ids::app_id(a.verifying_key().as_bytes()),
        replacement: b.map(|b| ids::app_id(b.verifying_key().as_bytes())),
        allowed_packages: allowed,
    }
}
fn upload_signed(registry: &Registry, key: &SigningKey, version: u32) -> Result<String> {
    let mut file = registry.temporary()?;
    file.write_all(&package_with_key(version, false, key, "Rotating app"))?;
    registry.upload(file.path())
}
async fn save(admin: &web::Client, registry: &Registry, rule: Rule) -> Result<()> {
    let mode = if registry
        .signing_key_rules()?
        .rules
        .iter()
        .any(|existing| existing.original == rule.original)
    {
        "edit"
    } else {
        "create"
    };
    let body = serde_json::to_vec(
        &serde_json::json!({"mode":mode,"revision":registry.signing_key_rules()?.revision,"rule":rule}),
    )?;
    let response = post(admin, "signing-keys", &body).await?;
    match response.get()?.which()? {
        web::response::NoContent(_) => Ok(()),
        _ => anyhow::bail!("rule save failed: {:?}", response.get()?),
    }
}
#[test]
fn graph_and_revocation_semantics() -> Result<()> {
    let a = key();
    let b = SigningKey::from_bytes(&[43; 32]);
    let c = SigningKey::from_bytes(&[44; 32]);
    let mut settings = Settings {
        revision: 0,
        rules: vec![
            rule(&a, Some(&b), Some(vec!["00".repeat(16)])),
            rule(&b, Some(&c), Some(vec![])),
        ],
    };
    settings.validate()?;
    assert_eq!(
        settings.current_key(a.verifying_key().as_bytes())?,
        c.verifying_key().to_bytes()
    );
    assert_eq!(
        settings.canonical_app(c.verifying_key().as_bytes(), &[1; 16])?,
        a.verifying_key().to_bytes()
    );
    assert!(
        settings
            .canonical_app(b.verifying_key().as_bytes(), &[0; 16])
            .is_err()
    );
    assert!(
        settings
            .canonical_app(a.verifying_key().as_bytes(), &[1; 16])
            .is_err()
    );
    assert!(
        settings
            .canonical_app(a.verifying_key().as_bytes(), &[0; 16])
            .is_ok()
    );
    settings.rules.push(rule(&c, Some(&a), None));
    assert!(settings.validate().is_err());
    settings.rules = vec![rule(&a, Some(&c), None), rule(&b, Some(&c), None)];
    assert!(settings.validate().is_err());
    settings.rules = vec![rule(&a, None, Some(vec![]))];
    settings.validate()?;
    assert!(settings.current_key(a.verifying_key().as_bytes()).is_err());
    settings.rules[0].allowed_packages = None;
    settings.validate()?;
    assert_eq!(
        settings.current_key(a.verifying_key().as_bytes())?,
        a.verifying_key().to_bytes()
    );
    Ok(())
}
#[tokio::test(flavor = "current_thread")]
async fn rotation_merges_history_revalidates_and_survives_restart() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let dir = tempfile::tempdir()?;
            let registry = Registry::new(dir.path().into())?;
            let a = key();
            let b = SigningKey::from_bytes(&[43; 32]);
            let app = ids::app_id(a.verifying_key().as_bytes());
            let first = upload_signed(&registry, &a, 1)?;
            registry.submission(&request(&first, Some(SubmissionState::Publish), 1))?;
            registry.review(&first, "approve", "")?;
            let second = upload_signed(&registry, &b, 2)?;
            let view: ui_view::Client = capnp_rpc::new_client(View {
                registry: registry.clone(),
                platform: Rc::new(RefCell::new(None)),
            });
            let admin = session_with_identity(
                &view,
                &[true, true, false],
                false,
                Some((&[7; 32], "Approver")),
            )
            .await?;
            let reviewer = session(&view, &[false, true, false], false).await?;
            let api = session(&view, &[false, false, true], true).await?;
            let body = serde_json::to_vec(
                &serde_json::json!({"mode":"create","revision":0,"rule":rule(&a,Some(&b),None)}),
            )?;
            for client in [&reviewer, &api] {
                assert!(matches!(
                    post(client, "signing-keys", &body).await?.get()?.which()?,
                    web::response::ClientError(_)
                ));
            }
            save(&admin, &registry, rule(&a, Some(&b), None)).await?;
            assert_eq!(
                ids::app_id(&ids::app_bytes(
                    registry
                        .metadata(&second)?
                        .get_root::<p::verified_info::Reader>()?
                        .get_app_id()?
                )),
                app
            );
            assert!(registry.submission(&request(&first, None, 0)).is_err());
            let mut signed = request(&first, None, 0);
            signed.truncate(signed.len() - 64);
            signed.extend_from_slice(&b.sign(&signed).to_bytes());
            registry.submission(&signed)?;
            let history = registry.audit_page(&app, None)?.events;
            assert!(
                history
                    .iter()
                    .any(|e| e.action == "submitted" && e.details.get("previousAppId").is_some())
            );
            assert!(
                history
                    .iter()
                    .any(|e| e.action == "signingKeyRuleChanged"
                        && e.actor.display_name == "Approver")
            );
            assert!(matches!(
                post(&admin, "signing-keys", &body).await?.get()?.which()?,
                web::response::ClientError(_)
            ));
            let third = upload_signed(&registry, &b, 3)?;
            assert_eq!(
                ids::app_id(&ids::app_bytes(
                    registry
                        .metadata(&third)?
                        .get_root::<p::verified_info::Reader>()?
                        .get_app_id()?
                )),
                app
            );
            save(&admin, &registry, rule(&a, Some(&b), Some(vec![]))).await?;
            assert!(!dir.path().join("www/packages").join(&first).exists());
            assert!(registry.review(&first, "approve", "").is_err());
            assert!(upload_signed(&registry, &a, 4).is_err());
            save(
                &admin,
                &registry,
                rule(&a, Some(&b), Some(vec![first.clone()])),
            )
            .await?;
            assert!(dir.path().join("www/packages").join(&first).exists());
            let restarted = Registry::new(dir.path().into())?;
            assert_eq!(restarted.signing_key_rules()?.revision, 3);
            assert_eq!(
                restarted
                    .signing_key_rules()?
                    .current_key(a.verifying_key().as_bytes())?,
                b.verifying_key().to_bytes()
            );
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn blocked_releases_merge_once_and_revocation_can_be_undone() -> Result<()> {
    tokio::task::LocalSet::new().run_until(async {
        let dir=tempfile::tempdir()?;
        let registry=Registry::new(dir.path().into())?;
        let a=key(); let b=SigningKey::from_bytes(&[43;32]);
        let second=upload_signed(&registry,&b,2)?;
        let view:ui_view::Client=capnp_rpc::new_client(View {registry:registry.clone(),platform:Rc::new(RefCell::new(None))});
        let admin=session(&view,&[true,true,false],false).await?;
        save(&admin,&registry,rule(&b,None,Some(vec![]))).await?;
        save(&admin,&registry,rule(&a,Some(&b),None)).await?;
        let app=ids::app_id(a.verifying_key().as_bytes());
        let history=registry.audit_page(&app,None)?.events;
        assert_eq!(history.iter().filter(|e|e.action=="submitted").count(),1);
        assert!(registry.review(&second,"approve","").is_err());
        save(&admin,&registry,rule(&b,None,None)).await?;
        registry.review(&second,"approve","")?;
        assert_eq!(registry.audit_page(&app,None)?.events.iter().filter(|e|e.action=="submitted").count(),1);
        // Established identity mappings cannot be redirected.
        let bad=serde_json::to_vec(&serde_json::json!({"mode":"edit","revision":registry.signing_key_rules()?.revision,"rule":rule(&a,None,None)}))?;
        assert!(matches!(post(&admin,"signing-keys",&bad).await?.get()?.which()?,web::response::ClientError(_)));
        // A missing package file is an operational error, not a revocation.
        std::fs::rename(registry.package_dir(&second)?.join("spk"),registry.package_dir(&second)?.join("saved-spk"))?;
        let revision=registry.signing_key_rules()?.revision;
        assert!(save(&admin,&registry,rule(&b,None,Some(vec![]))).await.is_err());
        assert_eq!(registry.signing_key_rules()?.revision,revision);
        Ok(())
    }).await
}

#[tokio::test(flavor = "current_thread")]
async fn revocation_blocks_downloads_even_when_publication_fails_and_retry_is_cheap() -> Result<()>
{
    tokio::task::LocalSet::new()
        .run_until(async {
            let dir = tempfile::tempdir()?;
            let registry = Registry::new(dir.path().into())?;
            let a = key();
            let b = SigningKey::from_bytes(&[43; 32]);
            let first = upload_signed(&registry, &a, 1)?;
            let unrelated = upload_signed(&registry, &b, 1)?;
            for id in [&first, &unrelated] {
                registry.set_state(id, SubmissionState::Publish, 1)?;
                registry.review(id, "approve", "")?;
            }
            let view: ui_view::Client = capnp_rpc::new_client(View {
                registry: registry.clone(),
                platform: Rc::new(RefCell::new(None)),
            });
            let admin = session(&view, &[true, true, false], false).await?;
            // Every recoverable publication must lose the revoked download, too.
            for tree in ["www.publish-previous", "www.publish-stage"] {
                std::fs::create_dir_all(dir.path().join(tree).join("packages"))?;
                std::fs::hard_link(
                    registry.package_dir(&first)?.join("spk"),
                    dir.path().join(tree).join("packages").join(&first),
                )?;
            }
            // An unrelated broken SPK must not stop preparation of this rule.
            // It does deliberately make the subsequent full publication fail.
            let other = registry.package_dir(&unrelated)?;
            std::fs::rename(other.join("spk"), other.join("saved-spk"))?;
            assert!(
                save(&admin, &registry, rule(&a, None, Some(vec![])))
                    .await
                    .is_err()
            );
            assert_eq!(registry.signing_key_rules()?.revision, 1);
            assert!(registry.package_dir(&first)?.join("spk").is_file());
            for tree in ["www", "www.publish-previous", "www.publish-stage"] {
                assert!(!dir.path().join(tree).join("packages").join(&first).exists());
            }
            assert!(dir.path().join("www/packages").join(&unrelated).is_file());
            std::fs::rename(other.join("saved-spk"), other.join("spk"))?;
            let app = ids::app_id(a.verifying_key().as_bytes());
            let decisions = registry
                .audit_page(&app, None)?
                .events
                .iter()
                .filter(|event| event.action == "signingKeyRuleChanged")
                .count();
            // No-op retry must neither reopen the revoked SPK nor change revision.
            let original = registry.package_dir(&first)?;
            std::fs::rename(original.join("spk"), original.join("saved-spk"))?;
            save(&admin, &registry, rule(&a, None, Some(vec![]))).await?;
            assert_eq!(registry.signing_key_rules()?.revision, 1);
            assert_eq!(
                registry
                    .audit_page(&app, None)?
                    .events
                    .iter()
                    .filter(|event| event.action == "signingKeyRuleChanged")
                    .count(),
                decisions
            );
            // A duplicate creation must not silently undo revocation.
            let body = serde_json::to_vec(&serde_json::json!({
                "mode": "create", "revision": 1, "rule": rule(&a, None, None),
            }))?;
            assert!(matches!(
                post(&admin, "signing-keys", &body).await?.get()?.which()?,
                web::response::ClientError(_)
            ));
            assert_eq!(registry.signing_key_rules()?.revision, 1);
            std::fs::rename(original.join("saved-spk"), original.join("spk"))?;
            save(&admin, &registry, rule(&a, None, None)).await?;
            assert!(dir.path().join("www/packages").join(&first).is_file());
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn unrelated_private_packages_are_not_reverified() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let dir = tempfile::tempdir()?;
            let registry = Registry::new(dir.path().into())?;
            let id = upload_signed(&registry, &key(), 1)?;
            std::fs::rename(
                registry.package_dir(&id)?.join("spk"),
                registry.package_dir(&id)?.join("saved-spk"),
            )?;
            let view: ui_view::Client = capnp_rpc::new_client(View {
                registry: registry.clone(),
                platform: Rc::new(RefCell::new(None)),
            });
            let admin = session(&view, &[true, true, false], false).await?;
            let b = SigningKey::from_bytes(&[43; 32]);
            let c = SigningKey::from_bytes(&[44; 32]);
            save(&admin, &registry, rule(&b, Some(&c), None)).await?;
            assert_eq!(registry.signing_key_rules()?.revision, 1);
            Ok(())
        })
        .await
}
