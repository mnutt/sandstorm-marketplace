use anyhow::Result;
use capnp_rpc::{RpcSystem, rpc_twoparty_capnp::Side, twoparty};
use futures::AsyncReadExt;
use marketplace_grain::{
    grain_capnp::ui_view,
    registry::Registry,
    rpc::{PlatformSlot, View},
};
use std::{cell::RefCell, os::fd::FromRawFd, path::PathBuf, rc::Rc};
use tokio_util::compat::TokioAsyncReadCompatExt;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args
        .first()
        .is_some_and(|a| a == "--verify" || a == "--verify-json")
    {
        let info = marketplace_grain::spk::verify(std::path::Path::new(
            args.get(1)
                .ok_or_else(|| anyhow::anyhow!("expected SPK path"))?,
        ))?;
        if args[0] == "--verify-json" {
            let info = info
                .get_root_as_reader::<marketplace_grain::package_capnp::verified_info::Reader>()?;
            let value = serde_json::json!({
                "appId": marketplace_grain::ids::app_id(&marketplace_grain::ids::app_bytes(info.get_app_id()?)),
                "packageId": hex::encode(marketplace_grain::ids::package_bytes(info.get_package_id()?)),
                "version": info.get_version(),
                "title": {"defaultText": info.get_title()?.get_default_text()?.to_str()?},
                "authorPgpKeyFingerprint": info.get_author_pgp_key_fingerprint()?.to_str()?,
            });
            println!("{value}");
        } else {
            capnp::serialize::write_message(std::io::stdout(), &info)?;
        }
        return Ok(());
    }
    anyhow::ensure!(
        args.is_empty() || args == ["--init"],
        "usage: marketplace-grain [--init | --verify FILE]"
    );
    let root = std::env::var_os("MARKETPLACE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/var".into());
    let registry = Registry::new(root)?;
    registry.publish()?;
    tokio::task::LocalSet::new()
        .run_until(async move {
            // Sandstorm passes the connected bootstrap RPC socket on descriptor 3.
            let socket = unsafe { std::os::unix::net::UnixStream::from_raw_fd(3) };
            socket.set_nonblocking(true)?;
            let stream = tokio::net::UnixStream::from_std(socket)?.compat();
            let (reader, writer) = stream.split();
            let network =
                twoparty::VatNetwork::new(reader, writer, Side::Client, Default::default());
            let platform: PlatformSlot = Rc::new(RefCell::new(None));
            let view: ui_view::Client = capnp_rpc::new_client(View {
                registry,
                platform: platform.clone(),
            });
            let mut rpc = RpcSystem::new(Box::new(network), Some(view.client));
            *platform.borrow_mut() = Some(rpc.bootstrap(Side::Server));
            rpc.await?;
            Ok::<_, anyhow::Error>(())
        })
        .await
}
