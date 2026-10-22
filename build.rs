use std::path::PathBuf;

fn main() {
    stamp_versions();

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

    // Only compile the Argus shim on aarch64 Linux (Jetson).
    if target_os != "linux" || target_arch != "aarch64" {
        return;
    }

    let argus_include = PathBuf::from("/usr/src/jetson_multimedia_api/argus/include");
    let mmapi_include = PathBuf::from("/usr/src/jetson_multimedia_api/include");

    if !argus_include.exists() {
        return;
    }

    println!("cargo:rerun-if-changed=src/lk_argus.cpp");

    cc::Build::new()
        .cpp(true)
        .file("src/lk_argus.cpp")
        .include(&argus_include)
        .include(&mmapi_include)
        .flag("-std=c++14")
        .flag("-Wno-deprecated-declarations")
        .compile("lk_argus");

    // Link Argus client library (talks to nvargus-daemon) and NvBufSurface
    println!("cargo:rustc-link-lib=dylib=nvargus_socketclient");
    println!("cargo:rustc-link-lib=dylib=nvbufsurface");

    let tegra_lib_dir = PathBuf::from("/usr/lib/aarch64-linux-gnu/tegra");
    if tegra_lib_dir.exists() {
        println!("cargo:rustc-link-search=native={}", tegra_lib_dir.display());
    }
}

/// Embed the pinned SDK commit and this repo's commit for `lk-latency version`.
fn stamp_versions() {
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-env-changed=LK_LATENCY_COMMIT");
    let manifest = std::fs::read_to_string("Cargo.toml").unwrap_or_default();
    let sdk_rev = manifest
        .lines()
        .find(|line| line.starts_with("livekit ="))
        .and_then(|line| line.split("rev = \"").nth(1))
        .and_then(|rest| rest.split('"').next())
        .unwrap_or("unknown")
        .to_owned();
    let commit = std::env::var("LK_LATENCY_COMMIT").ok().filter(|c| !c.is_empty()).unwrap_or_else(|| {
        std::process::Command::new("git")
            .args(["rev-parse", "--short=12", "HEAD"])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
            .unwrap_or_else(|| "unknown".into())
    });
    println!("cargo:rustc-env=LK_LATENCY_SDK_REV={sdk_rev}");
    println!("cargo:rustc-env=LK_LATENCY_COMMIT={commit}");
}
