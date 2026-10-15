use crate::{output, run};
use anyhow::{Result, ensure};
use std::{
    fs,
    io::Write,
    os::{
        fd::AsRawFd,
        unix::{fs::PermissionsExt, net::UnixStream, process::CommandExt},
    },
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
pub fn package(root: &Path) -> Result<()> {
    let stage = root.join("build/package/stage");
    ensure!(
        stage.join("marketplace-grain").exists(),
        "run cargo xtask package first"
    );
    let storage = tempfile::tempdir()?;
    let (child_socket, _parent_socket) = UnixStream::pair()?;
    let fd = child_socket.as_raw_fd();
    let mut command = Command::new("bwrap");
    command
        .args(["--die-with-parent", "--unshare-pid", "--ro-bind"])
        .arg(stage)
        .arg("/")
        .arg("--bind")
        .arg(storage.path())
        .arg("/var")
        .args([
            "--tmpfs",
            "/tmp",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--chdir",
            "/",
            "--",
            "/marketplace-grain",
            "--init",
        ]);
    // Only descriptor operations are performed between fork and exec.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(fd, 3) == -1 || libc::fcntl(3, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = Running(command.spawn()?);
    drop(child_socket);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !storage.path().join("www/index.html").exists() {
        ensure!(
            child.0.try_wait()?.is_none(),
            "packaged grain exited during startup"
        );
        ensure!(
            Instant::now() < deadline,
            "timed out waiting for initial publication"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let index: serde_json::Value =
        serde_json::from_slice(&fs::read(storage.path().join("www/apps/index.json"))?)?;
    ensure!(
        index == serde_json::json!({"apps": []}),
        "unexpected initial catalog"
    );
    ensure!(
        storage.path().join("www/marketplace.js").is_file(),
        "missing browser assets"
    );
    std::thread::sleep(Duration::from_millis(100));
    ensure!(
        child.0.try_wait()?.is_none(),
        "grain exited after initialization"
    );
    println!(
        "Packaged grain started with RPC fd 3 and published its site using bundled runtime files."
    );
    Ok(())
}
struct GpgHome(std::path::PathBuf);
impl GpgHome {
    fn command(&self) -> Command {
        let mut command = Command::new("gpg");
        command.arg("--homedir").arg(&self.0).args([
            "--batch",
            "--pinentry-mode",
            "loopback",
            "--passphrase",
            "",
        ]);
        command
    }
}
impl Drop for GpgHome {
    fn drop(&mut self) {
        let _ = Command::new("gpgconf")
            .arg("--homedir")
            .arg(&self.0)
            .args(["--kill", "gpg-agent"])
            .output();
    }
}
pub fn interop(root: &Path, spk: &Path) -> Result<()> {
    super::build(true)?;
    let fixture = tempfile::tempdir()?;
    let path = fixture.path();
    let keyring = path.join("keyring");
    fs::write(&keyring, [])?;
    fs::set_permissions(&keyring, fs::Permissions::from_mode(0o600))?;
    let result = output(Command::new(spk).args(["keygen", "-q", "-k"]).arg(&keyring))?;
    let app_id = String::from_utf8(result.stdout)?.trim().to_owned();
    fs::create_dir(path.join("stage"))?;
    fs::write(path.join("stage/payload"), "Rust interoperability test")?;
    let home = GpgHome(path.join("gnupg"));
    fs::create_dir(&home.0)?;
    fs::set_permissions(&home.0, fs::Permissions::from_mode(0o700))?;
    run(home.command().args([
        "--quick-generate-key",
        "Marketplace Test <test@example.invalid>",
        "ed25519",
        "sign",
        "0",
    ]))?;
    fs::write(
        path.join("pgp-key"),
        output(home.command().arg("--export"))?.stdout,
    )?;
    let mut sign = home
        .command()
        .arg("--sign")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    write!(
        sign.stdin.take().unwrap(),
        "I am the author of the Sandstorm.io app with the following ID: {app_id}"
    )?;
    let proof = sign.wait_with_output()?;
    ensure!(proof.status.success(), "GPG signing failed");
    fs::write(path.join("pgp-proof"), proof.stdout)?;
    let definition = r#"@0xdadce2b6ae08e5ef;
using Package = import "/sandstorm/package.capnp";
const pkgdef :Package.PackageDefinition = (
 id = "@APP_ID@", manifest = (
  appTitle = (defaultText = "Rust interoperability"), appVersion = 1,
  appMarketingVersion = (defaultText = "1.0"), continueCommand = (argv = ["/payload"]),
  metadata = (author = (contactEmail = "test@example.invalid", pgpSignature = embed "pgp-proof"),
   pgpKeyring = embed "pgp-key", categories = [developerTools], license = (openSource = apache2),
   shortDescription = (defaultText = "Test package"), description = (defaultText = "Interoperability test."))
 ), sourceMap = (searchPath = [(sourcePath = "stage")]), alwaysInclude = ["."]
);"#.replace("@APP_ID@", &app_id);
    fs::write(path.join("package.capnp"), definition)?;
    output(
        Command::new(spk)
            .args(["pack", "-k"])
            .arg(&keyring)
            .arg("-I")
            .arg(root.join("crates/marketplace-grain/schema"))
            .arg("-p")
            .arg(format!("{}:pkgdef", path.join("package.capnp").display()))
            .arg(path.join("test.spk")),
    )?;
    let reference = output(Command::new(spk).arg("verify").arg(path.join("test.spk")))?;
    let text = String::from_utf8(reference.stdout)?;
    let text = regex::Regex::new(r#"Base64\(("[A-Za-z0-9+/=]*")\)"#)?.replace_all(&text, "$1");
    let expected: serde_json::Value = serde_json::from_str(&text)?;
    let actual: serde_json::Value = serde_json::from_slice(
        &output(
            Command::new(root.join("target/debug/marketplace-grain"))
                .arg("--verify-json")
                .arg(path.join("test.spk")),
        )?
        .stdout,
    )?;
    for field in [
        "appId",
        "packageId",
        "version",
        "title",
        "authorPgpKeyFingerprint",
    ] {
        ensure!(
            actual[field] == expected[field],
            "SDK/Rust mismatch for {field}: {} vs {}",
            actual[field],
            expected[field]
        );
    }
    println!("Rust and SDK verification agree, including the OpenPGP author fingerprint.");
    run(Command::new("cargo")
        .args([
            "test",
            "-p",
            "marketplace-grain",
            "--test",
            "interoperability",
            "--",
            "--nocapture",
        ])
        .env("SANDSTORM_SPK", spk)
        .env("MARKETPLACE_INTEROP_FIXTURE", path))?;
    // Also verify our Rust-built distribution with the independently implemented SDK.
    let artifact = root.join("build/marketplace.spk");
    if artifact.exists() {
        output(Command::new(spk).arg("verify").arg(artifact))?;
        println!("SDK accepted the Rust-built marketplace SPK.");
    }
    Ok(())
}
