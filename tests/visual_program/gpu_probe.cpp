// Design experiment, not a production observer or an application controller.
// Reuse the existing DRM-matched context and GPU-only buffer allocation
// helpers.
#include "visual_gpu_internal.h"
#include "vulkan_probe_helpers.h"

#include "probe_programs.h"
#include <array>
#include <chrono>
#include <cstdint>
#include <cstring>
#include <iostream>
#include <ratio>
#include <vulkan/vulkan_core.h>

namespace {

	constexpr char GENERATE_SOURCE[] = R"(
#version 450
layout(local_size_x = 64) in;
layout(set = 0, binding = 0, rgba16f) uniform writeonly image2D frame;
layout(push_constant) uniform Frame {
	uint width;
	uint height;
	uint sequence;
	uint historyValid;
}
f;
void main() {
	uint i = gl_GlobalInvocationID.x;
	if (i >= f.width * f.height) {
		return;
	}
	ivec2 p   = ivec2(i % f.width, i / f.width);
	bool  hit = (f.sequence == 2u || f.sequence == 3u || f.sequence == 7u) && p == ivec2(7, 9);
	hit       = hit || (f.sequence == 4u && p == ivec2(11, 12));
	imageStore(frame, p, hit ? vec4(.2, .8, .3, 1) : vec4(0, 0, 0, 1));
}
)";


} // namespace

