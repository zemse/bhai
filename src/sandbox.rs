//! The OS sandbox `bash -lc` runs in once `[bash] sandbox = true`: writes only under the
//! project root, the temp dirs, `/dev` and the toolchain caches, and with `network =
//! false` no IP traffic. Seatbelt (`sandbox-exec`) on macOS, Landlock on Linux. Reads are
//! never confined. A command that cannot be sandboxed does not run.
//!
//! On macOS the profile only denies; everything else is allowed, so a command can still
//! ask a running service (over a unix socket or Mach) to write for it. On Linux the
//! network rule covers TCP only, so UDP, DNS included, still goes out.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tokio::process::Command;

/// `[bash]` as the config reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// `sandbox`.
    pub on: bool,
    /// `network`: `false` also turns the sandbox on, since nothing else can enforce it.
    pub network: bool,
    /// `writable`: more directories commands may write under, `~/` allowed.
    pub writable: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            on: false,
            network: true,
            writable: Vec::new(),
        }
    }
}

impl Settings {
    pub fn active(&self) -> bool {
        self.on || !self.network
    }
}

/// Toolchain caches under the home directory. Not their `bin` directories, which are on
/// `PATH`: a write there would run unsandboxed in the user's own shell.
const CACHES: [&str; 11] = [
    ".cargo/registry",
    ".cargo/git",
    ".npm",
    ".cache",
    ".bun/install/cache",
    ".yarn/berry/cache",
    ".gradle/caches",
    ".m2/repository",
    "go/pkg/mod",
    "Library/Caches",
    ".local/share/pnpm/store",
];

/// Variables that name a cache or build directory elsewhere.
const CACHE_VARS: [&str; 5] = [
    "CARGO_TARGET_DIR",
    "GOCACHE",
    "GOMODCACHE",
    "XDG_CACHE_HOME",
    "npm_config_cache",
];

static SANDBOX: OnceLock<Sandbox> = OnceLock::new();

/// Named once for the process, like `pass_env`: every bash call after this is sandboxed.
pub fn set(settings: &Settings, root: &Path, home: Option<&Path>) {
    if settings.active() {
        let _ = SANDBOX.set(Sandbox::new(settings, root, home));
    }
}

pub fn active() -> Option<&'static Sandbox> {
    SANDBOX.get()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sandbox {
    /// Resolved through symlinks, since both kernels check the real path.
    writable: Vec<PathBuf>,
    network: bool,
}

/// A command ready for `-lc` and its script, and what has to live until it is spawned.
pub struct Shell {
    pub command: Command,
    #[cfg(target_os = "linux")]
    _ruleset: std::os::fd::OwnedFd,
}

impl Sandbox {
    pub fn new(settings: &Settings, root: &Path, home: Option<&Path>) -> Self {
        let mut dirs = vec![
            root.to_path_buf(),
            std::env::temp_dir(),
            PathBuf::from("/tmp"),
            PathBuf::from("/var/tmp"),
            PathBuf::from("/dev"),
        ];
        if let Some(home) = home {
            dirs.extend(CACHES.iter().map(|dir| home.join(dir)));
        }
        dirs.extend(
            CACHE_VARS
                .iter()
                .filter_map(std::env::var_os)
                .map(PathBuf::from)
                .filter(|dir| dir.is_absolute()),
        );
        dirs.extend(
            settings
                .writable
                .iter()
                .filter_map(|dir| match dir.strip_prefix("~/") {
                    Some(rest) => home.map(|home| home.join(rest)),
                    None => Some(PathBuf::from(dir)).filter(|dir| dir.is_absolute()),
                }),
        );
        Self::with(dirs, settings.network)
    }

    pub(crate) fn with(dirs: Vec<PathBuf>, network: bool) -> Self {
        let mut writable = Vec::new();
        for dir in dirs.iter().map(|dir| resolve(dir)) {
            if !writable.contains(&dir) {
                writable.push(dir);
            }
        }
        Self { writable, network }
    }

    /// What the model is told, so a refused write reads as the sandbox and not a fault.
    pub fn describe(&self) -> &'static str {
        match self.network {
            true => {
                " Commands run in a sandbox: writes outside the working directory, the temp \
    dirs and the toolchain caches fail with \"Operation not permitted\"."
            }
            false => {
                " Commands run in a sandbox: writes outside the working directory, the temp \
    dirs and the toolchain caches fail with \"Operation not permitted\", and there is no \
    network."
            }
        }
    }

    #[cfg(target_os = "macos")]
    pub fn bash(&self) -> io::Result<Shell> {
        let mut command = Command::new("/usr/bin/sandbox-exec");
        command.arg("-p").arg(self.profile());
        for (i, dir) in self.writable.iter().enumerate() {
            let mut param = std::ffi::OsString::from(format!("W{i}="));
            param.push(dir);
            command.arg("-D").arg(param);
        }
        command.arg("bash");
        Ok(Shell { command })
    }

    /// Paths go in as parameters, so none has to be quoted into the profile.
    #[cfg(target_os = "macos")]
    fn profile(&self) -> String {
        let roots: String = (0..self.writable.len())
            .map(|i| format!(" (subpath (param \"W{i}\"))"))
            .collect();
        let mut profile = format!(
            "(version 1)\n(allow default)\n(deny file-write* (require-not (require-any{roots})))\n"
        );
        if !self.network {
            profile
                .push_str("(deny network-outbound (remote ip))\n(deny network-bind (local ip))\n");
        }
        profile
    }

    #[cfg(target_os = "linux")]
    #[allow(unsafe_code)]
    pub fn bash(&self) -> io::Result<Shell> {
        use std::os::fd::AsRawFd;
        let ruleset = landlock::ruleset(&self.writable, self.network)?;
        let fd = ruleset.as_raw_fd();
        let mut command = Command::new("bash");
        // SAFETY: the closure makes two syscalls and allocates nothing, so it is safe
        // between fork and exec; `fd` stays open until the `Shell` is dropped.
        unsafe {
            command.pre_exec(move || landlock::restrict(fd));
        }
        Ok(Shell {
            command,
            _ruleset: ruleset,
        })
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    pub fn bash(&self) -> io::Result<Shell> {
        Err(io::Error::other("bhai has no sandbox on this platform"))
    }
}

