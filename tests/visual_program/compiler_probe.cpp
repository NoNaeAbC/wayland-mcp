#include "shader_compiler.h"
#include <iostream>

int main() {
	const auto words = compile_visual_glsl(R"(
#version 450
layout(local_size_x = 64) in;
layout(set = 0, binding = 0, std430) buffer Result { uint flag; }
result;
void main() { atomicOr(result.flag, 1u); }
)");
	if (!words || words->empty() || (*words)[0] != 0x07230203u) {
		return 1;
	}
	for (const auto source: {
				 "#version 450\ninvalid GLSL",
				 R"(
#version 450
#extension GL_GOOGLE_include_directive : require
#include "/etc/passwd"
layout(local_size_x = 1) in;
void main() {}
)",
		 }) {
		const bool rejected = !compile_visual_glsl(source);
		if (!rejected) {
			return 2;
		}
	}
	if (compile_visual_glsl("") || compile_visual_glsl(std::string(262145, 'x'))) {
		return 3;
	}
	std::cout << "in-memory compute compilation, diagnostics and "
				 "filesystem-include rejection: PASS\n";
}