int main(int argc, char *const *argv) {
	{
		bool     feedback = false;
		uint32_t width = 64, height = 32;
		for (int i = 1; i < argc; i++) {
			if (std::strcmp(argv[i], "--feedback") == 0) {
				feedback = true;
			} else if (std::strcmp(argv[i], "--full-frame") == 0) {
				width  = 1280;
				height = 672;
			} else if (std::strcmp(argv[i], "--validation") == 0) {
				enable_vulkan_validation();
			} else {
				probe_fail("unknown probe argument");
			}
		}
		Context c;
		probe_value(c.init(226, 128)); // Real DRM device required; no software fallback.
		std::array<Texture, 2> textures{
				{
						Texture(c, width, height),
						Texture(c, width, height),
				},
		};
		std::array<Buffer, 2> states{};
		Buffer                scratch, result, params;
		const auto     usage = VkFlags{VK_BUFFER_USAGE_STORAGE_BUFFER_BIT} | VkFlags{VK_BUFFER_USAGE_TRANSFER_DST_BIT} |
							   VkFlags{VK_BUFFER_USAGE_TRANSFER_SRC_BIT};
		const uint32_t state_bytes = feedback ? 32 : 4;
		for (auto &b: states) {
			probe_value(b.init(&c, state_bytes, usage));
		}
		probe_value(scratch.init(&c, 32, usage));
		probe_value(result.init(&c, 32, usage,
								true)); // Only result bytes are mapped for GPU->CPU transfer.
		probe_value(params.init(&c, 64, usage,
								true)); // CPU-authored parameters, never framebuffer data.
		std::array<float, 8> ranges{
				{
						.19f,
						.79f,
						.29f,
						0,
						.21f,
						.81f,
						.31f,
						1,
				},
		};
		std::memcpy(params._mapped, ranges.data(), sizeof(ranges));
		std::array<uint32_t, 8> roi{
				{
						0,
						0,
						width,
						height,
						1,
						0,
						0,
						0,
				},
		};
		std::memcpy(static_cast<char *>(params._mapped) + 32, roi.data(), sizeof(roi));

		std::array<VkDescriptorSetLayoutBinding, 7> bs{};
		for (uint32_t i = 0; i < 7; i++) {
			bs[i] = {
					.binding            = i,
					.descriptorType     = i < 2 ? VK_DESCRIPTOR_TYPE_STORAGE_IMAGE : VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
					.descriptorCount    = 1,
					.stageFlags         = VK_SHADER_STAGE_COMPUTE_BIT,
					.pImmutableSamplers = nullptr,
			};
		}
		VkDescriptorSetLayoutCreateInfo const dl = {
				.sType        = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
				.pNext        = {},
				.flags        = {},
				.bindingCount = 7,
				.pBindings    = bs.data(),
		};
		VkDescriptorSetLayout layout{};
		check(vkCreateDescriptorSetLayout(c._device, &dl, nullptr, &layout), "probe descriptors");
		VkPushConstantRange const push{
				.stageFlags = VK_SHADER_STAGE_COMPUTE_BIT,
				.offset     = 0,
				.size       = 16,
		};
		VkPipelineLayoutCreateInfo const pl = {
				.sType                  = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
				.pNext                  = {},
				.flags                  = {},
				.setLayoutCount         = 1,
				.pSetLayouts            = &layout,
				.pushConstantRangeCount = 1,
				.pPushConstantRanges    = &push,
		};
		VkPipelineLayout pipeline_layout{};
		check(vkCreatePipelineLayout(c._device, &pl, nullptr, &pipeline_layout), "probe layout");
		std::array<VkPipeline, 3>             pipelines{};
		std::array<const std::string_view, 3> sources{
				{
						GENERATE_SOURCE,
						SCAN_SOURCE,
						feedback ? REDUCE_FEEDBACK_SOURCE : REDUCE_SOURCE,
				},
		};
		for (uint32_t i = 0; i < 3; i++) {
			auto                           words = compile_for_probe(sources[i]);
			VkShaderModuleCreateInfo const sm    = {
					.sType    = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
					.pNext    = {},
					.flags    = {},
					.codeSize = words.size() * 4,
					.pCode    = words.data(),
			};
			VkShaderModule module{};
			check(vkCreateShaderModule(c._device, &sm, nullptr, &module), "in-memory shader module");
			VkComputePipelineCreateInfo const pc = {
					.sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
					.pNext = {},
					.flags = {},
					.stage =
							{
									.sType               = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
									.pNext               = {},
									.flags               = {},
									.stage               = VK_SHADER_STAGE_COMPUTE_BIT,
									.module              = module,
									.pName               = "main",
									.pSpecializationInfo = {},
							},
					.layout             = pipeline_layout,
					.basePipelineHandle = {},
					.basePipelineIndex  = {},
			};
			VkResult const r = vkCreateComputePipelines(c._device, VK_NULL_HANDLE, 1, &pc, nullptr, &pipelines[i]);
			vkDestroyShaderModule(c._device, module, nullptr);
			check(r, "probe pipeline");
		}
		std::array<VkDescriptorPoolSize, 2> ps{
				{
						{
								.type            = VK_DESCRIPTOR_TYPE_STORAGE_IMAGE,
								.descriptorCount = 2,
						},
						{
								.type            = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
								.descriptorCount = 5,
						},
				},
		};
		VkDescriptorPoolCreateInfo const dp = {
				.sType         = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
				.pNext         = {},
				.flags         = {},
				.maxSets       = 1,
				.poolSizeCount = 2,
				.pPoolSizes    = ps.data(),
		};
		VkDescriptorPool pool{};
		check(vkCreateDescriptorPool(c._device, &dp, nullptr, &pool), "probe descriptor pool");
		VkDescriptorSetAllocateInfo const da = {
				.sType              = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
				.pNext              = {},
				.descriptorPool     = pool,
				.descriptorSetCount = 1,
				.pSetLayouts        = &layout,
		};
		VkDescriptorSet set{};
		check(vkAllocateDescriptorSets(c._device, &da, &set), "probe descriptor set");
		VkCommandPoolCreateInfo const cp = {
				.sType            = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
				.pNext            = {},
				.flags            = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
				.queueFamilyIndex = c._family,
		};
		VkCommandPool command_pool{};
		check(vkCreateCommandPool(c._device, &cp, nullptr, &command_pool), "probe command pool");
		VkCommandBufferAllocateInfo const ca = {
				.sType              = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
				.pNext              = {},
				.commandPool        = command_pool,
				.level              = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
				.commandBufferCount = 1,
		};
		VkCommandBuffer cmd{};
		check(vkAllocateCommandBuffers(c._device, &ca, &cmd), "probe command buffer");
		VkFenceCreateInfo const fc = {
				.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO,
				.pNext = {},
				.flags = {},
		};
		VkFence fence{};
		check(vkCreateFence(c._device, &fc, nullptr, &fence), "probe fence");

		const auto loop_start = std::chrono::steady_clock::now();
		for (uint32_t frame = 1; frame <= 1007; frame++) {
			uint32_t const                       current = (frame - 1) % 2, previous = 1 - current;
			std::array<VkDescriptorImageInfo, 2> images{
					{
							{
									.sampler     = VK_NULL_HANDLE,
									.imageView   = textures[current]._view,
									.imageLayout = VK_IMAGE_LAYOUT_GENERAL,
							},
							{
									.sampler     = VK_NULL_HANDLE,
									.imageView   = textures[previous]._view,
									.imageLayout = VK_IMAGE_LAYOUT_GENERAL,
							},
					},
			};
			std::array<VkDescriptorBufferInfo, 5> buffers{
					{
							{
									.buffer = states[previous]._buffer,
									.offset = 0,
									.range  = state_bytes,
							},
							{
									.buffer = states[current]._buffer,
									.offset = 0,
									.range  = state_bytes,
							},
							{
									.buffer = scratch._buffer,
									.offset = 0,
									.range  = 32,
							},
							{
									.buffer = result._buffer,
									.offset = 0,
									.range  = 32,
							},
							{
									.buffer = params._buffer,
									.offset = 0,
									.range  = 64,
							},
					},
			};
			std::array<VkWriteDescriptorSet, 7> writes{};
			for (uint32_t i = 0; i < 7; i++) {
				writes[i] = {
						.sType            = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
						.pNext            = {},
						.dstSet           = set,
						.dstBinding       = i,
						.dstArrayElement  = {},
						.descriptorCount  = 1,
						.descriptorType   = bs[i].descriptorType,
						.pImageInfo       = {},
						.pBufferInfo      = {},
						.pTexelBufferView = {},
				};
				if (i < 2) {
					writes[i].pImageInfo = &images[i];
				} else {
					writes[i].pBufferInfo = &buffers[i - 2];
				}
			}
			vkUpdateDescriptorSets(c._device, 7, writes.data(), 0, nullptr);
			check(vkResetCommandBuffer(cmd, 0), "reset probe commands");
			VkCommandBufferBeginInfo const begin = {
					.sType            = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
					.pNext            = {},
					.flags            = {},
					.pInheritanceInfo = {},
			};
			check(vkBeginCommandBuffer(cmd, &begin), "begin probe commands");
			const bool reset = frame == 1 || frame == 7;
			if (frame == 1) {
				for (auto const &t: textures) {
					VkImageMemoryBarrier const b = {
							.sType               = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
							.pNext               = {},
							.srcAccessMask       = {},
							.dstAccessMask       = VK_ACCESS_TRANSFER_WRITE_BIT,
							.oldLayout           = VK_IMAGE_LAYOUT_UNDEFINED,
							.newLayout           = VK_IMAGE_LAYOUT_GENERAL,
							.srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
							.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
							.image               = t._image,
							.subresourceRange =
									{
											.aspectMask     = VK_IMAGE_ASPECT_COLOR_BIT,
											.baseMipLevel   = 0,
											.levelCount     = 1,
											.baseArrayLayer = 0,
											.layerCount     = 1,
									},
					};
					vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0,
										 nullptr, 0, nullptr, 1, &b);
					VkClearColorValue const black{};
					vkCmdClearColorImage(cmd, t._image, VK_IMAGE_LAYOUT_GENERAL, &black, 1, &b.subresourceRange);
				}
			}
			barrier(cmd,
					VkFlags{VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT} | VkFlags{VK_PIPELINE_STAGE_HOST_BIT} |
							VkFlags{VK_PIPELINE_STAGE_TRANSFER_BIT},
					VkFlags{VK_PIPELINE_STAGE_TRANSFER_BIT} | VkFlags{VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT},
					VkFlags{VK_ACCESS_SHADER_WRITE_BIT} | VkFlags{VK_ACCESS_HOST_WRITE_BIT} |
							VkFlags{VK_ACCESS_TRANSFER_WRITE_BIT},
					VkFlags{VK_ACCESS_TRANSFER_WRITE_BIT} | VkFlags{VK_ACCESS_TRANSFER_READ_BIT} |
							VkFlags{VK_ACCESS_SHADER_READ_BIT} | VkFlags{VK_ACCESS_SHADER_WRITE_BIT});
			vkCmdFillBuffer(cmd, scratch._buffer, 0, 32, 0);
			vkCmdFillBuffer(cmd, result._buffer, 0, 32, 0);
			if (reset) {
				for (auto const &b: states) {
					vkCmdFillBuffer(cmd, b._buffer, 0, state_bytes, 0);
				}
			} else {
				VkBufferCopy const copy{
						.srcOffset = 0,
						.dstOffset = 0,
						.size      = state_bytes,
				};
				vkCmdCopyBuffer(cmd, states[previous]._buffer, states[current]._buffer, 1, &copy);
			}
			barrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
					VK_ACCESS_TRANSFER_WRITE_BIT,
					VkFlags{VK_ACCESS_SHADER_READ_BIT} | VkFlags{VK_ACCESS_SHADER_WRITE_BIT});
			std::array<uint32_t, 4> meta{
					{
							width,
							height,
							frame,
							reset ? 0u : 1u,
					},
			};
			vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, pipeline_layout, 0, 1, &set, 0, nullptr);
			vkCmdPushConstants(cmd, pipeline_layout, VK_SHADER_STAGE_COMPUTE_BIT, 0, 16, meta.data());
			for (uint32_t pass = 0; pass < 3; pass++) {
				vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, pipelines[pass]);
				vkCmdDispatch(cmd,
							  pass == 0   ? (width * height + 63) / 64
							  : pass == 1 ? (width + 15) / 16
										  : 1,
							  pass == 1 ? (height + 15) / 16 : 1, 1);
				barrier(cmd, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
						pass == 2 ? VkFlags{VK_PIPELINE_STAGE_HOST_BIT} | VkFlags{VK_PIPELINE_STAGE_TRANSFER_BIT}
								  : VkFlags{VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT},
						VK_ACCESS_SHADER_WRITE_BIT,
						pass == 2 ? VkFlags{VK_ACCESS_HOST_READ_BIT} | VkFlags{VK_ACCESS_TRANSFER_READ_BIT}
								  : VkFlags{VK_ACCESS_SHADER_READ_BIT} | VkFlags{VK_ACCESS_SHADER_WRITE_BIT});
			}
			if (feedback) {
				barrier(cmd, VkFlags{VK_PIPELINE_STAGE_TRANSFER_BIT} | VkFlags{VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT},
						VK_PIPELINE_STAGE_TRANSFER_BIT,
						VkFlags{VK_ACCESS_TRANSFER_WRITE_BIT} | VkFlags{VK_ACCESS_SHADER_WRITE_BIT},
						VkFlags{VK_ACCESS_TRANSFER_READ_BIT} | VkFlags{VK_ACCESS_TRANSFER_WRITE_BIT});
				VkBufferCopy const copy{
						.srcOffset = 0,
						.dstOffset = 0,
						.size      = 32,
				};
				vkCmdCopyBuffer(cmd, result._buffer, states[current]._buffer, 1, &copy);
				barrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
						VK_ACCESS_TRANSFER_WRITE_BIT, VK_ACCESS_SHADER_READ_BIT);
			}
			check(vkEndCommandBuffer(cmd), "end probe commands");
			check(vkResetFences(c._device, 1, &fence), "reset probe fence");
			VkSubmitInfo const submit = {
					.sType                = VK_STRUCTURE_TYPE_SUBMIT_INFO,
					.pNext                = {},
					.waitSemaphoreCount   = {},
					.pWaitSemaphores      = {},
					.pWaitDstStageMask    = {},
					.commandBufferCount   = 1,
					.pCommandBuffers      = &cmd,
					.signalSemaphoreCount = {},
					.pSignalSemaphores    = {},
			};
			check(vkQueueSubmit(c._queue, 1, &submit, fence), "submit probe");
			check(vkWaitForFences(c._device, 1, &fence, VK_TRUE, 5'000'000'000ull), "complete probe");
			std::array<uint32_t, 8> values{};
			std::memcpy(values.data(), result._mapped, 32);
			if (frame <= 7) {
				std::cout << "{\"frame\":" << frame << ",\"words\":[";
				for (int i = 0; i < 8; i++) {
					std::cout << (i ? "," : "") << values[i];
				}
				std::cout << "]}\n";
			}
			if (frame > 7 && (values[0] != (frame == 8 ? 28u : 16u) || values[5] != frame || values[6] || values[7])) {
				probe_fail("steady-state result/reset validation failed");
			}
		}
		const double ms =
				std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - loop_start).count();
		std::cerr << (feedback ? "previous-result feedback" : "separate state") << ": " << width << "x" << height
				  << "; 1007 submissions; mean CPU submission/completion time " << ms / 1007
				  << " ms; frames/history/state/scratch never mapped\n";
		check(vkDeviceWaitIdle(c._device), "finish probe");
		vkDestroyFence(c._device, fence, nullptr);
		vkDestroyCommandPool(c._device, command_pool, nullptr);
		vkDestroyDescriptorPool(c._device, pool, nullptr);
		for (auto p: pipelines) {
			vkDestroyPipeline(c._device, p, nullptr);
		}
		vkDestroyPipelineLayout(c._device, pipeline_layout, nullptr);
		vkDestroyDescriptorSetLayout(c._device, layout, nullptr);
		return 0;
	}
}
