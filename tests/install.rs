//! Real installs into temporary directories: a real cargo build of a fixture
//! binary, a system-scope install rooted in a temp dir (pathgen root
//! remapping), a user-scope install with explicit dirs, uninstall, and the
//! PATH hook written once.

use installation::{build_binary, Installer, InstallError, PathHook, Scope};
use std::path::Path;

fn fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join("hello")
}

#[test]
fn builds_plans_installs_and_uninstalls_in_a_rooted_system_scope() {
    let built = build_binary(&fixture(), "hello", false).expect("cargo builds the fixture");
    assert!(built.is_file(), "{built:?}");

    let root = tempfile::tempdir().unwrap();
    let inst = Installer::new("hello-app", Scope::System { root: Some(root.path().to_path_buf()) }).unwrap();
    assert_eq!(inst.bin_dir(), root.path().join("usr/local/bin"));
    assert_eq!(inst.state_dir(), root.path().join("var/lib/hello-app/install"));

    let plan = inst.plan("hello", &built).unwrap();
    assert_eq!(plan.destination, root.path().join("usr/local/bin/hello"));
    assert_eq!(plan.receipt, root.path().join("var/lib/hello-app/install/hello.install.json"));
    assert_eq!(plan.sha256.len(), 64);
    assert!(plan.audit.iter().any(|a| a.contains("role=state")), "{:?}", plan.audit);
    assert!(!plan.destination.exists(), "planning writes nothing");

    let receipt = inst.install(&plan).unwrap();
    assert!(plan.destination.is_file());
    assert!(plan.receipt.is_file());
    assert_eq!(receipt.sha256, plan.sha256);
    assert_eq!(inst.receipt("hello").unwrap(), Some(receipt.clone()));
    let out = std::process::Command::new(&plan.destination).output().expect("installed binary runs");
    assert!(String::from_utf8_lossy(&out.stdout).contains("hello from the installation fixture"));

    // replaced binary: uninstall refuses without force
    std::fs::write(&plan.destination, b"not the same").unwrap();
    assert!(matches!(inst.uninstall("hello", false), Err(InstallError::Changed { .. })));
    assert!(plan.destination.exists());
    let gone = inst.uninstall("hello", true).unwrap();
    assert_eq!(gone.map(|r| r.bin_name), Some("hello".to_string()));
    assert!(!plan.destination.exists());
    assert!(!plan.receipt.exists());
    assert_eq!(inst.uninstall("hello", false).unwrap(), None, "no receipt, nothing to do");
}

#[test]
fn user_scope_with_explicit_dirs_and_bad_names_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("tool.bin");
    std::fs::write(&src, b"#!/bin/sh\necho hi\n").unwrap();
    let inst = Installer::user_in("tool", tmp.path().join("bin"), tmp.path().join("state")).unwrap();
    let plan = inst.plan("tool", &src).unwrap();
    assert_eq!(plan.destination, tmp.path().join("bin/tool"));
    inst.install(&plan).unwrap();
    assert!(tmp.path().join("bin/tool").is_file());
    assert!(tmp.path().join("state/tool.install.json").is_file());

    assert!(matches!(inst.plan("../escape", &src), Err(InstallError::BadBinName(_))));
    assert!(matches!(inst.plan("a/b", &src), Err(InstallError::BadBinName(_))));
    assert!(matches!(inst.plan("missing", &tmp.path().join("nope")), Err(InstallError::SourceNotFile(_))));
    assert!(Installer::new("Bad Name", Scope::User).is_err(), "pathgen's app-name rules apply");
}

#[test]
fn path_hook_is_written_once_and_recognised() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let inst = Installer::user_in("tool", &bin, tmp.path().join("state")).unwrap();
    let rc = tmp.path().join("rc");
    let first = inst.ensure_path_hook(Some(&rc)).unwrap();
    assert_eq!(first, PathHook::Added { rc_file: rc.clone() });
    let second = inst.ensure_path_hook(Some(&rc)).unwrap();
    assert_eq!(second, PathHook::Present { rc_file: rc.clone() });
    let text = std::fs::read_to_string(&rc).unwrap();
    assert_eq!(text.matches("installation:").count(), 1, "{text}");
    assert!(text.contains(&format!("export PATH=\"{}:$PATH\"", bin.display())));

    let fish = tmp.path().join("config.fish");
    inst.ensure_path_hook(Some(&fish)).unwrap();
    assert!(std::fs::read_to_string(&fish).unwrap().contains("fish_add_path"));
}
