@0x9c7bf302ea35c8a1;

# Run `cargo xtask prepare-dev` before `spk dev`.
# The generated manifest is shared with the Rust release packager.
using Package = import "/sandstorm/package.capnp";
using Generated = import "build/package/manifest.capnp";

const pkgdef :Package.PackageDefinition = (
  id = "3theazdrcs30r9qysmh6ks0vndm3z4xkuk6k9qmu506h7mamvqg0",
  manifest = Generated.manifest,
  sourceMap = (searchPath = [(sourcePath = "build/package/stage")]),
  alwaysInclude = ["."]
);
