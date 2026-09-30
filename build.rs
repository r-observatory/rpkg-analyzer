// Sets RPKG_ANALYZER_BUILD_ID, so a cache entry written by one build is never
// read by another, even under the same version number.
include!("src/build_id.rs");

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    println!("cargo:rustc-env=RPKG_ANALYZER_BUILD_ID={}", build_id(&root));
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=build.rs");
}
