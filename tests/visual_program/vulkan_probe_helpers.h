#pragma once
#include "../../src/shader_compiler.h"
#include "../../src/visual_gpu_internal.h"
#include "../../src/visual_program_gpu.h"

#include <cstdlib>
#include <iostream>

[[noreturn]] inline void probe_fail(std::string_view message) {
	std::cerr << message << '\n';
	std::exit(EXIT_FAILURE);
}
inline void enable_vulkan_validation() {
	if (setenv("VK_INSTANCE_LAYERS", "VK_LAYER_KHRONOS_validation", 1) != 0 ||
		setenv("VK_LAYER_VALIDATE_SYNC", "1", 1) != 0) {
		probe_fail("cannot enable Vulkan validation for this probe");
	}
}
template<class T>
auto probe_value(visual::Result<T> result) -> T {
	if (!result) {
		probe_fail(result.error());
	}
	if constexpr (!std::is_void_v<T>) {
		return std::move(*result);
	}
}
inline void check(VkResult result, std::string_view operation) { probe_value(vk_status(result, operation)); }
inline auto compile_for_probe(std::string_view source) -> std::vector<uint32_t> {
	return probe_value(compile_visual_glsl(source));
}
struct Texture : visual::Non_Copyable {
	Context       &_c;
	VkImage        _image{};
	VkDeviceMemory _memory{};
	VkImageView    _view{};
	explicit Texture(Context &ctx, uint32_t width, uint32_t height) : _c(ctx) {
		VkImageCreateInfo const i = {
				.sType     = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
				.pNext     = {},
				.flags     = {},
				.imageType = VK_IMAGE_TYPE_2D,
				.format    = VK_FORMAT_R16G16B16A16_SFLOAT,
				.extent =
						{
								.width  = width,
								.height = height,
								.depth  = 1,
						},
				.mipLevels             = 1,
				.arrayLayers           = 1,
				.samples               = VK_SAMPLE_COUNT_1_BIT,
				.tiling                = VK_IMAGE_TILING_OPTIMAL,
				.usage                 = VK_IMAGE_USAGE_STORAGE_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT,
				.sharingMode           = {},
				.queueFamilyIndexCount = {},
				.pQueueFamilyIndices   = {},
				.initialLayout         = {},
		};
		check(vkCreateImage(_c._device, &i, nullptr, &_image), "create fp16 texture");
		VkMemoryRequirements r;
		vkGetImageMemoryRequirements(_c._device, _image, &r);
		VkMemoryAllocateInfo const a = {
				.sType           = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
				.pNext           = {},
				.allocationSize  = r.size,
				.memoryTypeIndex = probe_value(_c.memory_type(r.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)),
		};
		check(vkAllocateMemory(_c._device, &a, nullptr, &_memory), "allocate unmapped texture");
		check(vkBindImageMemory(_c._device, _image, _memory, 0), "bind texture");
		VkImageViewCreateInfo const v = {
				.sType      = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
				.pNext      = {},
				.flags      = {},
				.image      = _image,
				.viewType   = VK_IMAGE_VIEW_TYPE_2D,
				.format     = i.format,
				.components = {},
				.subresourceRange =
						{
								.aspectMask     = VK_IMAGE_ASPECT_COLOR_BIT,
								.baseMipLevel   = 0,
								.levelCount     = 1,
								.baseArrayLayer = 0,
								.layerCount     = 1,
						},
		};
		check(vkCreateImageView(_c._device, &v, nullptr, &_view), "view texture");
	}
	~Texture() {
		vkDestroyImageView(_c._device, _view, nullptr);
		vkDestroyImage(_c._device, _image, nullptr);
		vkFreeMemory(_c._device, _memory, nullptr);
	}
};

inline void barrier(VkCommandBuffer cmd, VkPipelineStageFlags from, VkPipelineStageFlags to, VkAccessFlags src,
					VkAccessFlags dst) {
	VkMemoryBarrier const b = {
			.sType         = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
			.pNext         = {},
			.srcAccessMask = src,
			.dstAccessMask = dst,
	};
	vkCmdPipelineBarrier(cmd, from, to, 0, 1, &b, 0, nullptr, 0, nullptr);
}

inline VkPipeline compute_pipeline(Context const &c, VkPipelineLayout layout, std::string_view source) {
	auto                           words = compile_for_probe(source);
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
			.layout             = layout,
			.basePipelineHandle = {},
			.basePipelineIndex  = {},
	};
	VkPipeline     pipeline{};
	VkResult const r = vkCreateComputePipelines(c._device, VK_NULL_HANDLE, 1, &pc, nullptr, &pipeline);
	vkDestroyShaderModule(c._device, module, nullptr);
	check(r, "compute pipeline");
	return pipeline;
}
