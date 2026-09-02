//! `installation` — install a built binary the honest way.
//!
//! The order of operations is the whole point:
//!
//! 1. [`build_binary`] asks cargo where the artifact really is (workspaces,
//!    `CARGO_TARGET_DIR` and Windows `.exe` included) instead of guessing
//!    `target/release/<name>`.
//! 2. [`Installer::plan`] decides every path up front and returns an
//!    [`InstallPlan`] you can print, test, or refuse. Nothing is written.
//! 3. [`Installer::install`] carries the plan out: copy to a temporary file
//!    next to the destination, verify the SHA-256, rename into place, mark
//!    executable, write an [`InstallReceipt`].
//! 4. [`Installer::uninstall`] reads the receipt and removes exactly what it
//!    installed, refusing to touch a binary that changed since.
//!
//! Paths come from [`pathgen`]: user installs live under the XDG user
//! directories, system installs under the FHS roles, and both can be rooted
//! elsewhere (`Scope::System { root }`) for staging trees and tests. Every
//! join goes through `pathgen::checked_join`, so a binary named `../x` cannot
//! escape the install directory.

use pathgen::{checked_join, AppPathRole, PathPolicy, PathgenError, StandardPath};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, BufRead, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// Everything that can go wrong; no panics on user input.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("cargo build failed with {status} for `{manifest}`")]
    CargoBuild { manifest: PathBuf, status: String },
    #[error("cargo could not be started: {0}")]
    CargoSpawn(io::Error),
    #[error("cargo built `{manifest}` but produced no executable named `{bin}`")]
    NoArtifact { manifest: PathBuf, bin: String },
    #[error("no home directory: cannot place a user install")]
    NoHome,
    #[error("invalid binary name `{0}`: one path component, no separators")]
    BadBinName(String),
    #[error("source `{0}` is not a file")]
    SourceNotFile(PathBuf),
    #[error("`{path}` exists and is not the binary this receipt installed (expected sha256 {expected}, found {found})")]
    Changed { path: PathBuf, expected: String, found: String },
    #[error("copy verification failed for `{path}`: expected sha256 {expected}, found {found}")]
    CopyMismatch { path: PathBuf, expected: String, found: String },
    #[error("receipt `{0}` is not readable as an install receipt: {1}")]
    BadReceipt(PathBuf, String),
    #[error("path policy: {0}")]
    Path(#[from] PathgenError),
    #[error("io error at `{path}`: {source}")]
    Io { path: PathBuf, #[source] source: io::Error },
}

fn io_at(path: &Path) -> impl FnOnce(io::Error) -> InstallError + '_ {
    move |source| InstallError::Io { path: path.to_path_buf(), source }
}

/// Where an install goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// The current user's binaries and state: `$XDG_BIN_HOME` or
    /// `~/.local/bin`, receipts under `$XDG_STATE_HOME/installation` or
    /// `~/.local/state/installation`. No privileges needed.
    User,
    /// The machine: `/usr/local/bin`, receipts in the app's `pathgen` state
    /// directory (`/var/lib/<app>/install`). `root` re-roots every path
    /// (`/srv/rootfs`, a container image, a test directory).
    System { root: Option<PathBuf> },
}

/// A resolved install: every path decided, nothing written yet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstallPlan {
    pub app: String,
    pub bin_name: String,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub receipt: PathBuf,
    pub sha256: String,
    /// `pathgen` audit lines for the directories the plan relies on.
    pub audit: Vec<String>,
}

impl InstallPlan {
    /// One line per decision, for `--dry-run` output.
    pub fn describe(&self) -> String {
        let mut s = format!(
            "install {}\n  from    {}\n  to      {}\n  receipt {}\n  sha256  {}\n",
            self.bin_name,
            self.source.display(),
            self.destination.display(),
            self.receipt.display(),
            self.sha256
        );
        for a in &self.audit {
            s.push_str("  policy  ");
            s.push_str(a);
            s.push('\n');
        }
        s
    }
}

/// What an install did; written as JSON next to the app's state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstallReceipt {
    pub version: u32,
    pub app: String,
    pub bin_name: String,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub sha256: String,
    pub installed_at_unix: u64,
}

pub const RECEIPT_VERSION: u32 = 1;

