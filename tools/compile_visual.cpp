#include "../src/shader_compiler.h"
#include "../src/visual_color_shader.h"
#include "../src/visual_result.h"
#include <expected>
#include <fstream>
#include <iostream>
#include <iterator>
#include <string>
#include <string_view>
#include <utility>

// Build tool only: reads repository source once and emits an embedded header.
// Runtime compilation uses the same library with source supplied in memory.
int main(int argc, char **argv) {
	if (argc != 3) {
		return 2;
	}
	auto const run = [&]() -> visual::Status {
		std::ifstream input(argv[1]);
		if (!input) {
			return std::unexpected("cannot read build-time shader source");
		}
		std::string source{std::istreambuf_iterator<char>(input), {}};
		if (input.bad()) {
			return std::unexpected("failed to read build-time shader source");
		}
		constexpr std::string_view MARKER = "// VISUAL_COLOR_GLSL";
		if (auto const at = source.find(MARKER); at != std::string::npos) {
			source.replace(at, MARKER.size(), VISUAL_COLOR_GLSL);
		}
		auto words_result = compile_visual_glsl(source);
		if (!words_result) {
			return std::unexpected(std::move(words_result.error()));
		}
		auto const    words = std::move(*words_result);
		std::ofstream output(argv[2]);
		output << "#pragma once\n#include <array>\n#include <cstdint>\ninline "
				  "constexpr std::array<std::uint32_t,"
			   << words.size() << "> visual_spirv{\n";
		for (auto const word: words) {
			output << "    0x" << std::hex << word << ",\n";
		}
		output << "};\n";
		output.flush();
		if (!output) {
			return std::unexpected("cannot write embedded shader header");
		}
		return {};
	};
	if (auto status = run(); !status) {
		std::cerr << status.error() << '\n';
		return 1;
	}
}
