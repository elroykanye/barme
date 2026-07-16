//! When the `ui` feature is on, build the React frontend so its output can be
//! embedded into the binary. Without the feature this is a no-op, so plain
//! `cargo build` stays fast and needs no Node toolchain.

use std::path::Path;
use std::process::Command;

fn main() {
    if std::env::var("CARGO_FEATURE_UI").is_err() {
        return;
    }

    let web = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web");
    println!("cargo:rerun-if-changed={}", web.join("src").display());
    println!("cargo:rerun-if-changed={}", web.join("package.json").display());
    println!("cargo:rerun-if-changed={}", web.join("index.html").display());

    let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };

    if !web.join("node_modules").exists() {
        run(npm, &["install"], &web);
    }
    run(npm, &["run", "build"], &web);
}

fn run(cmd: &str, args: &[&str], dir: &Path) {
    let status = Command::new(cmd)
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|e| panic!("failed to run `{cmd}` (is Node installed?): {e}"));
    if !status.success() {
        panic!("`{cmd} {}` failed", args.join(" "));
    }
}