/// The outcome of [`Installer::ensure_path_hook`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathHook {
    /// The directory is already on `PATH` in this process; nothing written.
    AlreadyOnPath,
    /// The marker block already exists in the rc file; nothing written.
    Present { rc_file: PathBuf },
    /// The marker block was appended.
    Added { rc_file: PathBuf },
}

/// Plans and performs installs for one app.
#[derive(Debug, Clone)]
pub struct Installer {
    app: String,
    scope: Scope,
    bin_dir: PathBuf,
    state_dir: PathBuf,
    policy: Option<PathPolicy>,
}

impl Installer {
    /// An installer for `app` in `scope`. The app name follows `pathgen`'s
    /// rules (lowercase, digits, `-`, `_`; no separators).
    pub fn new(app: impl Into<String>, scope: Scope) -> Result<Self, InstallError> {
        let app = app.into();
        pathgen::validate_app_name(&app)?;
        match &scope {
            Scope::User => {
                let home = dirs::home_dir().ok_or(InstallError::NoHome)?;
                let bin_dir = std::env::var_os("XDG_BIN_HOME")
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .unwrap_or_else(|| home.join(".local").join("bin"));
                let state_home = dirs::state_dir().unwrap_or_else(|| home.join(".local").join("state"));
                let state_dir = checked_join(&state_home, "installation")?;
                Ok(Self { app, scope, bin_dir, state_dir, policy: None })
            }
            Scope::System { root } => {
                let mut policy = PathPolicy::prod(&app)?;
                if let Some(r) = root {
                    policy = policy.with_root(r);
                }
                let usr = match root {
                    Some(r) => StandardPath::Usr.under_root(r),
                    None => StandardPath::Usr.as_path().to_path_buf(),
                };
                let bin_dir = checked_join(&usr, "local/bin")?;
                let state = policy.dir(AppPathRole::State)?;
                let state_dir = checked_join(&state.path, "install")?;
                Ok(Self { app, scope, bin_dir, state_dir, policy: Some(policy) })
            }
        }
    }

    /// A user-scope installer with explicit directories (tests, sandboxes).
    pub fn user_in(app: impl Into<String>, bin_dir: impl Into<PathBuf>, state_dir: impl Into<PathBuf>) -> Result<Self, InstallError> {
        let app = app.into();
        pathgen::validate_app_name(&app)?;
        Ok(Self { app, scope: Scope::User, bin_dir: bin_dir.into(), state_dir: state_dir.into(), policy: None })
    }

    pub fn app(&self) -> &str {
        &self.app
    }
    pub fn scope(&self) -> &Scope {
        &self.scope
    }
    /// Where binaries go.
    pub fn bin_dir(&self) -> &Path {
        &self.bin_dir
    }
    /// Where receipts go.
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Decide every path for installing `source` as `bin_name`. Reads the
    /// source to digest it; writes nothing.
    pub fn plan(&self, bin_name: &str, source: &Path) -> Result<InstallPlan, InstallError> {
        validate_bin_name(bin_name)?;
        if !source.is_file() {
            return Err(InstallError::SourceNotFile(source.to_path_buf()));
        }
        let sha256 = sha256_file(source)?;
        let destination = checked_join(&self.bin_dir, bin_name)?;
        let receipt = checked_join(&self.state_dir, format!("{bin_name}.install.json"))?;
        let mut audit = Vec::new();
        if let Some(p) = &self.policy {
            audit.push(p.dir(AppPathRole::State)?.audit_line());
        }
        Ok(InstallPlan { app: self.app.clone(), bin_name: bin_name.to_string(), source: source.to_path_buf(), destination, receipt, sha256, audit })
    }

