mod package;
mod validation;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Task,
}
#[derive(Subcommand)]
enum Task {
    /// Build and stage a debug grain for spk dev (no signing key required).
    PrepareDev,
    /// Build, stage, and sign the marketplace SPK using Rust.
    Package {
        #[arg(long, default_value = "maintainer@example.invalid")]
        contact_email: String,
        #[arg(long)]
        debug: bool,
    },
    /// Boot the packaged runtime in Bubblewrap with disposable storage.
    TestPackage,
    /// Compare against the original SDK and run its spk publish client.
    TestInterop {
        #[arg(long)]
        spk: PathBuf,
    },
}
fn run(command: &mut Command) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("starting {command:?}"))?;
    ensure!(status.success(), "{command:?} failed: {status}");
    Ok(())
}
fn output(command: &mut Command) -> Result<Output> {
    let output = command
        .output()
        .with_context(|| format!("starting {command:?}"))?;
    ensure!(
        output.status.success(),
        "{command:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}
fn build(debug: bool) -> Result<()> {
    let mut command = Command::new("cargo");
    command.args(["build", "-p", "marketplace-grain"]);
    if !debug {
        command.arg("--release");
    }
    run(&mut command)
}
fn main() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    std::env::set_current_dir(root)?;
    match Args::parse().command {
        Task::PrepareDev => package::prepare_dev(root),
        Task::Package {
            contact_email,
            debug,
        } => package::package(root, &contact_email, debug),
        Task::TestPackage => validation::package(root),
        Task::TestInterop { spk } => validation::interop(root, &spk),
    }
}
