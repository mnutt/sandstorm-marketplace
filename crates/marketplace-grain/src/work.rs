//! Keep long uploads alive without blocking the single-threaded capability RPC loop.
use crate::{
    activity_capnp::ongoing_notification,
    registry::Registry,
    rpc::{PlatformSlot, error},
};
use anyhow::{Result, ensure};
use capnp::capability::Promise;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(crate) fn check_cancelled(cancelled: &AtomicBool) -> Result<()> {
    ensure!(!cancelled.load(Ordering::Acquire), "operation cancelled");
    Ok(())
}
struct Notification(Arc<AtomicBool>);
impl ongoing_notification::Server for Notification {
    fn cancel(
        self: std::rc::Rc<Self>,
        _: ongoing_notification::CancelParams,
        _: ongoing_notification::CancelResults,
    ) -> impl std::future::Future<Output = capnp::Result<()>> + 'static {
        tracing::info!("Sandstorm requested cancellation of package processing");
        self.0.store(true, Ordering::Release);
        Promise::ok(())
    }
}
struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

async fn run<T, P, C>(
    registry: Registry,
    platform: PlatformSlot,
    import: bool,
    prepare: P,
    commit: C,
) -> Result<()>
where
    T: Send + 'static,
    P: FnOnce(Registry, &AtomicBool) -> Result<T> + Send + 'static,
    C: FnOnce(Registry, T, &AtomicBool) -> Result<()> + Send + 'static,
{
    let cancelled = Arc::new(AtomicBool::new(false));
    let _cancel_on_drop = CancelOnDrop(cancelled.clone());
    let platform = platform.borrow().clone();
    // None is used by in-process tests. The real grain connects the platform
    // before serving any sessions. Failure to acquire a wake handle is fatal.
    let wake = if let Some(platform) = platform {
        let mut request = platform.stay_awake_request();
        request
            .get()
            .init_display_info()
            .init_caption()
            .set_default_text(if import {
                "Importing marketplace packages"
            } else {
                "Verifying marketplace packages"
            });
        request
            .get()
            .set_notification(capnp_rpc::new_client(Notification(cancelled.clone())));
        let response = request.send().promise.await?;
        let handle = response.get()?.get_handle()?;
        tracing::info!(
            import,
            "Acquired Sandstorm wake handle for package processing"
        );
        Some(handle)
    } else {
        None
    };
    let started = std::time::Instant::now();
    let span = tracing::Span::current();
    let preparation_registry = registry.clone();
    let preparation_cancelled = cancelled.clone();
    let preparation_span = span.clone();
    let prepared = tokio::task::spawn_blocking(move || {
        let _span = preparation_span.enter();
        check_cancelled(&preparation_cancelled)?;
        prepare(preparation_registry, &preparation_cancelled)
    })
    .await
    .map_err(error)??;
    tracing::info!(
        import,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "Preparation complete; waiting to commit"
    );
    // Dropping the RPC future cancels this wait immediately. Notification
    // cancellation must also remove the waiter without waiting for the holder.
    let queued = std::time::Instant::now();
    let operation = tokio::select! {
        operation = registry.begin_operation() => operation,
        _ = async {
            while !cancelled.load(Ordering::Acquire) {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        } => anyhow::bail!("operation cancelled"),
    };
    tracing::info!(
        import,
        wait_ms = queued.elapsed().as_millis() as u64,
        "Registry gate acquired"
    );
    let committing = std::time::Instant::now();
    let result = tokio::task::spawn_blocking(move || {
        // Keep the gate even if the RPC is dropped during commit/cleanup.
        let _operation = operation;
        let _span = span.enter();
        check_cancelled(&cancelled)?;
        commit(registry, prepared, &cancelled)
    })
    .await
    .map_err(error)?;
    drop(wake);
    tracing::info!(
        import,
        commit_ms = committing.elapsed().as_millis() as u64,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "Package processing finished; released Sandstorm wake handle"
    );
    result
}

pub(crate) async fn upload(
    registry: Registry,
    platform: PlatformSlot,
    file: tempfile::NamedTempFile,
    import: bool,
) -> Result<()> {
    if import {
        run(
            registry,
            platform,
            true,
            move |registry, cancelled| registry.prepare_import(file.path(), cancelled),
            |registry, prepared, cancelled| registry.commit_import(prepared, cancelled),
        )
        .await
    } else {
        run(
            registry,
            platform,
            false,
            move |registry, cancelled| registry.prepare_upload(file.path(), cancelled),
            |registry, prepared, cancelled| registry.commit_upload(prepared, cancelled).map(|_| ()),
        )
        .await
    }
}

pub(crate) async fn update_signing_key(
    registry: Registry,
    platform: PlatformSlot,
    update: crate::replacements::Update,
) -> Result<()> {
    run(
        registry,
        platform,
        false,
        move |registry, cancelled| registry.prepare_key_rule(update, cancelled),
        |registry, prepared, cancelled| registry.commit_key_rule(prepared, cancelled),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        grain_capnp::{sandstorm_api, ui_view},
        rpc::View,
        util_capnp::handle,
    };
    use std::{cell::RefCell, rc::Rc, sync::atomic::AtomicUsize, time::Duration};
    struct Wake(Arc<AtomicUsize>);
    impl handle::Server for Wake {}
    impl Drop for Wake {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    struct Platform {
        handles: Arc<AtomicUsize>,
        notification: Rc<RefCell<Option<ongoing_notification::Client>>>,
    }
    impl sandstorm_api::Server<capnp::any_pointer::Owned> for Platform {
        fn stay_awake(
            self: std::rc::Rc<Self>,
            params: sandstorm_api::StayAwakeParams<capnp::any_pointer::Owned>,
            mut results: sandstorm_api::StayAwakeResults<capnp::any_pointer::Owned>,
        ) -> impl std::future::Future<Output = capnp::Result<()>> + 'static {
            *self.notification.borrow_mut() =
                Some(params.get().unwrap().get_notification().unwrap());
            self.handles.fetch_add(1, Ordering::SeqCst);
            results
                .get()
                .set_handle(capnp_rpc::new_client(Wake(self.handles.clone())));
            Promise::ok(())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn registry_operations_queue_in_order_and_cancel_without_holding_the_gate() -> Result<()>
    {
        let dir = tempfile::tempdir()?;
        let registry = Registry::new(dir.path().to_owned())?;
        let first = registry.begin_operation().await;
        let mut cancelled = Box::pin(registry.begin_operation());
        let mut second = Box::pin(registry.begin_operation());
        let mut third = Box::pin(registry.begin_operation());
        assert!(futures::poll!(&mut cancelled).is_pending());
        assert!(futures::poll!(&mut second).is_pending());
        assert!(futures::poll!(&mut third).is_pending());
        drop(cancelled);
        drop(first);
        // Polling a later arrival first must not let it jump the queue.
        assert!(futures::poll!(&mut third).is_pending());
        let second = tokio::time::timeout(Duration::from_secs(1), second).await?;
        assert!(registry.is_busy());
        assert!(futures::poll!(&mut third).is_pending());
        drop(second);
        let third = tokio::time::timeout(Duration::from_secs(1), third).await?;
        drop(third);
        assert!(!registry.is_busy());
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_while_waiting_does_not_commit_or_wait_for_the_gate() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = Registry::new(dir.path().to_owned())?;
        let holder = registry.begin_operation().await;
        let handles = Arc::new(AtomicUsize::new(0));
        let notification = Rc::new(RefCell::new(None));
        let platform = Rc::new(RefCell::new(Some(capnp_rpc::new_client(Platform {
            handles: handles.clone(),
            notification: notification.clone(),
        }))));
        let (prepared_tx, prepared_rx) = tokio::sync::oneshot::channel();
        let job = run(
            registry.clone(),
            platform,
            false,
            move |_, _| {
                prepared_tx.send(()).unwrap();
                Ok(())
            },
            |_, (), _| panic!("canceled work must not commit"),
        );
        let cancel = async {
            prepared_rx.await?;
            let notification = notification.borrow().clone().unwrap();
            notification.cancel_request().send().promise.await?;
            Ok::<_, anyhow::Error>(())
        };
        let (result, cancel) = tokio::time::timeout(Duration::from_secs(2), async {
            futures::join!(job, cancel)
        })
        .await?;
        cancel?;
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert!(registry.is_busy());
        drop(holder);
        assert!(!registry.is_busy());
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropped_rpc_keeps_gate_until_commit_worker_exits() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = Registry::new(dir.path().to_owned())?;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let mut job = Box::pin(run(
            registry.clone(),
            Rc::new(RefCell::new(None)),
            false,
            |_, _| Ok(()),
            move |_, (), cancelled| {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5))?;
                check_cancelled(cancelled)
            },
        ));
        tokio::select! {
            result = &mut job => panic!("worker finished early: {result:?}"),
            result = tokio::time::timeout(Duration::from_secs(2), started_rx) => { result??; }
        }
        drop(job);
        assert!(registry.is_busy());
        release_tx.send(())?;
        let next = tokio::time::timeout(Duration::from_secs(2), registry.begin_operation()).await?;
        drop(next);
        assert!(!registry.is_busy());
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn worker_keeps_grain_awake_and_rpc_responsive_and_honors_cancel() -> Result<()> {
        for cancel in [false, true] {
            let dir = tempfile::tempdir()?;
            let registry = Registry::new(dir.path().to_owned())?;
            let handles = Arc::new(AtomicUsize::new(0));
            let notification = Rc::new(RefCell::new(None));
            let client = capnp_rpc::new_client(Platform {
                handles: handles.clone(),
                notification: notification.clone(),
            });
            let platform = Rc::new(RefCell::new(Some(client)));
            let view: ui_view::Client = capnp_rpc::new_client(View {
                registry: registry.clone(),
                platform: platform.clone(),
            });
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let worker_handles = handles.clone();
            let job = run(
                registry.clone(),
                platform,
                true,
                move |_, cancelled| {
                    ensure!(
                        worker_handles.load(Ordering::SeqCst) == 1,
                        "wake handle must cover blocking work"
                    );
                    started_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(5))?;
                    check_cancelled(cancelled)
                },
                |registry, (), cancelled| {
                    assert!(registry.is_busy());
                    check_cancelled(cancelled)
                },
            );
            let monitor = async {
                tokio::time::timeout(Duration::from_secs(2), started_rx).await??;
                assert!(!registry.is_busy());
                let concurrent_operation =
                    tokio::time::timeout(Duration::from_secs(1), registry.begin_operation())
                        .await?;
                drop(concurrent_operation);
                // A real capability request must be serviced while the worker is blocked.
                let response = tokio::time::timeout(
                    Duration::from_secs(2),
                    view.get_view_info_request().send().promise,
                )
                .await??;
                assert_eq!(response.get()?.get_permissions()?.len(), 3);
                if cancel {
                    let notification = notification.borrow().clone().unwrap();
                    notification.cancel_request().send().promise.await?;
                }
                release_tx.send(())?;
                Ok::<_, anyhow::Error>(())
            };
            let (result, monitor) = futures::join!(job, monitor);
            monitor?;
            if cancel {
                assert!(result.unwrap_err().to_string().contains("cancelled"));
            } else {
                result?;
            }
            assert!(!registry.is_busy());
            for _ in 0..3 {
                tokio::task::yield_now().await;
            }
            assert_eq!(handles.load(Ordering::SeqCst), 0);
        }
        Ok(())
    }
}
