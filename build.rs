use std::env;
use std::path::PathBuf;
use std::process::Command;

fn compile_baseline(output: &std::path::Path) {
    let color_header = std::fs::read_to_string("src/visual_color_shader.h")
        .expect("read shared color shader source");
    let color = color_header
        .split_once("R\"GLSL(")
        .and_then(|(_, rest)| rest.split_once(")GLSL\";"))
        .map(|(source, _)| source)
        .expect("shared color shader must use the GLSL raw-string delimiter");
    let source = std::fs::read_to_string("src/visual.comp")
        .expect("read baseline shader source")
        .replace("// VISUAL_COLOR_GLSL", color);
    let compiler = shaderc::Compiler::new().expect("initialize static Shaderc compiler");
    let mut options = shaderc::CompileOptions::new().expect("initialize compiler options");
    options.set_source_language(shaderc::SourceLanguage::GLSL);
    options.set_target_env(
        shaderc::TargetEnv::Vulkan,
        shaderc::EnvVersion::Vulkan1_1 as u32,
    );
    options.set_optimization_level(shaderc::OptimizationLevel::Performance);
    let spirv = compiler
        .compile_into_spirv(
            &source,
            shaderc::ShaderKind::Compute,
            "submitted-program",
            "main",
            Some(&options),
        )
        .expect("compile embedded baseline shader");
    let words = spirv.as_binary();
    let mut header = format!(
        "#pragma once\n#include <array>\n#include <cstdint>\ninline constexpr std::array<std::uint32_t,{}> visual_spirv{{\n",
        words.len()
    );
    for word in words {
        use std::fmt::Write as _;
        writeln!(header, "    0x{word:x},").expect("format SPIR-V word");
    }
    header.push_str("};\n");
    std::fs::write(output.join("visual_spirv.h"), header).expect("write embedded shader header");
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
    let gpu_out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
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

    compile_baseline(&gpu_out);
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
