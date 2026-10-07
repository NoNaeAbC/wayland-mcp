#include "visual_gpu_internal.h"
#include "vulkan_probe_helpers.h"

#include "../../src/visual_normalize_shader.h"
#include "color_reference.h"
#include "visual_gpu.h"
#include <array>
#include <chrono>
#include <cmath>
#include <cstdint>
#include <cstring>
#include <iostream>
#include <ratio>
#include <string>
#include <vulkan/vulkan_core.h>

namespace {

	constexpr char GENERATOR[] = R"(
#version 450
layout(local_size_x = 64) in;
layout(set = 0, binding = 0, std430) buffer Raw { uint pixels[]; }
raw;
layout(push_constant) uniform Source {
	uint width;
	uint height;
	uint rgbaOrder;
	uint alphaMode;
}
source;
void main() {
	uint index = gl_GlobalInvocationID.x;
	if (index >= source.width * source.height) {
		return;
	}
	const uvec4 colors[12] = uvec4[12](uvec4(0, 0, 0, 255), uvec4(255), uvec4(255, 0, 0, 255), uvec4(0, 255, 0, 255),
									   uvec4(0, 0, 255, 255), uvec4(64, 64, 64, 255), uvec4(128, 128, 128, 255),
									   uvec4(192, 192, 192, 255), uvec4(64, 128, 192, 255), uvec4(80, 120, 160, 0),
									   uvec4(255, 0, 0, 128), uvec4(128, 0, 0, 128));
	uvec4       v          = index < 12u ? colors[index] : uvec4(0, 0, 0, 255);
	if (source.rgbaOrder == 0u) {
		v = v.bgra;
	}
	raw.pixels[index] = v.x | (v.y << 8) | (v.z << 16) | (v.w << 24);
}
)";

	struct Reference {
		std::array<uint32_t, 4> _xybit;
		std::array<float, 4>    _low, _high;
	};
	static_assert(sizeof(Reference) == 48);


} // namespace