    /// Carry out a plan: copy next to the destination, verify, rename into
    /// place, mark executable, write the receipt.
    pub fn install(&self, plan: &InstallPlan) -> Result<InstallReceipt, InstallError> {
        let dest_dir = plan.destination.parent().unwrap_or(&self.bin_dir);
        fs::create_dir_all(dest_dir).map_err(io_at(dest_dir))?;
        let tmp = dest_dir.join(format!(".{}.installing", plan.bin_name));
        fs::copy(&plan.source, &tmp).map_err(io_at(&tmp))?;
        let found = sha256_file(&tmp)?;
        if found != plan.sha256 {
            let _ = fs::remove_file(&tmp);
            return Err(InstallError::CopyMismatch { path: plan.destination.clone(), expected: plan.sha256.clone(), found });
        }
        set_executable(&tmp)?;
        fs::rename(&tmp, &plan.destination).map_err(io_at(&plan.destination))?;

        let receipt = InstallReceipt {
            version: RECEIPT_VERSION,
            app: plan.app.clone(),
            bin_name: plan.bin_name.clone(),
            source: plan.source.clone(),
            destination: plan.destination.clone(),
            sha256: plan.sha256.clone(),
            installed_at_unix: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        };
        if let Some(parent) = plan.receipt.parent() {
            fs::create_dir_all(parent).map_err(io_at(parent))?;
        }
        let json = serde_json::to_string_pretty(&receipt).map_err(|e| InstallError::BadReceipt(plan.receipt.clone(), e.to_string()))?;
        fs::write(&plan.receipt, json).map_err(io_at(&plan.receipt))?;
        Ok(receipt)
    }

    /// The receipt for `bin_name`, if one exists.
    pub fn receipt(&self, bin_name: &str) -> Result<Option<InstallReceipt>, InstallError> {
        validate_bin_name(bin_name)?;
        let path = checked_join(&self.state_dir, format!("{bin_name}.install.json"))?;
        if !path.is_file() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path).map_err(io_at(&path))?;
        serde_json::from_str(&text).map(Some).map_err(|e| InstallError::BadReceipt(path, e.to_string()))
    }

    /// Remove what a receipt says was installed. Refuses if the binary's
    /// digest no longer matches the receipt (someone replaced it), unless
    /// `force`. Returns `None` when there is no receipt.
    pub fn uninstall(&self, bin_name: &str, force: bool) -> Result<Option<InstallReceipt>, InstallError> {
        let Some(receipt) = self.receipt(bin_name)? else { return Ok(None) };
        if receipt.destination.is_file() {
            let found = sha256_file(&receipt.destination)?;
            if found != receipt.sha256 && !force {
                return Err(InstallError::Changed { path: receipt.destination.clone(), expected: receipt.sha256.clone(), found });
            }
            fs::remove_file(&receipt.destination).map_err(io_at(&receipt.destination))?;
        }
        let receipt_path = checked_join(&self.state_dir, format!("{bin_name}.install.json"))?;
        fs::remove_file(&receipt_path).map_err(io_at(&receipt_path))?;
        Ok(Some(receipt))
    }

    /// Make sure `self.bin_dir()` is on `PATH` for future shells, without
    /// ever writing the same block twice. Picks the rc file from `$SHELL`
    /// (zsh, bash, fish), creating it if missing. `rc_file` overrides the
    /// choice (tests, unusual setups).
    pub fn ensure_path_hook(&self, rc_file: Option<&Path>) -> Result<PathHook, InstallError> {
        if on_path(&self.bin_dir) {
            return Ok(PathHook::AlreadyOnPath);
        }
        let rc = match rc_file {
            Some(p) => p.to_path_buf(),
            None => default_rc_file()?,
        };
        let is_fish = rc.extension().map(|e| e == "fish").unwrap_or(false);
        let dir = self.bin_dir.display().to_string();
        let marker = format!("# >>> installation: {dir} >>>");
        let existing = fs::read_to_string(&rc).unwrap_or_default();
        if existing.contains(&marker) {
            return Ok(PathHook::Present { rc_file: rc });
        }
        let line = if is_fish { format!("fish_add_path \"{dir}\"") } else { format!("export PATH=\"{dir}:$PATH\"") };
        let block = format!("\n{marker}\n{line}\n# <<< installation <<<\n");
        if let Some(parent) = rc.parent() {
            fs::create_dir_all(parent).map_err(io_at(parent))?;
        }
        let mut current = existing;
        current.push_str(&block);
        fs::write(&rc, current).map_err(io_at(&rc))?;
        Ok(PathHook::Added { rc_file: rc })
    }
}

