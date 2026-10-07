// General ABI/synchronization checks; GPU-created inputs, bounded result only.
#include "visual_gpu_internal.h"
#include "vulkan_probe_helpers.h"

#include "visual_gpu.h"
#include "visual_program_gpu.h"
#include <array>
#include <cmath>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <iostream>
#include <string>
#include <sys/mman.h>
#include <sys/stat.h>
#include <type_traits>
#include <unistd.h>
#include <utility>
#include <vulkan/vulkan_core.h>

namespace {

	void import_cache_layout(Context &context) {
		int const fd = memfd_create("dmabuf-layout-identity", MFD_CLOEXEC);
		if (fd < 0) {
			probe_fail("cannot create layout identity fixture");
		}
		struct stat stat{};
		if (fstat(fd, &stat) < 0) {
			close(fd);
			probe_fail("fixture identity unavailable");
		}
		Visual_Plane plane{
				._fd       = fd,
				._offset   = 0,
				._stride   = 256,
				._modifier = 0,
		};
		Imported entry(&context);
		entry._key               = stat.st_ino;
		entry._allocation_device = stat.st_dev;
		entry._width             = 64;
		entry._height            = 32;
		entry._format            = 0x34324241u;
		entry._plane_layout      = {plane};
		auto const matches = [&] { return entry.matches(stat.st_ino, 64, 32, 0x34324241u, std::span(&plane, 1)); };
		if (!matches()) {
			probe_fail("matching import identity rejected");
		}
		for (auto const member: {
					 &Visual_Plane::_offset,
					 &Visual_Plane::_stride,
			 }) {
			plane.*member += 64;
			if (matches()) {
				probe_fail("different plane layout reused cached image");
			}
			plane.*member -= 64;
		}
		plane._modifier = 1;
		if (matches()) {
			probe_fail("different modifier reused cached image");
		}
		plane._modifier = 0;
		if (entry.matches(stat.st_ino, 32, 32, entry._format, std::span(&plane, 1)) ||
			entry.matches(stat.st_ino, 64, 32, 0x30334241u, std::span(&plane, 1))) {
			probe_fail("different geometry/format reused cached image");
		}
		close(fd);
	}

