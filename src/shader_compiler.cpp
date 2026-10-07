#include "shader_compiler.h"
#include "visual_result.h"
#include <cstddef>
#include <cstdint>
#include <expected>
#include <shaderc/env.h>
#include <shaderc/shaderc.h>
#include <shaderc/shaderc.hpp>
#include <shaderc/status.h>
#include <string_view>
#include <vector>

visual::Result<std::vector<uint32_t>> compile_visual_glsl(std::string_view source) {
	if (source.empty() || source.size() > std::size_t{256} * 1024) {
		return std::unexpected("GLSL source must contain 1..262144 bytes");
	}
	shaderc::Compiler const compiler;
	shaderc::CompileOptions options;
	options.SetSourceLanguage(shaderc_source_language_glsl);
	options.SetTargetEnvironment(shaderc_target_env_vulkan, shaderc_env_version_vulkan_1_1);
	options.SetOptimizationLevel(shaderc_optimization_level_performance);
	// shaderc has no default filesystem include resolver.
	auto const result = compiler.CompileGlslToSpv(source.data(), source.size(), shaderc_compute_shader,
												  "submitted-program", "main", options);
	if (result.GetCompilationStatus() != shaderc_compilation_status_success) {
		return std::unexpected(result.GetErrorMessage());
	}
	return std::vector<uint32_t>(result.cbegin(), result.cend());
}
