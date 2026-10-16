fn main() {
    println!("cargo:rerun-if-changed=evbar.manifest");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let manifest = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("evbar.manifest");
        println!("cargo:rustc-link-arg-bin=evbar=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bin=evbar=/MANIFESTINPUT:{}", manifest.display());
    }
}
