// Validate the actual RuntimeProgram implementation on the DRM-affine GPU.
// Synthetic source frames are filled on GPU; only the bounded result packet
// is mapped back to the CPU.
#include "visual_gpu_internal.h"
#include "vulkan_probe_helpers.h"

#include "probe_programs.h"
#include "visual_gpu.h"
#include "visual_program_gpu.h"
#include <array>
#include <cstdint>
#include <cstring>
#include <iostream>
#include <utility>
#include <vulkan/vulkan_core.h>

namespace {

	void require(bool value, const char *message) {
		if (!value) {
			probe_fail(message);
		}
	}

	constexpr char INVALID_BINDING_SOURCE[] = R"GLSL(
#version 450
layout(local_size_x = 1) in;
layout(set = 0, binding = 7, std430) readonly buffer Bad { uint value; }
bad;
layout(set = 0, binding = 5, std430) buffer Out { uint value; }
outData;
void main() { outData.value = bad.value; }
)GLSL";

	constexpr char WRITABLE_FRAME_SOURCE[] = R"GLSL(
#version 450
layout(local_size_x = 1) in;
layout(set = 0, binding = 0, rgba16f) writeonly uniform image2D frame;
void main() { imageStore(frame, ivec2(0), vec4(0)); }
)GLSL";

	std::array<uint32_t, 8> run_sequence(Context &context, bool feedback) {
		constexpr uint32_t          WIDTH = 64, HEIGHT = 32, RESULT_BYTES = 32;
		Visual_Program_Config const cfg = {
				._width           = WIDTH,
				._height          = HEIGHT,
				._result_bytes    = RESULT_BYTES,
				._state_bytes     = feedback ? RESULT_BYTES : 4,
				._scratch_bytes   = 32,
				._parameter_bytes = 64,
				._previous_frame  = 1,
				._feedback        = feedback,
				._alpha_mode      = 0,
				._pass_count      = 2,
		};

		std::array<float, 8> limits{
				{
						.99f,
						.99f,
						.99f,
						.99f,
						1.01f,
						1.01f,
						1.01f,
						1.01f,
				},
		};
		std::array<uint32_t, 8> roi{
				{
						0,
						0,
						WIDTH,
						HEIGHT,
						WIDTH * HEIGHT,
						0,
						0,
						0,
				},
		};
		std::array<uint8_t, 64> parameters{};
		std::memcpy(parameters.data(), limits.data(), sizeof(limits));
		std::memcpy(parameters.data() + 32, roi.data(), sizeof(roi));
		std::array<Visual_Shader_Pass, 2> passes{};
		passes[0] = {
				._source      = SCAN_SOURCE,
				._source_size = static_cast<uint32_t>(std::strlen(SCAN_SOURCE)),
				._groups =
						{
								4,
								2,
								1,
						},
		};
		const char *reduce = feedback ? REDUCE_FEEDBACK_SOURCE : REDUCE_SOURCE;
		passes[1]          = {
				._source      = reduce,
				._source_size = static_cast<uint32_t>(std::strlen(reduce)),
				._groups =
						{
								1,
								1,
								1,
						},
		};

		Runtime_Program program(&context);
		probe_value(program.init(cfg, passes, parameters));
		std::array<const uint32_t, 4> patterns{
				{
						0xffffffffu,
						0u,
						0u,
						0xffffffffu,
				},
		};
		std::array<const uint32_t, 4> expected_flags{
				{
						1u,
						28u,
						16u,
						27u,
				},
		};
		std::array<uint32_t, 8> last{};
		for (uint32_t i = 0; i < 4; i++) {
			probe_value(program.capture(nullptr, patterns[i]));
			std::array<uint8_t, RESULT_BYTES> bytes{};
			const int                         n = probe_value(program.analyze_result(bytes.data()));
			require(std::cmp_equal(n, RESULT_BYTES), "runtime result size mismatch");
			std::memcpy(last.data(), bytes.data(), RESULT_BYTES);
			require(last[0] == expected_flags[i], "runtime event flags mismatch");
			require(last[5] == i + 1, "runtime processed sequence mismatch");
			if (patterns[i]) {
				require(last[1] == 0 && last[2] == 0 && last[3] == WIDTH - 1 && last[4] == HEIGHT - 1,
						"runtime full-frame bounds mismatch");
			} else {
				require(last[1] == 0 && last[2] == 0 && last[3] == 0 && last[4] == 0,
						"empty runtime result bounds were not cleared");
			}
			std::cout << R"({"mode":")" << (feedback ? "previousResult" : "separateState") << R"(","frame":)" << i + 1
					  << ",\"flags\":" << last[0] << ",\"bbox\":[" << last[1] << "," << last[2] << "," << last[3] << ","
					  << last[4] << "]}\n";
		}
		return last;
	}

	void validate_rejections(Context const &context) {
		VkPhysicalDeviceProperties properties{};
		vkGetPhysicalDeviceProperties(context._physical, &properties);
		Visual_Program_Config cfg{};
		auto const            expect_reject = [&](const char *source, const char *label) {
			auto words    = compile_for_probe(source);
			bool rejected = false;
			rejected      = !check_program_contract(words, cfg, properties.limits);
			require(rejected, label);
		};
		expect_reject(INVALID_BINDING_SOURCE, "binding 7 was accepted");
		expect_reject(WRITABLE_FRAME_SOURCE, "writable frame image was accepted");
	}


} // namespace

int main(int argc, char *const *argv) {
	{
		bool validation = false;
		for (int i = 1; i < argc; i++) {
			if (std::strcmp(argv[i], "--validation") == 0) {
				validation = true;
				enable_vulkan_validation();
			} else {
				probe_fail("unknown runtime probe argument");
			}
		}
		Context context;
		probe_value(context.init(226, 128));
		validate_rejections(context);
		run_sequence(context, false);
		run_sequence(context, true);
		check(vkDeviceWaitIdle(context._device), "wait for runtime probe completion");
		std::cerr << "RuntimeProgram: separate state and previous-result feedback "
					 "passed; "
				  << "invalid binding 7 and writable frame access rejected; only "
					 "32-byte results mapped"
				  << (validation ? " with Vulkan validation" : "") << '\n';
		return 0;
	}
}