/// Build `bin_name` from the crate at `manifest_dir` and return the path of
/// the executable cargo reports, wherever the target directory is.
pub fn build_binary(manifest_dir: &Path, bin_name: &str, release: bool) -> Result<PathBuf, InstallError> {
    validate_bin_name(bin_name)?;
    let manifest = manifest_dir.join("Cargo.toml");
    let mut cmd = Command::new("cargo");
    cmd.arg("build").arg("--manifest-path").arg(&manifest).arg("--bin").arg(bin_name).arg("--message-format=json-render-diagnostics");
    if release {
        cmd.arg("--release");
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::inherit());
    let mut child = cmd.spawn().map_err(InstallError::CargoSpawn)?;
    let mut executable: Option<PathBuf> = None;
    if let Some(out) = child.stdout.take() {
        for line in io::BufReader::new(out).lines() {
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
            if v.get("reason").and_then(|r| r.as_str()) != Some("compiler-artifact") {
                continue;
            }
            let is_bin = v.pointer("/target/kind").and_then(|k| k.as_array()).is_some_and(|k| k.iter().any(|x| x.as_str() == Some("bin")));
            let name_ok = v.pointer("/target/name").and_then(|n| n.as_str()) == Some(bin_name);
            if is_bin && name_ok {
                if let Some(p) = v.get("executable").and_then(|e| e.as_str()) {
                    executable = Some(PathBuf::from(p));
                }
            }
        }
    }
    let status = child.wait().map_err(InstallError::CargoSpawn)?;
    if !status.success() {
        return Err(InstallError::CargoBuild { manifest, status: status.to_string() });
    }
    executable.ok_or(InstallError::NoArtifact { manifest, bin: bin_name.to_string() })
}

/// The 0.1 shape, kept so callers keep compiling: build `bin_name` from the
/// crate in the current directory (release) and install it for the current
/// user. Prefer [`build_binary`] + [`Installer`] for anything you can print
/// or undo.
#[derive(Debug, Clone)]
pub struct InstallOptions {
    pub bin_name: String,
    pub dest_dir: Option<PathBuf>,
}

impl InstallOptions {
    #[allow(clippy::should_implement_trait)]
    pub fn default(bin_name: &str) -> Self {
        Self { bin_name: bin_name.to_string(), dest_dir: None }
    }
}

pub fn build_and_install(opts: InstallOptions) -> Result<PathBuf, InstallError> {
    let cwd = std::env::current_dir().map_err(|e| InstallError::Io { path: PathBuf::from("."), source: e })?;
    let built = build_binary(&cwd, &opts.bin_name, true)?;
    let installer = match opts.dest_dir {
        Some(dir) => {
            let base = Installer::new(&opts.bin_name, Scope::User)?;
            Installer::user_in(&opts.bin_name, dir, base.state_dir().to_path_buf())?
        }
        None => Installer::new(&opts.bin_name, Scope::User)?,
    };
    let plan = installer.plan(&opts.bin_name, &built)?;
    let receipt = installer.install(&plan)?;
    Ok(receipt.destination)
}

// ---------------------------------------------------------------------------

fn validate_bin_name(name: &str) -> Result<(), InstallError> {
    let ok = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.chars().any(char::is_control);
    if ok {
        Ok(())
    } else {
        Err(InstallError::BadBinName(name.to_string()))
    }
}

/// Lowercase hex SHA-256 of a file, streamed.
pub fn sha256_file(path: &Path) -> Result<String, InstallError> {
    let mut f = fs::File::open(path).map_err(io_at(path))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(io_at(path))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), InstallError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path).map_err(io_at(path))?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).map_err(io_at(path))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), InstallError> {
    Ok(())
}

fn on_path(dir: &Path) -> bool {
    std::env::var_os("PATH").map(|p| std::env::split_paths(&p).any(|d| d == dir)).unwrap_or(false)
}

fn default_rc_file() -> Result<PathBuf, InstallError> {
    let home = dirs::home_dir().ok_or(InstallError::NoHome)?;
    let shell = std::env::var("SHELL").unwrap_or_default();
    let shell = shell.rsplit('/').next().unwrap_or("");
    Ok(match shell {
        "fish" => home.join(".config").join("fish").join("config.fish"),
        "bash" => home.join(".bashrc"),
        _ => home.join(".zshrc"),
    })
}
