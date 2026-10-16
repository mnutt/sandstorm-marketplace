//! Keep long uploads alive without blocking the single-threaded capability RPC loop.
use crate::{
    activity_capnp::ongoing_notification,
    registry::{Operation, Registry},
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
        &mut self,
        _: ongoing_notification::CancelParams,
        _: ongoing_notification::CancelResults,
    ) -> Promise<(), capnp::Error> {
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

async fn run<F>(
    registry: Registry,
    platform: PlatformSlot,
    operation: Operation,
    import: bool,
    task: F,
) -> Result<()>
where
    F: FnOnce(Registry, &AtomicBool) -> Result<()> + Send + 'static,
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
                "Verifying an uploaded app package"
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
    let span = tracing::Span::current();
    let result = tokio::task::spawn_blocking(move || {
        // The write gate stays held even if the RPC future is cancelled while
        // the blocking task is finishing its cancellation/cleanup checks.
        let _operation = operation;
        let _span = span.enter();
        check_cancelled(&cancelled)?;
        task(registry, &cancelled)
    })
    .await
    .map_err(error)?;
    drop(wake);
    tracing::info!(
        import,
        "Package processing finished; released Sandstorm wake handle"
    );
    result
}

pub(crate) async fn upload(
    registry: Registry,
    platform: PlatformSlot,
    operation: Operation,
    file: tempfile::NamedTempFile,
    import: bool,
) -> Result<()> {
    run(
        registry,
        platform,
        operation,
        import,
        move |registry, cancelled| {
            if import {
                registry.import_catalog_cancellable(file.path(), cancelled)
            } else {
                registry
                    .upload_cancellable(file.path(), cancelled)
                    .map(|_| ())
            }
        },
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
            &mut self,
            params: sandstorm_api::StayAwakeParams<capnp::any_pointer::Owned>,
            mut results: sandstorm_api::StayAwakeResults<capnp::any_pointer::Owned>,
        ) -> Promise<(), capnp::Error> {
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
                registry.begin_operation()?,
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
            );
            let monitor = async {
                tokio::time::timeout(Duration::from_secs(2), started_rx).await??;
                assert!(registry.is_busy());
                assert!(registry.begin_operation().is_err());
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
