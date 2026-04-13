fn main() {
    #[cfg(target_os = "macos")]
    {
        let mock = std::env::var("CARGO_FEATURE_VMNET_MOCK").is_ok();
        if mock {
            cc::Build::new()
                .file("src/vmnet_mock.c")
                .compile("vmnet_bridge");
            // Do NOT link vmnet.framework — the stub provides all symbols.
        } else {
            cc::Build::new()
                .file("src/vmnet_bridge.c")
                .compile("vmnet_bridge");
            println!("cargo:rustc-link-lib=framework=vmnet");
        }
        println!("cargo:rerun-if-changed=src/vmnet_bridge.c");
        println!("cargo:rerun-if-changed=src/vmnet_mock.c");
        println!("cargo:rerun-if-changed=build.rs");
    }
}
