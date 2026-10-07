use std::env;
use std::path::PathBuf;
use std::process::Command;

fn compiler_archives() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/usr/lib"),
        PathBuf::from("/usr/local/lib"),
        PathBuf::from(".private/compiler-lib"),
    ];
    if let Some(paths) = env::var_os("WAYLAND_MCP_SHADERC_ARCHIVE_DIRS") {
        dirs.extend(env::split_paths(&paths));
    }
    ["shaderc_combined", "glslang", "MachineIndependent", "GenericCodeGen", "OSDependent", "SPIRV", "SPIRV-Tools-opt", "SPIRV-Tools"].into_iter().map(|name| {
        let archive = format!("lib{name}.a");
        dirs.iter().map(|dir| dir.join(&archive)).find(|file| file.is_file()).unwrap_or_else(|| panic!("static compiler archive {archive} required; provide its directory in WAYLAND_MCP_SHADERC_ARCHIVE_DIRS"))
    }).collect()
}

fn main() {
    println!("cargo:rerun-if-changed=src/visual.comp");
    println!("cargo:rerun-if-changed=src/visual_gpu.cpp");
    println!("cargo:rerun-if-changed=src/visual_gpu.h");
    for file in [
        "src/shader_compiler.cpp",
        "src/shader_compiler.h",
        "src/visual_result.h",
        "src/visual_normalize_shader.h",
        "src/visual_color_shader.h",
        "src/visual_program_gpu.h",
        "src/visual_gpu_internal.h",
        "tools/compile_visual.cpp",
    ] {
        println!("cargo:rerun-if-changed={file}");
    }
    println!("cargo:rerun-if-env-changed=WAYLAND_MCP_SHADERC_ARCHIVE_DIRS");
    let gpu_out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let archives = compiler_archives();
    for archive in &archives {
        println!("cargo:rerun-if-changed={}", archive.display());
    }
    println!("cargo:rerun-if-env-changed=CXX");
    let native_cxx = env::var_os("CXX").unwrap_or_else(|| "g++".into());
    let native_flags = [
        "-std=c++26",
        "-fno-exceptions",
        "-fno-rtti",
        "-Wall",
        "-Wextra",
        "-Wpedantic",
        "-Werror",
        "-Wold-style-cast",
    ];

    let mut compiler = Command::new(&native_cxx);
    compiler.args(native_flags).args([
        "-O2",
        "tools/compile_visual.cpp",
        "src/shader_compiler.cpp",
        "-Wl,--start-group",
    ]);
    compiler
        .args(&archives)
        .args(["-Wl,--end-group", "-lpthread", "-o"])
        .arg(gpu_out.join("compile_visual"));
    assert!(compiler.status().expect("C++ compiler required").success());
    assert!(
        Command::new(gpu_out.join("compile_visual"))
            .arg("src/visual.comp")
            .arg(gpu_out.join("visual_spirv.h"))
            .status()
            .expect("statically linked build compiler required")
            .success()
    );
    assert!(
        Command::new(&native_cxx)
            .args(native_flags)
            .args(["-O2", "-fPIC", "-c", "src/shader_compiler.cpp", "-o"])
            .arg(gpu_out.join("shader_compiler.o"))
            .status()
            .expect("C++ compiler required")
            .success()
    );
    assert!(
        Command::new(&native_cxx)
            .args(native_flags)
            .args(["-O2", "-fPIC", "-c", "src/visual_gpu.cpp", "-I"])
            .arg(&gpu_out)
            .arg("-o")
            .arg(gpu_out.join("visual_gpu.o"))
            .status()
            .expect("C++ compiler required")
            .success()
    );
    assert!(
        Command::new("ar")
            .arg("crs")
            .arg(gpu_out.join("libvisual_gpu.a"))
            .arg(gpu_out.join("visual_gpu.o"))
            .arg(gpu_out.join("shader_compiler.o"))
            .status()
            .unwrap()
            .success()
    );
    println!("cargo:rustc-link-search=native={}", gpu_out.display());
    println!("cargo:rustc-link-lib=static=visual_gpu");
    println!("cargo:rustc-link-lib=stdc++");
    println!("cargo:rustc-link-arg=-Wl,--start-group");
    for archive in &archives {
        println!(
            "cargo:rustc-link-arg={}",
            std::fs::canonicalize(archive).unwrap().display()
        );
    }
    println!("cargo:rustc-link-arg=-Wl,--end-group");
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
