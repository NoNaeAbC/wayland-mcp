// Queue work and program compilation own separate resources on one context.
#include "visual_gpu_internal.h"
#include "vulkan_probe_helpers.h"

#include "visual_gpu.h"
#include "visual_program_gpu.h"
#include <cstdint>
#include <iostream>
#include <latch>
#include <string_view>
#include <thread>

constexpr std::string_view SOURCE = R"GLSL(
#version 450
layout(local_size_x = 1) in;
layout(set = 0, binding = 0, rgba16f) readonly uniform image2D frame;
layout(set = 0, binding = 5, std430) buffer Result { uint value; }
result;
void main() { result.value = imageLoad(frame, ivec2(0)).r >= 0.0 ? 42u : 0u; }
)GLSL";

int main() {
	Context context;
	probe_value(context.init(226, 128));
	Visual_Program_Config config{
			._width           = 16,
			._height          = 16,
			._result_bytes    = 4,
			._state_bytes     = 4,
			._scratch_bytes   = 4,
			._parameter_bytes = 0,
			._previous_frame  = 0,
			._feedback        = 0,
			._alpha_mode      = 0,
			._pass_count      = 1,
	};
	Visual_Shader_Pass pass{
			._source      = SOURCE.data(),
			._source_size = static_cast<uint32_t>(SOURCE.size()),
			._groups =
					{
							1,
							1,
							1,
					},
	};
	std::latch   ready{1};
	std::jthread frames([&] {
		Runtime_Program program(&context);
		probe_value(program.init(config, std::span(&pass, 1), {}));
		ready.count_down();
		for (unsigned frame = 0; frame < 200; frame++) {
			probe_value(program.capture(nullptr, 0));
			uint32_t result{};
			if (probe_value(program.analyze_result(reinterpret_cast<uint8_t *>(&result))) != 4 || result != 42) {
				probe_fail("concurrent program compilation corrupted frame results");
			}
		}
	});
	ready.wait();
	for (unsigned compilation = 0; compilation < 8; compilation++) {
		Runtime_Program program(&context);
		probe_value(program.init(config, std::span(&pass, 1), {}));
	}
	frames.join();
	std::cout << "Concurrent compilation/destruction and 200 GPU submissions on "
				 "one context: PASS\n";
}
