//! Whole-session execution in a Tart guest, without host tools or directory shares.

use serde::Deserialize;

/// Host-global `[tart]` settings.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Settings {
    /// The name of an existing local VM with bhai and the Tart Guest Agent installed.
    pub vm: Option<String>,
}

pub(crate) async fn launch(args: Vec<String>) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use anyhow::Context;
        use std::os::unix::process::CommandExt;

        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let config = crate::config::Config::load_global(home.as_deref())?;
        let vm = vm_name(&config.tart)?;
        let tart = executable()?;
        let args = guest_args(args)?;
        let _boot = ensure_ready(&tart, vm, || verify_running(&tart, vm).map(|_| ())).await?;
        let owner = verify_running(&tart, vm)?;
        check_guest(&tart, vm).await?;
        anyhow::ensure!(
            verify_running(&tart, vm)? == owner,
            "Tart VM changed during attach; no host session was started"
        );
        let tty = {
            use std::io::IsTerminal;
            std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
        };
        let mut command = attach(&tart, vm, &args, tty)?;
        crate::startup::end("tart");
        Err(command.exec()).context("could not attach to Tart guest; no host session was started")
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = args;
        anyhow::bail!("--tart requires macOS with Tart installed; no host session was started")
    }
}

#[cfg(any(target_os = "macos", test))]
fn vm_name(settings: &Settings) -> anyhow::Result<&str> {
    let vm = settings.vm.as_deref().unwrap_or_default();
    anyhow::ensure!(
        !vm.is_empty()
            && !vm.starts_with('-')
            && vm
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
            && vm != "."
            && vm != "..",
        "set [tart] vm = \"VM-NAME\" in ~/.config/bhai/config.toml to an existing local VM"
    );
    Ok(vm)
}

#[cfg(any(target_os = "macos", test))]
fn guest_args(mut args: Vec<String>) -> anyhow::Result<Vec<String>> {
    let end = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    let mut tail = args.split_off(end);
    let before = args.len();
    args.retain(|arg| arg != "--tart");
    anyhow::ensure!(args.len() < before, "missing --tart launcher flag");
    args.append(&mut tail);
    Ok(args)
}

#[cfg(any(target_os = "macos", test))]
mod host {
    use super::*;
    use anyhow::{Context, Result, bail, ensure};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::Duration;
    use tokio::time::{Instant, sleep, timeout};

    const BOOT_TIMEOUT: Duration = Duration::from_secs(120);
    const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
    const GUEST_ENV: &str =
        "cd \"$HOME\" || exit; PATH=\"$HOME/.cargo/bin:$HOME/.local/bin:$PATH\"; export PATH;";

    fn command(tart: &Path) -> Result<Command> {
        let mut command = Command::new(tart);
        command.env_clear().env("TART_HOME", storage_root()?);
        for name in ["HOME", "PATH", "TMPDIR", "TERM"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        Ok(command)
    }

    #[cfg(target_os = "macos")]
    pub(super) fn executable() -> Result<PathBuf> {
        use std::os::unix::fs::PermissionsExt;
        for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
            let path = dir.join("tart");
            if let Ok(meta) = path.metadata()
                && meta.is_file()
                && meta.permissions().mode() & 0o111 != 0
            {
                return native_executable(&path);
            }
        }
        bail!("Tart is not on PATH; install it before using --tart")
    }

    fn native_executable(path: &Path) -> Result<PathBuf> {
        use std::io::Read;
        let path = path.canonicalize().context("resolving Tart executable")?;
        let mut file = std::fs::File::open(&path)?;
        let mut magic = [0; 2];
        file.read_exact(&mut magic)?;
        if magic != *b"#!" {
            return Ok(path);
        }
        let mut script = String::from("#!");
        file.take(4096).read_to_string(&mut script)?;
        let prefix = path
            .parent()
            .and_then(Path::parent)
            .context("unknown Tart launcher layout")?;
        let native = prefix.join("libexec/tart.app/Contents/MacOS/tart");
        let expected = format!("#!/bin/bash\nexec \"{}\" \"$@\"", native.display());
        ensure!(
            script.trim_end() == expected,
            "unsupported Tart wrapper {}; put the native Tart executable on PATH",
            path.display()
        );
        native
            .canonicalize()
            .context("resolving Homebrew Tart app executable")
    }

    #[derive(Deserialize)]
    struct Vm {
        #[serde(rename = "Name")]
        name: String,
        #[serde(rename = "Running")]
        running: bool,
    }

