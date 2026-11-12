//! Builds the `.rpm` and `.deb` in `target/dist`, from one binary compiled in a container whose old
//! glibc lets it run on other distros. See `packaging/`.
//!
//! The source tree goes into the container as a tar on stdin and the packages come out as a tar on
//! stdout. Nothing is mounted, so the container cannot touch the tree or leave root-owned files.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};

const IMAGE: &str = "localhost/rmx-package-build:1";

/// Unpack the source, build, and pack the packages onto stdout, which is why the log goes to stderr.
const IN_CONTAINER: &str = "mkdir /src && tar -x --no-same-owner -C /src && cd /src \
    && bash packaging/build.sh >&2 && cd /out && tar -c *";

pub fn package() -> Result<(), String> {
    let root =
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().ok_or("xtask has no parent directory")?;
    let dist = root.join("target/dist");
    fs::create_dir_all(&dist).map_err(|err| format!("could not make {}: {err}", dist.display()))?;

    let engine = ["podman", "docker"]
        .into_iter()
        .find(|engine| {
            let check = Command::new(engine).arg("--version").stdout(Stdio::null()).status();
            check.is_ok_and(|status| status.success())
        })
        .ok_or("package needs podman or docker")?;

    let built = Command::new(engine)
        .args(["build", "-t", IMAGE, "-f", "packaging/Containerfile", "packaging"])
        .current_dir(root)
        .status();
    check("building the image", built)?;

    let files = source_files(root)?;
    let mut pack = spawn(
        Command::new("tar")
            .args(["-c", "--null", "-T", "-"])
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped()),
    )?;
    let source = pack.stdout.take().ok_or("tar has no stdout")?;
    let mut build = spawn(
        Command::new(engine)
            // Passing just the name hands on the host's value, if it has one.
            .args(["run", "--rm", "-i", "--env", "CARGO_BUILD_JOBS", IMAGE, "sh", "-c", IN_CONTAINER])
            .stdin(source)
            .stdout(Stdio::piped()),
    )?;
    let packages = build.stdout.take().ok_or("the container has no stdout")?;
    let unpack = spawn(
        Command::new("tar")
            .args(["-xv", "--no-same-owner", "-C"])
            .arg(&dist)
            .stdin(packages)
            .stdout(Stdio::piped()),
    )?;

    let mut list = pack.stdin.take().ok_or("tar has no stdin")?;
    list.write_all(&files).map_err(|err| format!("could not list the source for tar: {err}"))?;
    drop(list);

    check("packing the source", pack.wait())?;
    check("the container build", build.wait())?;
    let unpacked =
        unpack.wait_with_output().map_err(|err| format!("unpacking the packages: {err}"))?;
    check("unpacking the packages", Ok(unpacked.status))?;
    for name in String::from_utf8_lossy(&unpacked.stdout).lines() {
        println!("{}", dist.join(name).display());
    }
    Ok(())
}

/// The working tree as it stands, NUL-separated: tracked files, minus any deleted, and new ones.
fn source_files(root: &Path) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        .current_dir(root)
        .output()
        .map_err(|err| format!("could not run git: {err}"))?;
    if !output.status.success() {
        return Err(format!("git ls-files: {}", String::from_utf8_lossy(&output.stderr)));
    }
    let mut files = Vec::with_capacity(output.stdout.len());
    for path in output.stdout.split(|byte| *byte == 0).filter(|path| !path.is_empty()) {
        let name =
            std::str::from_utf8(path).map_err(|err| format!("source path is not UTF-8: {err}"))?;
        if root.join(name).symlink_metadata().is_ok() {
            files.extend_from_slice(path);
            files.push(0);
        }
    }
    Ok(files)
}

fn spawn(command: &mut Command) -> Result<Child, String> {
    command.spawn().map_err(|err| format!("could not start {:?}: {err}", command.get_program()))
}

fn check(what: &str, status: std::io::Result<ExitStatus>) -> Result<(), String> {
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("{what} failed: {status}")),
        Err(err) => Err(format!("{what} failed: {err}")),
    }
}
