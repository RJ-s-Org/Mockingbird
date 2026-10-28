//! Where the embedded example catalog and agent skills are read from: the
//! copies under `embedded/` in a published package (the release workflow puts
//! them there before `cargo publish`), else the repository's `examples/apis`
//! and `plugins/mockingbird/skills`.

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    for (var, packaged, repo) in [
        (
            "MB_EXAMPLES_DIR",
            "embedded/examples",
            "../../examples/apis",
        ),
        (
            "MB_SKILLS_DIR",
            "embedded/skills",
            "../../plugins/mockingbird/skills",
        ),
    ] {
        let packaged = manifest.join(packaged);
        let dir = if packaged.is_dir() {
            packaged
        } else {
            manifest.join(repo)
        };
        println!("cargo:rerun-if-changed={}", dir.display());
        println!("cargo:rustc-env={var}={}", dir.display());
    }
}
