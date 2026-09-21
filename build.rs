use std::path::{Path, PathBuf};
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    for path in ["--git-dir", "--git-common-dir"] {
        if let Some(dir) = git(&["rev-parse", path]) {
            for entry in ["HEAD", "refs", "packed-refs", "index"] {
                println!("cargo:rerun-if-changed={dir}/{entry}");
            }
        }
    }
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets");
    println!("cargo:rerun-if-changed=crates");
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let release_tag = format!("v{version}");
    let tagged = git(&["tag", "--points-at", "HEAD"])
        .is_some_and(|tags| tags.lines().any(|tag| tag == release_tag || tag == version));
    let dirty = git(&["diff", "HEAD", "--quiet"]).is_none();
    let display_version = if tagged && !dirty {
        version
    } else {
        let commit = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into());
        format!("{version}+{commit}{}", if dirty { ".dirty" } else { "" })
    };
    println!("cargo:rustc-env=RIFT_VERSION={display_version}");
    println!("cargo:rustc-link-search=framework=/System/Library/PrivateFrameworks");

    println!("cargo:rustc-link-lib=framework=SkyLight");
    println!("cargo:rustc-link-lib=framework=CoreFoundation");
    println!("cargo:rustc-link-lib=framework=CoreVideo");
    println!("cargo:rustc-link-lib=framework=IOKit");
    println!("cargo:rustc-link-lib=framework=MultitouchSupport");
    println!("cargo:rustc-link-lib=framework=Carbon");

    build_osax();
}

/// Builds the two binaries of the scripting addition and leaves them in
/// `OUT_DIR` for `sys::osax` to embed.
///
/// Both are fat x86_64 + arm64e, and arm64e is the point: the payload is
/// `dlopen`ed inside Dock, which is arm64e on Apple Silicon, and the loader
/// spawns a thread there, which needs the same pointer-authentication ABI.
/// Neither is built for the host triple, so `cc` is no help and clang is driven
/// directly.
fn build_osax() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/osax");
    for source in [
        "payload.m",
        "loader.m",
        "arm64_payload.m",
        "x64_payload.m",
        "common.h",
        "hashtable.h",
    ] {
        println!("cargo:rerun-if-changed={}", dir.join(source).display());
    }

    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));

    clang(&dir.join("payload.m"), &out.join("payload"), &[
        "-shared",
        "-fPIC",
        "-F/System/Library/PrivateFrameworks",
        "-framework",
        "SkyLight",
        "-framework",
        "Foundation",
        "-framework",
        "Carbon",
    ]);

    clang(&dir.join("loader.m"), &out.join("loader"), &[
        "-framework",
        "Cocoa",
    ]);
}

fn clang(source: &Path, output: &Path, extra: &[&str]) {
    let mut command = Command::new("xcrun");
    command
        .arg("clang")
        .arg(source)
        .args(["-O3", "-mmacosx-version-min=11.0"])
        // -fno-objc-arc matches yabai's build: the vendored payload manages its
        // own retain/release and does not compile under ARC.
        .args(["-fno-objc-arc", "-arch", "x86_64", "-arch", "arm64e"])
        .args(extra)
        .arg("-o")
        .arg(output);

    let status = command.status().unwrap_or_else(|error| {
        panic!("failed to run xcrun clang for {}: {error}", source.display())
    });
    assert!(status.success(), "failed to build {}", source.display());
}