	constexpr char COLOR_CHECK_SOURCE[] = R"GLSL(
#version 450
layout(local_size_x = 1) in;
layout(set = 0, binding = 0, rgba16f) readonly uniform image2D current;
layout(set = 0, binding = 2, std430) readonly buffer Previous { uint count; }
previous;
layout(set = 0, binding = 3, std430) buffer Next { uint count; }
nextState;
layout(set = 0, binding = 5, std430) buffer Result { uint failed, history, count, sequence; }
result;
layout(set = 0, binding = 6, std430) readonly buffer Reference { vec4 expected; }
reference;
layout(push_constant) uniform Frame { uint width, height, sequence, historyValid; }
frame;
void main() {
	vec4  observed = imageLoad(current, ivec2(0));
	bvec4 valid    = lessThanEqual(abs(observed - reference.expected), max(vec4(.002), abs(reference.expected) * .002));
	result.failed  = all(valid) ? 0u : 1u;
	result.history = frame.historyValid;
	result.count   = previous.count;
	result.sequence = frame.sequence;
	nextState.count = previous.count + 1u;
}
)GLSL";

	void color_formats_and_history(Context &context) {
		Visual_Program_Config const cfg{
				._width           = 1,
				._height          = 1,
				._result_bytes    = 16,
				._state_bytes     = 4,
				._scratch_bytes   = 4,
				._parameter_bytes = 16,
				._previous_frame  = 1,
				._feedback        = 0,
				._alpha_mode      = 0,
				._pass_count      = 1,
		};
		Visual_Shader_Pass pass{
				._source      = COLOR_CHECK_SOURCE,
				._source_size = static_cast<uint32_t>(std::strlen(COLOR_CHECK_SOURCE)),
				._groups =
						{
								1,
								1,
								1,
						},
		};
		std::array<float, 4> expected{};
		Runtime_Program      program(&context);
		probe_value(program.init(cfg, std::span(&pass, 1),
								 std::span(reinterpret_cast<const uint8_t *>(expected.data()), sizeof(expected))));
		auto const sample = [&](uint32_t format, std::array<uint32_t, 2> words, Visual_Color_Profile profile,
								std::array<float, 4> reference, int history = -1, int state = -1) {
			program.set_color(profile);
			probe_value(program.capture(nullptr, 0));
			program._source_format = format;
			std::memcpy(program._parameters._mapped, reference.data(), 16);
			probe_value(program.begin_commands());
			program.barrier(VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT,
							VK_ACCESS_TRANSFER_WRITE_BIT, VK_ACCESS_TRANSFER_WRITE_BIT);
			vkCmdUpdateBuffer(program._cmd, program._pixels._buffer, 0, 8, words.data());
			probe_value(program.submit_wait(false));
			std::array<uint32_t, 4> result{};
			probe_value(program.analyze_result(reinterpret_cast<uint8_t *>(result.data())));
			if (result[0] || (history >= 0 && std::cmp_not_equal(result[1], history)) ||
				(state >= 0 && std::cmp_not_equal(result[2], state))) {
				probe_fail("GPU source color/format/history mismatch format=" + std::to_string(format) +
						   " transfer=" + std::to_string(profile._transfer));
			}
			if (program._pixels._mapped || program._states[0]._mapped || program._states[1]._mapped ||
				program._scratch._mapped) {
				probe_fail("frame/state/scratch must stay unmapped");
			}
		};
		auto srgb        = default_visual_color();
		srgb._alpha_mode = 2;
		const std::array<float, 4> red{
				.6274039f,
				.06909729f,
				.01639144f,
				1,
		};
		for (uint32_t const format: {
					 0x34325258u,
					 0x34325241u,
					 0x34324258u,
					 0x34324241u,
					 0x30335258u,
					 0x30335241u,
					 0x30334258u,
					 0x30334241u,
					 0x48344258u,
					 0x48344241u,
			 }) {
			uint32_t const packed = format == 0x34325258u || format == 0x34325241u   ? 0xffff0000u
									: format == 0x34324258u || format == 0x34324241u ? 0xff0000ffu
									: format == 0x30335258u || format == 0x30335241u ? 0xfff00000u
																					 : 0xc00003ffu;
			auto words = format == 0x48344258u || format == 0x48344241u ? std::array<uint32_t, 2>{0x00003c00u, 0x3c000000u,}
																	: std::array<uint32_t, 2>{packed, 0,};
			// X channels are undefined padding, never transparency.
			if (format == 0x34325258u || format == 0x34324258u) {
				words[0] &= 0x00ffffffu;
			}
			if (format == 0x30335258u || format == 0x30334258u) {
				words[0] &= 0x3fffffffu;
			}
			if (format == 0x48344258u) {
				words[1] = 0;
			}
			sample(format, words, srgb, red);
		}
		sample(0x30334241u,
			   {
					   0x800002aau,
					   0,
			   },
			   srgb,
			   {
					   red[0],
					   red[1],
					   red[2],
					   2.f / 3,
			   });
		srgb._alpha_mode = 0;
		// Independently evaluated transfer reference values at encoded E=0.5.
		std::array<const float, 15> transfers{
				{
						0,
						.18946457f,
						.21763764f,
						.1435873f,
						.26503573f,
						.5f,
						.1f,
						.05623413f,
						.2595894f,
						.21404114f,
						.21404114f,
						.009224571f,
						.179957f,
						.083333333f,
						.21404114f,
				},
		};
		for (uint32_t tf = 1; tf <= 14; tf++) {
			auto profile      = srgb;
			profile._transfer = tf;
			float value       = transfers[tf];
			if (tf == 13) {
				profile._maximum   = 1000;
				profile._reference = 203;
				value              = 1000.f / 203 * std::pow(1.f / 12, 1.2f);
			}
			if (tf == 11) {
				profile._maximum   = 10000;
				profile._reference = 203;
				value *= 10000.f / 203;
			}
			sample(0x48344241u,
				   {
						   0x38003800u,
						   0x3c003800u,
				   },
				   profile,
				   {
						   value,
						   value,
						   value,
						   1,
				   });
		}
		auto hlg       = srgb;
		hlg._transfer  = 13;
		hlg._maximum   = 100;
		hlg._reference = 100;
		sample(0x48344241u,
			   {
					   0,
					   0x3c000000u,
			   },
			   hlg,
			   {
					   0,
					   0,
					   0,
					   1,
			   });
		auto linear      = srgb;
		linear._transfer = 5;
		// Identity BT.2020 primaries: negative values and HDR highlights survive.
		std::memset(linear._matrix, 0, sizeof(linear._matrix));
		for (int i = 0; i < 3; i++) {
			linear._matrix[i][i] = 1;
		}
		sample(0x48344241u,
			   {
					   0xb4004000u,
					   0x3c003800u,
			   },
			   linear,
			   {
					   2,
					   -.25f,
					   .5f,
					   1,
			   },
			   0, 0);
		sample(0x48344241u,
			   {
					   0xb4004000u,
					   0x3c003800u,
			   },
			   linear,
			   {
					   2,
					   -.25f,
					   .5f,
					   1,
			   },
			   1, 1);
		auto equivalent          = linear;
		equivalent._matrix[0][1] = -0.0f;
		sample(0x48344241u,
			   {
					   0xb4004000u,
					   0x3c003800u,
			   },
			   equivalent,
			   {
					   2,
					   -.25f,
					   .5f,
					   1,
			   },
			   1, 2);
		linear._power     = 2.2f;
		const float gamma = std::pow(.5f, 2.2f);
		sample(0x48344241u,
			   {
					   0x38003800u,
					   0x3c003800u,
			   },
			   linear,
			   {
					   gamma,
					   gamma,
					   gamma,
					   1,
			   },
			   0, 0);
		// Encoded premultiplied alpha is decoded before transfer conversion.
		linear._power      = 0;
		linear._alpha_mode = 2;
		sample(0x48344241u,
			   {
					   0x00003c00u,
					   0x38000000u,
			   },
			   linear,
			   {
					   2,
					   0,
					   0,
					   .5f,
			   },
			   0, 0);
		sample(0x48344241u,
			   {
					   0x00003c00u,
					   0x00000000u,
			   },
			   linear,
			   {
					   0,
					   0,
					   0,
					   0,
			   });
		std::cout << "All 10 RGB DMA-BUF formats, all 14 transfer functions, PQ/HLG "
					 "luminance, FP16 HDR/negative values, premultiplied alpha and "
					 "color-change history reset passed on GPU.\n";
	}

	constexpr char PARTIAL_SOURCE[] = R"GLSL(
