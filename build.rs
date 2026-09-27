use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=scripts/generate_wayland_protocols.py");
    println!("cargo:rerun-if-env-changed=PYTHON");

    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"));
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo build output directory"))
        .join("gui_wayland_generated.rs");
    let python = env::var_os("PYTHON").unwrap_or_else(|| "python3".into());
    let status = Command::new(&python)
        .arg(root.join("scripts/generate_wayland_protocols.py"))
        .arg("--cargo")
        .arg("--output")
        .arg(&output)
        .status()
        .unwrap_or_else(|error| {
            panic!(
                "cannot run Wayland protocol generator with {python:?}: {error}; \
                 install Python 3.10 or newer, or set PYTHON to its executable"
            )
        });
    assert!(
        status.success(),
        "Wayland protocol generation failed ({status}); generated output is required"
    );
}