    fn running(list: &[u8], name: &str) -> Result<bool> {
        let vms: Vec<Vm> = serde_json::from_slice(list).context("reading Tart VM list")?;
        let vm = vms
            .iter()
            .find(|vm| vm.name == name)
            .with_context(|| format!("local Tart VM {name:?} does not exist; create it first"))?;
        Ok(vm.running)
    }

    async fn output(mut command: Command, limit: Duration) -> Result<std::process::Output> {
        command.stdin(Stdio::null());
        let mut command = tokio::process::Command::from(command);
        command.kill_on_drop(true);
        timeout(limit, command.output())
            .await
            .context("Tart command timed out")?
            .context("running Tart command")
    }

    pub(super) async fn ensure_ready(
        tart: &Path,
        vm: &str,
        verify_existing: impl FnOnce() -> Result<()>,
    ) -> Result<Option<std::process::Child>> {
        let mut list = command(tart)?;
        list.args(["list", "--source", "local", "--format", "json"]);
        let list = output(list, Duration::from_secs(10)).await?;
        ensure!(
            list.status.success(),
            "could not list Tart VMs: {}",
            String::from_utf8_lossy(&list.stderr)
        );
        let mut boot = if running(&list.stdout, vm)? {
            verify_existing()?;
            None
        } else {
            eprintln!("bhai: starting Tart VM {vm} without a display or host shares");
            let mut run = command(tart)?;
            run.args(["run", vm, "--no-graphics", "--no-clipboard", "--no-audio"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            use std::os::unix::process::CommandExt;
            run.process_group(0);
            Some(run.spawn().context("starting Tart VM")?)
        };
        let deadline = Instant::now() + BOOT_TIMEOUT;
        loop {
            if let Some(boot) = &mut boot
                && let Some(status) = boot.try_wait().context("checking Tart startup")?
            {
                bail!(
                    "Tart VM {vm} exited during startup ({status}); run tart run manually to diagnose"
                );
            }
            let mut probe = command(tart)?;
            probe.args(["exec", vm, "/bin/sh", "-c", "true"]);
            if output(probe, PROBE_TIMEOUT)
                .await
                .is_ok_and(|out| out.status.success())
            {
                return Ok(boot);
            }
            ensure!(
                Instant::now() < deadline,
                "Tart VM {vm} did not become ready within 120s; install/start the Tart Guest Agent; no host session was started"
            );
            sleep(Duration::from_millis(500)).await;
        }
    }

    pub(super) async fn check_guest(tart: &Path, vm: &str) -> Result<()> {
        let mut check = command(tart)?;
        check.args([
            "exec",
            vm,
            "/bin/sh",
            "-lc",
            &format!("{GUEST_ENV} command -v bhai >/dev/null"),
        ]);
        let out = output(check, Duration::from_secs(10)).await?;
        ensure!(
            out.status.success(),
            "install bhai in VM {vm} (on PATH or in ~/.cargo/bin), and configure model authentication there; no host credentials are copied and no host session was started"
        );
        Ok(())
    }

    pub(super) fn attach(tart: &Path, vm: &str, args: &[String], tty: bool) -> Result<Command> {
        let mut command = command(tart)?;
        command.args(["exec", "-i"]);
        if tty {
            command.arg("-t");
        }
        command.args([
            vm,
            "/bin/sh",
            "-lc",
            &format!(
                "{GUEST_ENV} TERM=\"${{TERM:-xterm-256color}}\"; export TERM; exec bhai \"$@\""
            ),
            "bhai-tart",
        ]);
        command.args(args);
        Ok(command)
    }

    /// Only a known, share-free Tart invocation may back a guest-only session.
    fn safe_run(args: &[String], vm: &str) -> bool {
        args.first()
            .is_some_and(|p| Path::new(p).file_name().is_some_and(|p| p == "tart"))
            && args.get(1).is_some_and(|a| a == "run")
            && args[2..].iter().filter(|a| a.as_str() == vm).count() == 1
            && args[2..].iter().any(|a| a == "--no-clipboard")
            && args[2..].iter().all(|a| {
                a == vm
                    || matches!(
                        a.as_str(),
                        "--no-graphics" | "--no-clipboard" | "--no-audio"
                    )
            })
    }

    fn storage_root() -> Result<PathBuf> {
        let root = match std::env::var_os("TART_HOME") {
            Some(root) => PathBuf::from(root),
            None => {
                PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".tart")
            }
        };
        Ok(std::path::absolute(root)?)
    }

    #[cfg(target_os = "macos")]
    pub(super) fn verify_running(tart: &Path, vm: &str) -> Result<libc::pid_t> {
        let config = storage_root()?.join("vms").join(vm).join("config.json");
        let file = std::fs::File::open(config)
            .context("opening Tart VM configuration for isolation verification")?;
        let pid = lock_owner(&file)?;
        ensure!(
            pid > 0,
            "Tart VM stopped before attach; no host session was started"
        );
        let args = process_args(pid)?;
        ensure!(
            process_path(pid)?.canonicalize()? == tart.canonicalize()? && safe_run(&args, vm),
            "cannot verify that VM {vm} has no host shares, disks or clipboard; stop it and let bhai --tart start it, or use tart run {vm} --no-graphics --no-clipboard --no-audio"
        );
        Ok(pid)
    }

    #[cfg(target_os = "macos")]
    #[allow(unsafe_code)]
    fn lock_owner(file: &std::fs::File) -> Result<libc::pid_t> {
        use std::os::fd::AsRawFd;
        let mut lock = libc::flock {
            l_start: 0,
            l_len: 0,
            l_pid: 0,
            l_type: libc::F_RDLCK,
            l_whence: libc::SEEK_SET as _,
        };
        // F_GETLK only queries Tart's config.json PID lock; it does not acquire it.
        let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut lock) };
        ensure!(
            result != -1,
            "reading Tart VM owner: {}",
            std::io::Error::last_os_error()
        );
        Ok(lock.l_pid)
    }

    #[cfg(target_os = "macos")]
    #[allow(unsafe_code)]
    fn process_path(pid: libc::pid_t) -> Result<PathBuf> {
        use std::os::unix::ffi::OsStrExt;
        let mut path = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // proc_pidpath writes a NUL-terminated path within the supplied buffer.
        let result = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as _) };
        ensure!(
            result > 0,
            "reading Tart executable path: {}",
            std::io::Error::last_os_error()
        );
        let end = path
            .iter()
            .position(|b| *b == 0)
            .context("unterminated Tart executable path")?;
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&path[..end])))
    }

    #[cfg(target_os = "macos")]
    #[allow(unsafe_code)]
    fn process_args(pid: libc::c_int) -> Result<Vec<String>> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut bytes = vec![0u8; 1024 * 1024];
        let mut len = bytes.len();
        // sysctl writes at most `len` bytes into the allocated buffer.
        let result = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as _,
                bytes.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        ensure!(
            result == 0,
            "reading Tart process arguments: {}",
            std::io::Error::last_os_error()
        );
        bytes.truncate(len);
        decode_args(&bytes)
    }

    fn decode_args(bytes: &[u8]) -> Result<Vec<String>> {
        let size = std::mem::size_of::<i32>();
        ensure!(bytes.len() >= size, "short process arguments");
        let count = i32::from_ne_bytes(bytes[..size].try_into()?);
        ensure!(
            count > 0 && count < 100_000,
            "invalid process argument count"
        );
        let bytes = &bytes[size..];
        let end = bytes
            .iter()
            .position(|c| *c == 0)
            .context("missing process path")?;
        let bytes = &bytes[end..];
        let start = bytes
            .iter()
            .position(|c| *c != 0)
            .context("missing process argv")?;
        let mut parts = bytes[start..].split(|c| *c == 0);
        let mut args = Vec::new();
        for _ in 0..count {
            let part = parts.next().context("incomplete process argv")?;
            args.push(std::str::from_utf8(part)?.to_owned());
        }
        Ok(args)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        struct FakeTart {
            dir: PathBuf,
            path: PathBuf,
        }

        impl FakeTart {
            fn new(running: bool) -> Self {
                use std::os::unix::fs::PermissionsExt;
                let dir = std::env::temp_dir().join(format!("bhai-tart-{}", uuid::Uuid::new_v4()));
                std::fs::create_dir_all(&dir).unwrap();
                let path = dir.join("tart");
                let script = format!(
                    r#"#!/bin/sh
root="${{0%/*}}"
case "$1" in
list) printf '%s\n' '[{{"Name":"vm","Running":{running}}}]' ;;
run) printf '%s\n' "$@" > "$root/run-args"; touch "$root/ready"; exec /bin/sleep 60 ;;
exec) touch "$root/probed"; [ -f "$root/ready" ] && [ ! -f "$root/missing" ] ;;
*) exit 99 ;;
esac
"#
                );
                std::fs::write(&path, script).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
                if running {
                    std::fs::write(dir.join("ready"), "").unwrap();
                }
                Self { dir, path }
            }
        }

        impl Drop for FakeTart {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.dir).unwrap();
            }
        }

        #[tokio::test]
        async fn stopped_vm_starts_headlessly_without_shares() {
            let fake = FakeTart::new(false);
            let mut boot = ensure_ready(&fake.path, "vm", || Ok(()))
                .await
                .unwrap()
                .unwrap();
            let args = std::fs::read_to_string(fake.dir.join("run-args")).unwrap();
            boot.kill().unwrap();
            boot.wait().unwrap();
            assert_eq!(args, "run\nvm\n--no-graphics\n--no-clipboard\n--no-audio\n");
        }

        #[tokio::test]
        async fn running_vm_is_reused_and_missing_guest_bhai_fails_closed() {
            let fake = FakeTart::new(true);
            assert!(
                ensure_ready(&fake.path, "vm", || Ok(()))
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(!fake.dir.join("run-args").exists());
            check_guest(&fake.path, "vm").await.unwrap();
            std::fs::write(fake.dir.join("missing"), "").unwrap();
            assert!(
                check_guest(&fake.path, "vm")
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("install bhai")
            );
        }

        #[tokio::test]
        async fn unsafe_running_vm_is_rejected_before_any_guest_command() {
            let fake = FakeTart::new(true);
            let error = ensure_ready(&fake.path, "vm", || anyhow::bail!("host share"))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("host share"));
            assert!(!fake.dir.join("probed").exists());
            assert!(!fake.dir.join("run-args").exists());
        }

        #[tokio::test]
        async fn unknown_vm_does_not_start_anything() {
            let fake = FakeTart::new(false);
            assert!(
                ensure_ready(&fake.path, "missing", || Ok(()))
                    .await
                    .is_err()
            );
            assert!(!fake.dir.join("run-args").exists());
        }

        #[tokio::test]
        async fn probes_have_a_timeout() {
            let mut command = Command::new("/bin/sleep");
            command.arg("60");
            assert!(output(command, Duration::from_millis(20)).await.is_err());
        }

        #[test]
        fn homebrew_wrapper_resolves_to_the_native_app_executable() {
            let fake = FakeTart::new(false);
            let wrapper = fake.dir.join("bin/tart");
            let native = fake.dir.join("libexec/tart.app/Contents/MacOS/tart");
            std::fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
            std::fs::create_dir_all(native.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(std::env::current_exe().unwrap(), &native).unwrap();
            std::fs::write(
                &wrapper,
                format!("#!/bin/bash\nexec \"{}\" \"$@\"\n", native.display()),
            )
            .unwrap();
            assert_eq!(
                native_executable(&wrapper).unwrap(),
                std::env::current_exe().unwrap().canonicalize().unwrap()
            );
            assert!(native_executable(&fake.path).is_err());
        }

        #[test]
        fn list_selects_only_the_named_vm() {
            let list = br#"[{"Name":"one","Running":false},{"Name":"two","Running":true}]"#;
            assert!(!running(list, "one").unwrap());
            assert!(running(list, "two").unwrap());
            assert!(running(list, "missing").is_err());
            assert!(running(b"not json", "one").is_err());
        }

        #[test]
        fn existing_launch_must_have_no_host_integration() {
            let args = [
                "/opt/homebrew/bin/tart",
                "run",
                "vm",
                "--no-graphics",
                "--no-clipboard",
                "--no-audio",
            ];
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            assert!(safe_run(&args, "vm"));
            assert!(!safe_run(&args, "other"));
            assert!(!safe_run(&args[..4], "vm"));
            for flag in [
                "--dir=/tmp",
                "--dir",
                "--disk=/dev/disk1",
                "--rosetta=host",
                "--vnc",
                "--unknown",
            ] {
                let mut unsafe_args = args.clone();
                unsafe_args.push(flag.into());
                assert!(!safe_run(&unsafe_args, "vm"), "{flag}");
            }
        }

        #[test]
        fn attach_preserves_guest_arguments_without_shell_interpolation() {
            let args = vec![
                "exec".into(),
                "$(touch /host); 'quoted' --tart".into(),
                "-C".into(),
                "/guest/project".into(),
            ];
            let command = attach(Path::new("/usr/local/bin/tart"), "vm", &args, false).unwrap();
            let actual: Vec<_> = command.get_args().map(|a| a.to_str().unwrap()).collect();
            assert_eq!(&actual[..4], &["exec", "-i", "vm", "/bin/sh"]);
            assert_eq!(&actual[actual.len() - args.len()..], args);
            assert!(actual[5].contains("cd \"$HOME\""));
            assert!(actual[5].contains("exec bhai \"$@\""));
            assert!(!actual[5].contains("touch /host"));
            let tty = attach(Path::new("/tart"), "vm", &[], true).unwrap();
            assert!(tty.get_args().any(|a| a == "-t"));
        }

        #[test]
        fn process_argument_decoding_excludes_environment() {
            let mut bytes = 4i32.to_ne_bytes().to_vec();
            bytes.extend_from_slice(
                b"/opt/homebrew/bin/tart\0\0tart\0run\0vm\0--no-clipboard\0SECRET=hidden\0",
            );
            assert_eq!(
                decode_args(&bytes).unwrap(),
                ["tart", "run", "vm", "--no-clipboard"]
            );
            assert!(decode_args(b"short").is_err());
        }

        #[cfg(target_os = "macos")]
        #[test]
        fn reads_native_process_arguments() {
            let args = process_args(std::process::id() as _).unwrap();
            assert!(!args.is_empty());
            assert_eq!(
                process_path(std::process::id() as _)
                    .unwrap()
                    .canonicalize()
                    .unwrap(),
                std::env::current_exe().unwrap().canonicalize().unwrap()
            );
        }

        #[cfg(target_os = "macos")]
        #[test]
        #[allow(unsafe_code)]
        fn lock_fixture() {
            use std::os::fd::AsRawFd;
            let Some(path) = std::env::var_os("BHAI_TEST_TART_LOCK") else {
                return;
            };
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .unwrap();
            let mut lock = libc::flock {
                l_start: 0,
                l_len: 0,
                l_pid: 0,
                l_type: libc::F_WRLCK,
                l_whence: libc::SEEK_SET as _,
            };
            // The fixture owns this descriptor for the duration of the child process.
            assert_eq!(
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &mut lock) },
                0
            );
            std::thread::sleep(Duration::from_secs(30));
        }

        #[cfg(target_os = "macos")]
        #[test]
        fn vm_owner_is_bound_to_the_exact_config_not_just_the_vm_name() {
            let fake = FakeTart::new(false);
            let one = fake.dir.join("config-one.json");
            let two = fake.dir.join("config-two.json");
            std::fs::write(&one, "{}").unwrap();
            std::fs::write(&two, "{}").unwrap();
            let one_file = std::fs::File::open(&one).unwrap();
            let two_file = std::fs::File::open(&two).unwrap();
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "tart::host::tests::lock_fixture"])
                .env("BHAI_TEST_TART_LOCK", &one)
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            let owner = loop {
                let owner = lock_owner(&one_file).unwrap();
                if owner != 0 || Instant::now() >= deadline {
                    break owner;
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            let other_owner = lock_owner(&two_file).unwrap();
            child.kill().unwrap();
            child.wait().unwrap();
            assert_eq!(owner, child.id() as libc::pid_t);
            assert_eq!(other_owner, 0);
        }
    }
}

