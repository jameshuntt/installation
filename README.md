# installation

Install a built binary the honest way: plan first, install from the plan,
keep a receipt, uninstall from the receipt. Paths come from
[`pathgen`](https://crates.io/crates/pathgen).

```rust,no_run
use installation::{build_binary, Installer, Scope};
use std::path::Path;

fn main() -> Result<(), installation::InstallError> {
    // 1. ask cargo where the artifact really is (workspaces, CARGO_TARGET_DIR)
    let built = build_binary(Path::new("."), "mytool", true)?;

    // 2. decide every path; nothing is written yet
    let installer = Installer::new("mytool", Scope::User)?;
    let plan = installer.plan("mytool", &built)?;
    print!("{}", plan.describe());

    // 3. copy, verify the sha256, rename into place, write the receipt
    let receipt = installer.install(&plan)?;
    println!("installed {} ({})", receipt.destination.display(), receipt.sha256);

    // 4. make sure ~/.local/bin is on PATH for future shells, once
    installer.ensure_path_hook(None)?;
    Ok(())
}
```

## Scopes

- `Scope::User` — `$XDG_BIN_HOME` or `~/.local/bin`; receipts under
  `$XDG_STATE_HOME/installation` or `~/.local/state/installation`.
- `Scope::System { root }` — `/usr/local/bin`; receipts in the app's
  `pathgen` state directory, `/var/lib/<app>/install`. `root` re-roots every
  path, so a staging tree or a test directory gets the same layout under
  `root/usr/local/bin` and `root/var/lib/<app>/install`.

## Guarantees

- Nothing is written until `install`; `plan` only reads the source to digest it.
- Every path is built with `pathgen::checked_join`: a binary named `../x`
  is refused, not placed outside the directory.
- The copy is verified against the planned SHA-256 before it is renamed
  into place, so a partial write never becomes the installed binary.
- `uninstall` removes only what its receipt names and refuses a binary whose
  digest changed since, unless told to force.
- `ensure_path_hook` writes one marked block, recognises it on later runs,
  and skips entirely when the directory is already on `PATH`.

## Compatibility

`build_and_install(InstallOptions::default("name"))` from 0.1 still exists
and now goes through the same plan → install path.