int main(int argc, char *const *argv) {
	{
		uint32_t width = 64, height = 32, runs = 6;
		bool     wrong_reference = false;
		for (int i = 1; i < argc; i++) {
			if (std::strcmp(argv[i], "--full-frame") == 0) {
				width  = 1280;
				height = 672;
				runs   = 1002;
			} else if (std::strcmp(argv[i], "--wrong-reference") == 0) {
				wrong_reference = true;
			} else if (std::strcmp(argv[i], "--validation") == 0) {
				enable_vulkan_validation();
			} else {
				probe_fail("unknown normalization probe option");
			}
		}
		Context c;
		probe_value(c.init(226, 128));
		Texture const canonical(c, width, height);
		Buffer        raw, result, params, color;
		probe_value(raw.init(&c, static_cast<uint64_t>(width) * height * 4, VK_BUFFER_USAGE_STORAGE_BUFFER_BIT));
		probe_value(result.init(
				&c, 16, VkFlags{VK_BUFFER_USAGE_STORAGE_BUFFER_BIT} | VkFlags{VK_BUFFER_USAGE_TRANSFER_DST_BIT}, true));
		probe_value(params.init(&c, 16 + 12 * sizeof(Reference), VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, true));

		probe_value(color.init(&c, sizeof(Visual_Color_Profile), VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, true));
		std::array<VkDescriptorSetLayoutBinding, 3> nb{
				{
						{
								.binding            = 0,
								.descriptorType     = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
								.descriptorCount    = 1,
								.stageFlags         = VK_SHADER_STAGE_COMPUTE_BIT,
								.pImmutableSamplers = nullptr,
						},
						{
								.binding            = 1,
								.descriptorType     = VK_DESCRIPTOR_TYPE_STORAGE_IMAGE,
								.descriptorCount    = 1,
								.stageFlags         = VK_SHADER_STAGE_COMPUTE_BIT,
								.pImmutableSamplers = nullptr,
						},
						{
								.binding            = 2,
								.descriptorType     = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
								.descriptorCount    = 1,
								.stageFlags         = VK_SHADER_STAGE_COMPUTE_BIT,
								.pImmutableSamplers = nullptr,
						},
				},
		};
		std::array<VkDescriptorSetLayoutBinding, 3> cb{
				{
						{
								.binding            = 0,
								.descriptorType     = VK_DESCRIPTOR_TYPE_STORAGE_IMAGE,
								.descriptorCount    = 1,
								.stageFlags         = VK_SHADER_STAGE_COMPUTE_BIT,
								.pImmutableSamplers = nullptr,
						},
						{
								.binding            = 5,
								.descriptorType     = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
								.descriptorCount    = 1,
								.stageFlags         = VK_SHADER_STAGE_COMPUTE_BIT,
								.pImmutableSamplers = nullptr,
						},
						{
								.binding            = 6,
								.descriptorType     = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
								.descriptorCount    = 1,
								.stageFlags         = VK_SHADER_STAGE_COMPUTE_BIT,
								.pImmutableSamplers = nullptr,
						},
				},
		};
		std::array<VkDescriptorSetLayout, 2> layouts{};
		std::array<VkPipelineLayout, 2>      pipeline_layouts{};
		for (int i = 0; i < 2; i++) {
			VkDescriptorSetLayoutCreateInfo const l = {
					.sType        = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
					.pNext        = {},
					.flags        = {},
					.bindingCount = 3,
					.pBindings    = i ? cb.data() : nb.data(),
			};
			check(vkCreateDescriptorSetLayout(c._device, &l, nullptr, &layouts[i]), "normalization descriptors");
			VkPushConstantRange const push{
					.stageFlags = VK_SHADER_STAGE_COMPUTE_BIT,
					.offset     = 0,
					.size       = 16,
			};
			VkPipelineLayoutCreateInfo p = {
					.sType                  = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
					.pNext                  = {},
					.flags                  = {},
					.setLayoutCount         = 1,
					.pSetLayouts            = &layouts[i],
					.pushConstantRangeCount = {},
					.pPushConstantRanges    = {},
			};
			if (!i) {
				p.pushConstantRangeCount = 1;
				p.pPushConstantRanges    = &push;
			}
			check(vkCreatePipelineLayout(c._device, &p, nullptr, &pipeline_layouts[i]),
				  "normalization pipeline layout");
		}
		std::array<VkPipeline, 3> pipelines{
				{
						compute_pipeline(c, pipeline_layouts[0], GENERATOR),
						compute_pipeline(c, pipeline_layouts[0], visual_normalize_source.c_str()),
						compute_pipeline(c, pipeline_layouts[1], COLOR_REFERENCE_SOURCE),
				},
		};
		std::array<VkDescriptorPoolSize, 2> sizes{
				{
						{
								.type            = VK_DESCRIPTOR_TYPE_STORAGE_IMAGE,
								.descriptorCount = 2,
						},
						{
								.type            = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
								.descriptorCount = 4,
						},
				},
		};
		VkDescriptorPoolCreateInfo const pool_info = {
				.sType         = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
				.pNext         = {},
				.flags         = {},
				.maxSets       = 2,
				.poolSizeCount = 2,
				.pPoolSizes    = sizes.data(),
		};
		VkDescriptorPool descriptor_pool{};
		check(vkCreateDescriptorPool(c._device, &pool_info, nullptr, &descriptor_pool),
			  "normalization descriptor pool");
		VkDescriptorSetAllocateInfo const a = {
				.sType              = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
				.pNext              = {},
				.descriptorPool     = descriptor_pool,
				.descriptorSetCount = 2,
				.pSetLayouts        = layouts.data(),
		};
		std::array<VkDescriptorSet, 2> sets{};
		check(vkAllocateDescriptorSets(c._device, &a, sets.data()), "normalization descriptor sets");
		VkDescriptorImageInfo const image{
				.sampler     = VK_NULL_HANDLE,
				.imageView   = canonical._view,
				.imageLayout = VK_IMAGE_LAYOUT_GENERAL,
		};
		std::array<VkDescriptorBufferInfo, 4> infos{
				{
						{
								.buffer = raw._buffer,
								.offset = 0,
								.range  = raw._size,
						},
						{
								.buffer = result._buffer,
								.offset = 0,
								.range  = 16,
						},
						{
								.buffer = params._buffer,
								.offset = 0,
								.range  = params._size,
						},
						{
								.buffer = color._buffer,
								.offset = 0,
								.range  = color._size,
						},
				},
		};
		std::array<VkWriteDescriptorSet, 6> writes{};
		std::array<const uint32_t, 6>       bindings{
				{
						0,
						1,
						0,
						5,
						6,
						2,
				},
		};
		for (int i = 0; i < 6; i++) {
			writes[i] = {
					.sType           = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
					.pNext           = {},
					.dstSet          = sets[(i < 2 || i == 5) ? 0 : 1],
					.dstBinding      = bindings[i],
					.dstArrayElement = {},
					.descriptorCount = 1,
					.descriptorType =
							i == 1 || i == 2 ? VK_DESCRIPTOR_TYPE_STORAGE_IMAGE : VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
					.pImageInfo       = {},
					.pBufferInfo      = {},
					.pTexelBufferView = {},
			};
			if (i == 1 || i == 2) {
				writes[i].pImageInfo = &image;
			} else {
				writes[i].pBufferInfo = &infos[i == 0 ? 0 : i == 3 ? 1 : i == 5 ? 3 : 2];
			}
		}
		vkUpdateDescriptorSets(c._device, 6, writes.data(), 0, nullptr);
		VkCommandPoolCreateInfo const cp = {
				.sType            = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
				.pNext            = {},
				.flags            = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
				.queueFamilyIndex = c._family,
		};
		VkCommandPool pool{};
		check(vkCreateCommandPool(c._device, &cp, nullptr, &pool), "normalization command pool");
		VkCommandBufferAllocateInfo const ca = {
				.sType              = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
				.pNext              = {},
				.commandPool        = pool,
				.level              = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
				.commandBufferCount = 1,
		};
		VkCommandBuffer cmd{};
		check(vkAllocateCommandBuffers(c._device, &ca, &cmd), "normalization command buffer");
		VkFenceCreateInfo const fc = {
				.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO,
				.pNext = {},
				.flags = {},
		};
		VkFence fence{};
		check(vkCreateFence(c._device, &fc, nullptr, &fence), "normalization fence");
		const std::array<std::array<float, 4>, 12> centers = {
				{
						{
								0,
								0,
								0,
								1,
						},
						{
								1,
								1,
								1,
								1,
						},
						{
								.6274039f,
								.0690973f,
								.0163914f,
								1,
						},
						{
								.3292830f,
								.9195404f,
								.0880133f,
								1,
						},
						{
								.0433131f,
								.0113623f,
								.8955953f,
								1,
						},
						{
								.0512695f,
								.0512695f,
								.0512695f,
								1,
						},
						{
								.2158605f,
								.2158605f,
								.2158605f,
								1,
						},
						{
								.5271151f,
								.5271151f,
								.5271151f,
								1,
						},
						{
								.126077f,
								.208024f,
								.491921f,
								1,
						},
						{
								0,
								0,
								0,
								0,
						},
						{
								.6274039f,
								.0690973f,
								.0163914f,
								128.f / 255,
						},
						{
								.6274039f,
								.0690973f,
								.0163914f,
								128.f / 255,
						},
				},
		};
		const auto start = std::chrono::steady_clock::now();
		for (uint32_t run = 0; run < runs; run++) {
			const uint32_t order = run % 2, alpha = (run / 2) % 3, count = alpha ? 12 : 9;
			auto           profile = default_visual_color(alpha);
			std::memcpy(color._mapped, &profile, sizeof(profile));
			std::array<uint32_t, 4> meta{
					{
							count,
							width,
							height,
							run + 1,
					},
			};
			std::memcpy(params._mapped, meta.data(), 16);
			for (uint32_t i = 0; i < count; i++) {
				Reference r{};
				r._xybit[0] = i;
				r._xybit[2] = i;
				for (uint32_t channel = 0; channel < 4; channel++) {
					float value = centers[i][channel];
					if (wrong_reference && i == 0 && channel == 0) {
						value = .25f;
					}
					if (i == 11 && alpha == 1 && channel < 3) {
						value *= .2158605f;
					}
					if (i == 10 && alpha == 2 && channel < 3) {
						value *= std::pow((255.f / 128 + .055f) / 1.055f, 2.4f);
					}
					r._low[channel]  = value - .002f;
					r._high[channel] = value + .002f;
				}
				std::memcpy(static_cast<char *>(params._mapped) + 16 + i * sizeof(r), &r, sizeof(r));
			}
			check(vkResetCommandBuffer(cmd, 0), "reset normalization commands");
			VkCommandBufferBeginInfo const bi = {
					.sType            = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
					.pNext            = {},
					.flags            = {},
					.pInheritanceInfo = {},
			};
			check(vkBeginCommandBuffer(cmd, &bi), "begin normalization commands");
			if (!run) {
				VkImageMemoryBarrier const b = {
						.sType               = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
						.pNext               = {},
						.srcAccessMask       = {},
						.dstAccessMask       = VK_ACCESS_SHADER_WRITE_BIT,
						.oldLayout           = VK_IMAGE_LAYOUT_UNDEFINED,
						.newLayout           = VK_IMAGE_LAYOUT_GENERAL,
						.srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
						.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
						.image               = canonical._image,
						.subresourceRange =
								{
										.aspectMask     = VK_IMAGE_ASPECT_COLOR_BIT,
										.baseMipLevel   = 0,
										.levelCount     = 1,
										.baseArrayLayer = 0,
										.layerCount     = 1,
								},
				};
				vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, 0, 0,
									 nullptr, 0, nullptr, 1, &b);
			}
			barrier(cmd, VkFlags{VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT} | VkFlags{VK_PIPELINE_STAGE_HOST_BIT},
					VkFlags{VK_PIPELINE_STAGE_TRANSFER_BIT} | VkFlags{VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT},
					VkFlags{VK_ACCESS_SHADER_READ_BIT} | VkFlags{VK_ACCESS_SHADER_WRITE_BIT} |
							VkFlags{VK_ACCESS_HOST_WRITE_BIT},
					VkFlags{VK_ACCESS_TRANSFER_WRITE_BIT} | VkFlags{VK_ACCESS_SHADER_READ_BIT} |
							VkFlags{VK_ACCESS_SHADER_WRITE_BIT});
			vkCmdFillBuffer(cmd, result._buffer, 0, 16, 0);
			barrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
					VK_ACCESS_TRANSFER_WRITE_BIT,
					VkFlags{VK_ACCESS_SHADER_READ_BIT} | VkFlags{VK_ACCESS_SHADER_WRITE_BIT});
			std::array<uint32_t, 4> source{
					{
							width,
							height,
							order,
							alpha,
					},
			};
			vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, pipeline_layouts[0], 0, 1, &sets[0], 0,
									nullptr);
			vkCmdPushConstants(cmd, pipeline_layouts[0], VK_SHADER_STAGE_COMPUTE_BIT, 0, 16, source.data());
			vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, pipelines[0]);
			vkCmdDispatch(cmd, (width * height + 63) / 64, 1, 1);
			barrier(cmd, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
					VK_ACCESS_SHADER_WRITE_BIT,
					VkFlags{VK_ACCESS_SHADER_READ_BIT} | VkFlags{VK_ACCESS_SHADER_WRITE_BIT});
			std::array<uint32_t, 4> normalization{
					{
							width,
							height,
							order ? 0x34324241u : 0x34325241u,
							0,
					},
			};
			vkCmdPushConstants(cmd, pipeline_layouts[0], VK_SHADER_STAGE_COMPUTE_BIT, 0, 16, normalization.data());
			vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, pipelines[1]);
			vkCmdDispatch(cmd, (width + 15) / 16, (height + 15) / 16, 1);
			barrier(cmd, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
					VK_ACCESS_SHADER_WRITE_BIT,
					VkFlags{VK_ACCESS_SHADER_READ_BIT} | VkFlags{VK_ACCESS_SHADER_WRITE_BIT});
			vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, pipeline_layouts[1], 0, 1, &sets[1], 0,
									nullptr);
			vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, pipelines[2]);
			vkCmdDispatch(cmd, 1, 1, 1);
			barrier(cmd, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_HOST_BIT, VK_ACCESS_SHADER_WRITE_BIT,
					VK_ACCESS_HOST_READ_BIT);
			check(vkEndCommandBuffer(cmd), "end normalization commands");
			check(vkResetFences(c._device, 1, &fence), "reset normalization fence");
			VkSubmitInfo const si = {
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
			check(vkQueueSubmit(c._queue, 1, &si, fence), "submit normalization");
			check(vkWaitForFences(c._device, 1, &fence, VK_TRUE, 5'000'000'000ull), "complete normalization");
			std::array<uint32_t, 4> checked{};
			std::memcpy(checked.data(), result._mapped, 16);
			if (checked[0] != (wrong_reference ? 1u : 0u) || checked[1] != ((1u << count) - 1) ||
				checked[2] != run + 1 || checked[3]) {
				probe_fail("color-reference failure mask=" + std::to_string(checked[0]));
			}
			if (run < 6) {
				std::cout << "{\"rgbaOrder\":" << order << ",\"alphaMode\":" << alpha
						  << ",\"failedMask\":" << checked[0] << ",\"checkedMask\":" << checked[1] << "}\n";
			}
		}
		const double ms = std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - start).count();
		std::cerr << width << "x" << height << " normalization: " << runs << " submissions, " << ms / runs
				  << " ms average; raw/canonical pixels never mapped\n";
		check(vkDeviceWaitIdle(c._device), "normalization idle");
		vkDestroyFence(c._device, fence, nullptr);
		vkDestroyCommandPool(c._device, pool, nullptr);
		vkDestroyDescriptorPool(c._device, descriptor_pool, nullptr);
		for (auto p: pipelines) {
			vkDestroyPipeline(c._device, p, nullptr);
		}
		for (auto p: pipeline_layouts) {
			vkDestroyPipelineLayout(c._device, p, nullptr);
		}
		for (auto l: layouts) {
			vkDestroyDescriptorSetLayout(c._device, l, nullptr);
		}
	}
}