#version 450
layout(local_size_x = 1) in;
layout(set = 0, binding = 5, std430) buffer Result { uint words[]; }
result;
layout(push_constant) uniform Frame { uint width, height, sequence, historyValid; }
frame;
void main() {
	// First frame writes every field; second writes just one; third writes none.
	if (frame.sequence == 1u) {
		for (uint i = 0u; i < 8u; i++) {
			result.words[i] = i + 1u;
		}
	}
	if (frame.sequence == 2u) {
		result.words[0] = 9u;
	}
}
)GLSL";
	constexpr char OVERSIZED_PUSH[] = R"GLSL(
#version 450
layout(local_size_x = 1) in;
layout(set = 0, binding = 5, std430) buffer Result { uint value; }
result;
layout(push_constant) uniform Frame { uint width, height, sequence, historyValid, extra; }
frame;
void main() { result.value = frame.extra; }
)GLSL";
	void           ffi_error_recovery(Context &context) {
		static_assert(!std::is_copy_constructible_v<Context>);
		static_assert(!std::is_copy_constructible_v<Buffer>);
		static_assert(!std::is_copy_constructible_v<Imported>);
		static_assert(!std::is_copy_constructible_v<Runtime_Program>);
		if (context.memory_type(0, 0)) {
			probe_fail("invalid memory type unexpectedly succeeded");
		}
		Visual_Program_Config cfg{
				._width           = 8,
				._height          = 8,
				._result_bytes    = 32,
				._state_bytes     = 32,
				._scratch_bytes   = 4,
				._parameter_bytes = 0,
				._previous_frame  = 0,
				._feedback        = 0,
				._alpha_mode      = 0,
				._pass_count      = 2,
		};
		constexpr char                    INVALID[] = "#version 450\ninvalid GLSL";
		std::array<Visual_Shader_Pass, 2> passes{
				{
						{
								._source      = PARTIAL_SOURCE,
								._source_size = static_cast<uint32_t>(std::strlen(PARTIAL_SOURCE)),
								._groups =
										{
												1,
												1,
												1,
										},
						},
						{
								._source      = INVALID,
								._source_size = static_cast<uint32_t>((sizeof(INVALID) - 1)),
								._groups =
										{
												1,
												1,
												1,
										},
						},
				},
		};
		for (unsigned i = 0; i < 8; i++) {
			std::array<char, 512> error{};
			auto const           *failed = visual_runtime_create(&context, &cfg, passes.data(), nullptr, error.data());
			if (failed || error[0] == 0 || error.back() != 0) {
				probe_fail("failed compilation did not return a bounded FFI diagnostic");
			}
		}
		cfg._pass_count = 1;
		std::array<char, 512> error{};
		auto                 *program = visual_runtime_create(&context, &cfg, passes.data(), nullptr, error.data());
		if (!program) {
			probe_fail(error.data());
		}
		visual_runtime_destroy(program);
		// Missing type IDs must be diagnosed instead of invoking map::at's throw.
		VkPhysicalDeviceProperties properties{};
		vkGetPhysicalDeviceProperties(context._physical, &properties);
		auto words = compile_for_probe(PARTIAL_SOURCE);
		for (std::size_t i = 5; i < words.size(); i += words[i] >> 16u) {
			if ((words[i] & 65535u) == 59) {
				words[i + 1] = 0xffffffffu;
				break;
			}
		}
		if (check_program_contract(words, cfg, properties.limits)) {
			probe_fail("missing SPIR-V type accepted");
		}
		std::cout << "Failed partial program initialization returned bounded errors; "
					 "subsequent creation recovered; resource owners cannot copy.\n";
	}


} // namespace

