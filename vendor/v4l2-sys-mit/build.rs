use std::path::PathBuf;

fn main() {
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let src = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("bindings")
        .join(format!("{arch}.rs"));
    if !src.exists() {
        panic!(
            "No pre-generated v4l2 bindings for {}. Remove the [patch.crates-io] section \
             from TurtleFrame's Cargo.toml and install libclang to generate them.",
            arch
        );
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("v4l2_bindings.rs");
    std::fs::copy(&src, out).unwrap();
    println!("cargo:rerun-if-changed=bindings");
}
