//! Embed every template's `config.yml` and file list, so the Worker can build
//! its catalog without listing directories (static assets can't be listed).

use std::fmt::Write as _;
use std::path::Path;

fn main() {
    let templates = Path::new(env!("CARGO_MANIFEST_DIR")).join("../templates");
    println!("cargo:rerun-if-changed={}", templates.display());

    let mut entries: Vec<_> = std::fs::read_dir(&templates)
        .expect("reading templates/")
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.join("config.yml").is_file())
        .collect();
    entries.sort();

    let mut out = String::from("pub const TEMPLATES: &[(&str, &str, &[&str])] = &[\n");
    for path in entries {
        println!("cargo:rerun-if-changed={}", path.display());
        let id = path.file_name().unwrap().to_str().unwrap();
        let mut files: Vec<String> = std::fs::read_dir(&path)
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.file_type().unwrap().is_file())
            .map(|entry| entry.file_name().into_string().unwrap())
            .collect();
        files.sort();
        let config = path.join("config.yml").canonicalize().unwrap();
        writeln!(
            out,
            "    ({id:?}, include_str!({:?}), &{files:?}),",
            config.display().to_string()
        )
        .unwrap();
    }
    out.push_str("];\n");
    let destination = Path::new(&std::env::var("OUT_DIR").unwrap()).join("templates.rs");
    std::fs::write(destination, out).unwrap();
}