int main(int argc, char *const *argv) {
	{
		if (argc == 2 && std::strcmp(argv[1], "--validation") == 0) {
			enable_vulkan_validation();
		} else if (argc != 1) {
			probe_fail("unknown argument");
		}
		Context context;
		probe_value(context.init(226, 128));
		import_cache_layout(context);
		ffi_error_recovery(context);
		color_formats_and_history(context);
		for (uint32_t feedback = 0; feedback < 2; feedback++) {
			Visual_Program_Config const cfg{
					._width           = 64,
					._height          = 32,
					._result_bytes    = 32,
					._state_bytes     = 32,
					._scratch_bytes   = 4,
					._parameter_bytes = 0,
					._previous_frame  = 0,
					._feedback        = feedback,
					._alpha_mode      = 0,
					._pass_count      = 1,
			};
			Visual_Shader_Pass pass{
					._source      = PARTIAL_SOURCE,
					._source_size = static_cast<uint32_t>(std::strlen(PARTIAL_SOURCE)),
					._groups =
							{
									1,
									1,
									1,
							},
			};
			Runtime_Program program(&context);
			probe_value(program.init(cfg, std::span(&pass, 1), {}));
			for (uint32_t frame = 1; frame <= 3; frame++) {
				probe_value(program.capture(nullptr, 0));
				std::array<uint32_t, 8> result;
				result.fill(0xffffffffu);
				if (probe_value(program.analyze_result(reinterpret_cast<uint8_t *>(result.data()))) != 32) {
					probe_fail("result size changed");
				}
				for (uint32_t i = 0; i < 8; i++) {
					uint32_t const expected = frame == 1 ? i + 1u : (frame == 2 && i == 0 ? 9u : 0u);
					if (result[i] != expected) {
						probe_fail("unwritten result field retained stale data");
					}
				}
			}
		}
		{
			Program snapshot(&context);
			probe_value(snapshot.init_raw(4, 4));
			probe_value(snapshot.capture(nullptr, 0xaabbccddu));
			if (snapshot._pixels._mapped) {
				probe_fail("owned screenshot source mapped");
			}
			std::array<uint8_t, 64> raw{};
			probe_value(snapshot.read_snapshot(raw.data(), raw.size()));
			std::array<const uint8_t, 4> expected{
					{
							0xdd,
							0xcc,
							0xbb,
							0xaa,
					},
			};
			for (size_t i = 0; i < raw.size(); i++) {
				if (raw[i] != expected[i % 4]) {
					probe_fail("in-memory screenshot readback mismatch");
				}
			}
			bool rejected = false;
			rejected      = !snapshot.read_snapshot(raw.data(), raw.size() - 1);
			if (!rejected) {
				probe_fail("incorrect screenshot output size accepted");
			}
		}
		{
			Cached_Snapshot_Native snapshot(&context, 4, 4, 0x34324241u);
			probe_value(snapshot.init());
			for (uint32_t const pattern: {
						 0xff000000u,
						 0xffaabbccu,
						 0xffffffffu,
				 }) {
				snapshot._raw._captured = false;
				probe_value(snapshot._raw.capture(nullptr, pattern));
				std::array<uint32_t, 16> pixels{};
				char                     error[512]{};
				if (visual_snapshot_read(&snapshot, reinterpret_cast<uint8_t *>(pixels.data()), 64, error) < 0) {
					probe_fail(error);
				}
				for (auto const pixel: pixels) {
					if (pixel != pattern) {
						probe_fail("retained GPU frame mismatch");
					}
				}
				pixels.fill(0);
				if (visual_snapshot_read(&snapshot, reinterpret_cast<uint8_t *>(pixels.data()), 64, error) < 0) {
					probe_fail(error);
				}
				for (auto const pixel: pixels) {
					if (pixel != pattern) {
						probe_fail("idle GPU snapshot lost");
					}
				}
			}
		}
		VkPhysicalDeviceProperties properties;
		vkGetPhysicalDeviceProperties(context._physical, &properties);
		Visual_Program_Config const cfg{};
		bool                        rejected = false;
		rejected = !check_program_contract(compile_for_probe(OVERSIZED_PUSH), cfg, properties.limits);
		if (!rejected) {
			probe_fail("oversized push block accepted");
		}
		std::cout << "Both feedback modes: zero/partial writes clear stale "
					 "results; oversized push rejected. Observer results32B; "
					 "explicit screenshot64B in memory, no image files.\n";
	}
}