#[cfg(target_os = "macos")]
use host::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_name_must_be_a_local_name_not_an_option_or_path() {
        for vm in [
            None,
            Some(""),
            Some("--dir=/"),
            Some("../vm"),
            Some("vm name"),
            Some("."),
            Some(".."),
            Some("a\nb"),
        ] {
            assert!(
                vm_name(&Settings {
                    vm: vm.map(str::to_owned)
                })
                .is_err()
            );
        }
        assert_eq!(
            vm_name(&Settings {
                vm: Some("ubuntu-24.04-bhai".into())
            })
            .unwrap(),
            "ubuntu-24.04-bhai"
        );
    }

    #[test]
    fn only_the_launcher_flag_is_removed() {
        let args = ["--tart", "exec", "--", "--tart"]
            .map(str::to_owned)
            .to_vec();
        assert_eq!(guest_args(args).unwrap(), ["exec", "--", "--tart"]);
        assert!(guest_args(Vec::new()).is_err());
        assert!(guest_args(["exec", "--", "--tart"].map(str::to_owned).to_vec()).is_err());
        let args = ["--tart", "exec", "review", "--tart"]
            .map(str::to_owned)
            .to_vec();
        assert_eq!(guest_args(args).unwrap(), ["exec", "review"]);
    }
}
