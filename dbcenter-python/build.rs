use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=../dbcenter-watcher/src/main.rs");

    // Prevent recursive invocations when we build the watcher binary below.
    if env::var_os("DBCENTER_BUILDING_WATCHER").is_some() {
        return;
    }

    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let target_triple = env::var("TARGET").expect("TARGET not set");
    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let target_os = env::var("CARGO_CFG_TARGET_OS").expect("CARGO_CFG_TARGET_OS not set");

    let bin_name = if target_os == "windows" {
        "dbcenter-watcher.exe"
    } else {
        "dbcenter-watcher"
    };

    // Build the watcher binary for the current target/profile.
    let nested_target = out_dir.join("watcher-build");
    fs::create_dir_all(&nested_target).expect("failed to create watcher target dir");

    let watcher_manifest = manifest_dir
        .parent()
        .expect("dbcenter-python should have a parent directory")
        .join("dbcenter-watcher")
        .join("Cargo.toml");

    let mut build_cmd = Command::new("cargo");
    build_cmd.env("DBCENTER_BUILDING_WATCHER", "1");
    build_cmd.env("CARGO_TARGET_DIR", &nested_target);
    build_cmd.current_dir(&manifest_dir);
    build_cmd.args([
        "build",
        "--manifest-path",
        watcher_manifest
            .to_str()
            .expect("watcher manifest path should be valid UTF-8"),
        "--target",
        &target_triple,
    ]);

    if profile == "release" {
        build_cmd.arg("--release");
    } else if profile != "debug" {
        build_cmd.args(["--profile", &profile]);
    }

    let status = build_cmd
        .status()
        .expect("failed to invoke cargo to build watcher");
    if !status.success() {
        panic!("failed to build dbcenter-watcher binary");
    }

    // Copy the freshly built binary into OUT_DIR so we can embed it at compile time.
    let mut artifact_path = nested_target;
    artifact_path = artifact_path.join(&target_triple);
    artifact_path = artifact_path.join(&profile);
    artifact_path = artifact_path.join(bin_name);

    if !artifact_path.exists() {
        panic!(
            "dbcenter-watcher artifact not found at {}",
            artifact_path.display()
        );
    }

    let embedded_path = out_dir.join(bin_name);
    fs::create_dir_all(&out_dir).expect("failed to create OUT_DIR");
    fs::copy(&artifact_path, &embedded_path)
        .unwrap_or_else(|err| panic!("failed to copy watcher binary: {}", err));

    // Expose the embedded binary path to the Rust code for include_bytes!
    println!(
        "cargo:rustc-env=DBCENTER_WATCHER_EMBED={}",
        embedded_path.display()
    );
}
