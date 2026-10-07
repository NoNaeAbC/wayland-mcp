#pragma once
#include "visual_color_shader.h"
#include <string>
// The trusted GPU pass decodes raw source pixels without CPU pixel readback.
inline const std::string visual_normalize_source = std::string(R"GLSL(
#version 450
#define COLOR_BINDING 2
layout(local_size_x = 16, local_size_y = 16) in;
layout(set = 0, binding = 0, std430) readonly buffer RawPixels { uint rgba[]; }
raw;
layout(set = 0, binding = 1, rgba16f) writeonly uniform image2D canonical;
layout(push_constant) uniform Source { uint width, height, format, reserved; }
source;
)GLSL") + VISUAL_COLOR_GLSL + R"GLSL(
void main() {
	uvec2 p = gl_GlobalInvocationID.xy;
	if (p.x >= source.width || p.y >= source.height) {
		return;
	}
	uint words = (source.format == 0x48344258u || source.format == 0x48344241u) ? 2u : 1u;
	imageStore(canonical, ivec2(p), normalizeSource(unpackSource((p.y * source.width + p.x) * words, source.format)));
}
)GLSL";