/// `path` through its symlinks. A directory that does not exist yet is taken under its
/// nearest existing parent, resolved, so a cache made later is still covered.
fn resolve(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => resolve(parent).join(name),
        _ => path.to_path_buf(),
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod landlock {
    use std::fs::OpenOptions;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::PathBuf;

    const CREATE_RULESET_VERSION: u32 = 1;
    const RULE_PATH_BENEATH: u32 = 1;

    const WRITE_FILE: u64 = 1 << 1;
    const REMOVE_DIR: u64 = 1 << 4;
    const REMOVE_FILE: u64 = 1 << 5;
    const MAKE_CHAR: u64 = 1 << 6;
    const MAKE_DIR: u64 = 1 << 7;
    const MAKE_REG: u64 = 1 << 8;
    const MAKE_SOCK: u64 = 1 << 9;
    const MAKE_FIFO: u64 = 1 << 10;
    const MAKE_BLOCK: u64 = 1 << 11;
    const MAKE_SYM: u64 = 1 << 12;
    /// ABI 2.
    const REFER: u64 = 1 << 13;
    /// ABI 3.
    const TRUNCATE: u64 = 1 << 14;
    /// ABI 4.
    const BIND_TCP: u64 = 1 << 0;
    const CONNECT_TCP: u64 = 1 << 1;

    /// Write rights only: read and execute are left unhandled, so they stay allowed.
    const WRITES: u64 = WRITE_FILE
        | REMOVE_DIR
        | REMOVE_FILE
        | MAKE_CHAR
        | MAKE_DIR
        | MAKE_REG
        | MAKE_SOCK
        | MAKE_FIFO
        | MAKE_BLOCK
        | MAKE_SYM;

    /// An older kernel takes the 16 bytes as long as what it does not know is zero.
    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
    }

    #[repr(C, packed)]
    struct PathBeneathAttr {
        allowed_access: u64,
        parent_fd: i32,
    }

    /// A ruleset that allows writes under `writable` alone, and with `network` false no
    /// TCP bind or connect. Built in the parent, where allocating is fine.
    pub fn ruleset(writable: &[PathBuf], network: bool) -> io::Result<OwnedFd> {
        // SAFETY: a version query reads no memory.
        let abi = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<RulesetAttr>(),
                0usize,
                CREATE_RULESET_VERSION,
            )
        };
        if abi < 1 {
            return Err(io::Error::other(
                "the sandbox needs Landlock, which this kernel does not offer",
            ));
        }
        if !network && abi < 4 {
            return Err(io::Error::other(
                "`network = false` needs Landlock ABI 4 (Linux 6.7); this kernel has an older one",
            ));
        }
        let mut access = WRITES;
        if abi >= 2 {
            access |= REFER;
        }
        if abi >= 3 {
            access |= TRUNCATE;
        }
        let attr = RulesetAttr {
            handled_access_fs: access,
            handled_access_net: if network { 0 } else { BIND_TCP | CONNECT_TCP },
        };
        // SAFETY: `attr` is a live struct of the size passed.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const RulesetAttr,
                std::mem::size_of::<RulesetAttr>(),
                0u32,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the kernel just handed over this descriptor.
        let ruleset = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
        for dir in writable {
            let Ok(file) = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
                .open(dir)
            else {
                continue;
            };
            let rule = PathBeneathAttr {
                allowed_access: access,
                parent_fd: file.as_raw_fd(),
            };
            // SAFETY: `rule` is live and `file` is open for the call.
            let added = unsafe {
                libc::syscall(
                    libc::SYS_landlock_add_rule,
                    ruleset.as_raw_fd(),
                    RULE_PATH_BENEATH,
                    &rule as *const PathBeneathAttr,
                    0u32,
                )
            };
            if added < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(ruleset)
    }

    /// Between fork and exec: syscalls only.
    pub fn restrict(ruleset: RawFd) -> io::Result<()> {
        // SAFETY: neither call touches memory.
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0u32) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_false_turns_the_sandbox_on() {
        assert!(!Settings::default().active());
        let off = Settings {
            network: false,
            ..Settings::default()
        };
        assert!(off.active());
    }

    #[test]
    fn writable_dirs_are_the_root_temp_and_caches_not_bin() {
        let dir = crate::tools::temp_dir();
        let (root, home) = (dir.join("root"), dir.join("home"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(home.join(".cargo/bin")).unwrap();
        let settings = Settings {
            on: true,
            writable: vec!["~/extra".to_string(), "relative".to_string()],
            ..Settings::default()
        };
        let sandbox = Sandbox::new(&settings, &root, Some(&home));
        let real = dir.canonicalize().unwrap();
        let has = |path: &Path| sandbox.writable.contains(&path.to_path_buf());
        assert!(has(&real.join("root")));
        assert!(has(&resolve(&std::env::temp_dir())));
        // Missing yet, so taken under the resolved home.
        assert!(has(&real.join("home/.cargo/registry")));
        assert!(has(&real.join("home/extra")));
        assert!(!has(&real.join("home/.cargo")));
        assert!(!has(&real.join("home/.cargo/bin")));
        assert!(!sandbox.writable.iter().any(|d| d.ends_with("relative")));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
