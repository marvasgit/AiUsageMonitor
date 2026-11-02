fn main() {
    println!("cargo:rerun-if-changed=assets/app.ico");
    // CI picks the release version and passes it in; local builds use Cargo.toml's.
    println!("cargo:rerun-if-env-changed=AUM_VERSION");
    let version = std::env::var("AUM_VERSION")
        .ok()
        .map(|v| v.trim().trim_start_matches('v').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").unwrap());
    println!("cargo:rustc-env=AUM_VERSION={version}");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/app.ico");
        if let Err(e) = res.compile() {
            println!("cargo:warning=failed to embed icon: {e}");
        }
    }
}
