use std::{env, fs, path::Path};
fn schemas(path: &Path, files: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            schemas(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "capnp") {
            files.push(path);
        }
    }
}
fn main() {
    println!("cargo:rerun-if-changed=schema");
    let mut files = Vec::new();
    schemas(Path::new("schema"), &mut files);
    files.sort();
    let mut command = capnpc::CompilerCommand::new();
    command.src_prefix("schema").import_path("schema");
    if let Ok(path) = env::var("CAPNP") {
        command.capnp_executable(path);
    }
    println!("cargo:rerun-if-env-changed=CAPNP");
    let mut modules = String::new();
    for file in files {
        if file.file_name().unwrap() == "c++.capnp" {
            continue;
        }
        command.file(&file);
        let name = file
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap()
            .replace('-', "_");
        let relative = file
            .strip_prefix("schema")
            .unwrap()
            .with_file_name(format!("{name}_capnp.rs"));
        modules.push_str(&format!("#[allow(clippy::all)] pub mod {name}_capnp {{ include!(concat!(env!(\"OUT_DIR\"), \"/{}\")); }}\n", relative.display()));
    }
    command
        .run()
        .expect("compile Sandstorm schemas (install the capnp compiler or set CAPNP)");
    // capnpc emits an unbound generic for an enum nested in a generic
    // supervisor struct. The enum has no generic fields; instantiate its
    // annotation accessor with AnyPointer without changing the wire schema.
    let supervisor = Path::new(&env::var("OUT_DIR").unwrap()).join("sandstorm/supervisor_capnp.rs");
    let generated = fs::read_to_string(&supervisor).unwrap().replace(
        "type_::get_annotation_types)",
        "type_::get_annotation_types::<::capnp::any_pointer::Owned>)",
    );
    fs::write(supervisor, generated).unwrap();
    fs::write(
        Path::new(&env::var("OUT_DIR").unwrap()).join("schemas.rs"),
        modules,
    )
    .unwrap();
}
