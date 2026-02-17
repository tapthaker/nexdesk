fn main() {
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
