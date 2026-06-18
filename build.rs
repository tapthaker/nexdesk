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
            if v.is_empty() {
                None
            } else {
                Some(v)
            }
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

        // macOS 15+ gates Bonjour/mDNS behind the Local Network privacy
        // permission. A command-line binary still needs an embedded Info.plist
        // with the local-network usage string and advertised/browsed Bonjour
        // service types, otherwise the permission prompt may never appear and
        // discovery silently fails.
        let out_dir = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
        let plist_path = out_dir.join("nexdesk-Info.plist");
        std::fs::write(
            &plist_path,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>works.earendil.nexdesk</string>
    <key>CFBundleName</key>
    <string>Nexdesk</string>
    <key>CFBundleDisplayName</key>
    <string>Nexdesk</string>
    <key>NSLocalNetworkUsageDescription</key>
    <string>Nexdesk uses the local network to discover and connect to your other computer.</string>
    <key>NSBonjourServices</key>
    <array>
        <string>_nexdesk._udp</string>
    </array>
</dict>
</plist>
"#,
        )
        .expect("write macOS Info.plist");
        println!(
            "cargo:rustc-link-arg=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist_path.display()
        );
    }
}
