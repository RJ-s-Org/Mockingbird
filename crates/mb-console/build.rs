//! Makes sure `dist/index.html` exists, so `cargo build` never needs Node:
//! without a Vite build, a placeholder page tells how to build the console.

use std::path::Path;

const PLACEHOLDER: &str = "<!doctype html>
<html lang=\"en\">
<head><meta charset=\"utf-8\"><title>Mockingbird</title></head>
<body><p>Console not built — run npm --prefix crates/mb-console/web run build</p></body>
</html>
";

fn main() {
    println!("cargo:rerun-if-changed=dist");
    let dist = Path::new("dist");
    if dist.join("index.html").exists() {
        return;
    }
    if let Err(e) = std::fs::create_dir_all(dist)
        .and_then(|()| std::fs::write(dist.join("index.html"), PLACEHOLDER))
    {
        println!("cargo:warning=cannot write the console placeholder: {e}");
        std::process::exit(1);
    }
}
