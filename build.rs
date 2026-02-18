fn main() {
    // Burn git-derived version into the binary.
    // Uses `git describe --tags --always --dirty` to produce e.g. "v0.1.2",
    // "v0.1.2-3-gabcdef", or "v0.1.2-dirty". Falls back to CARGO_PKG_VERSION.
    let version = std::process::Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if v.is_empty() { None } else { Some(v) }
        })
        .unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".into()));

    println!("cargo:rustc-env=NEXDESK_VERSION={}", version);
    // Re-run if HEAD changes (new commit or tag)
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/tags");

    // On macOS, nix may provide GCC as `cc` which doesn't know about the
    // macOS SDK paths. Tell the linker where to find system libraries like iconv.
    if std::env::consts::OS == "macos" {
        if let Ok(output) = std::process::Command::new("xcrun")
            .args(["--show-sdk-path"])
            .output()
        {
            if output.status.success() {
                let sdk_path = String::from_utf8_lossy(&output.stdout);
                let sdk_path = sdk_path.trim();
                println!("cargo:rustc-link-search=native={}/usr/lib", sdk_path);
            }
        }
    }
}
