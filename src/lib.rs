use std::{
    env, fs, io::Write, path::{Path, PathBuf}, process::Command
};
use anyhow::{Result, anyhow};

pub struct InstallOptions {
    pub bin_name: String,
    pub dest_dir: Option<PathBuf>, // override if needed
}

impl InstallOptions {
    pub fn default(bin_name: &str) -> Self {
        Self {
            bin_name: bin_name.to_string(),
            dest_dir: None,
        }
    }
}

pub fn build_and_install(opts: InstallOptions) -> Result<PathBuf> {
    // 1. Build
    let status = Command::new("cargo")
        .arg("build")
        .arg("--release")
        .status()?;

    if !status.success() {
        return Err(anyhow!("cargo build failed"));
    }

    // 2. Locate target binary
    let exe_name = &opts.bin_name;
    let exe_path = Path::new("target").join("release").join(exe_name);

    if !exe_path.exists() {
        return Err(anyhow!("expected binary at {:?}", exe_path));
    }

    // 3. Determine install location
    let install_dir = opts
        .dest_dir
        .clone()
        .unwrap_or_else(|| dirs::home_dir().unwrap().join(".local/bin"));

    fs::create_dir_all(&install_dir)?;

    // 4. Copy binary
    let target = install_dir.join(exe_name);
    fs::copy(&exe_path, &target)?;

    println!("Installed → {}", target.display());

    Ok(target)
}



pub fn ensure_path_hook(path: &Path) -> Result<()> {
    let zshrc = dirs::home_dir().unwrap().join(".zshrc");

    let line = format!(
        "\n# added by toolchain installer\nexport PATH=\"{}:$PATH\"\n",
        path.display()
    );

    fs::OpenOptions::new()
        .append(true)
        .open(&zshrc)?
        .write_all(line.as_bytes())?;

    Ok(())
}
