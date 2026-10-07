#pragma once
// Vulkan observation and explicit in-memory screenshots. Observer readback is
// bounded.
#include "visual_gpu.h"
#include "visual_result.h"
#include "visual_spirv.h"
#include <algorithm>
#include <array>
#include <cstdint>
#include <cstring>
#include <expected>
#include <fcntl.h>
#include <linux/dma-buf.h>
#include <memory>
#include <span>
#include <string>
#include <string_view>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>
#include <utility>
#include <vector>
#include <vulkan/vulkan_core.h>

inline Visual_Color_Profile default_visual_color(uint32_t alpha = 0) {
	Visual_Color_Profile p{
			._transfer   = 14,
			._alpha_mode = alpha,
			._power      = 0,
			._minimum    = 0,
			._maximum    = 1,
			._reference  = 1,
			._reserved =
					{
							0,
							0,
					},
			._matrix =
					{
							{
									.6274039f,
									.32928303f,
									.043313067f,
									0,
							},
							{
									.06909729f,
									.9195404f,
									.011362316f,
									0,
							},
							{
									.01639144f,
									.08801331f,
									.89559525f,
									0,
							},
					},
			._weights =
					{
							.212639006f,
							.715168679f,
							.072192315f,
							0,
					},
	};
	return p;
}
[[nodiscard]] inline visual::Status vk_status(VkResult r, std::string_view op) {
	if (r != VK_SUCCESS) {
		return std::unexpected(std::string(op) + ": VkResult " + std::to_string(r));
	}
	return {};
}
inline void error_copy(char *error, std::string_view message) {
	if (!error) {
		return;
	}
	const auto length = std::min(message.size(), std::size_t{511});
	std::ranges::copy(message.substr(0, length), error);
	error[length] = 0;
}
inline int ffi_status(visual::Status status, char *error) {
	if (!status) {
		error_copy(error, status.error());
		return -1;
	}
	return 0;
}
inline int ffi_result(visual::Result<int> result, char *error) {
	if (!result) {
		error_copy(error, result.error());
		return -1;
	}
	return *result;
}
struct Unique_Fd : visual::Non_Copyable {
	int _value = -1;
	explicit Unique_Fd(int fd) : _value(fd) {}
	~Unique_Fd() {
		if (_value >= 0) {
			close(_value);
		}
	}
	int release() { return std::exchange(_value, -1); }
};
inline bool same_color(const Visual_Color_Profile &a, const Visual_Color_Profile &b) {
	if (a._transfer != b._transfer || a._alpha_mode != b._alpha_mode || a._power != b._power ||
		a._minimum != b._minimum || a._maximum != b._maximum || a._reference != b._reference) {
		return false;
	}
	for (std::size_t row = 0; row < 3; row++) {
		if (!std::ranges::equal(a._matrix[row], b._matrix[row])) {
			return false;
		}
	}
	return std::ranges::equal(a._weights, b._weights);
}
struct Context : visual::Non_Copyable {
	VkInstance                       _instance{};
	VkPhysicalDevice                 _physical{};
	VkDevice                         _device{};
	VkQueue                          _queue{};
	uint32_t                         _family{};
	VkPhysicalDeviceMemoryProperties _memory{};
	VkDescriptorSetLayout            _layout{};
	VkPipelineLayout                 _pipeline_layout{};
	VkPipeline                       _pipeline{};
	~Context() {
		if (_device) {
			vkDeviceWaitIdle(_device);
			vkDestroyPipeline(_device, _pipeline, nullptr);
			vkDestroyPipelineLayout(_device, _pipeline_layout, nullptr);
			vkDestroyDescriptorSetLayout(_device, _layout, nullptr);
			vkDestroyDevice(_device, nullptr);
		}
		if (_instance) {
			vkDestroyInstance(_instance, nullptr);
		}
	}
	[[nodiscard]] visual::Result<uint32_t> memory_type(uint32_t bits, VkMemoryPropertyFlags flags) {
		for (uint32_t i = 0; i < _memory.memoryTypeCount; i++) {
			if ((bits & (1u << i)) && (_memory.memoryTypes[i].propertyFlags & flags) == flags) {
				return i;
			}
		}
		return std::unexpected("no compatible Vulkan memory type");
	}
	[[nodiscard]] visual::Status init(uint32_t major, uint32_t minor) {
		VkApplicationInfo const app = {
				.sType              = VK_STRUCTURE_TYPE_APPLICATION_INFO,
				.pNext              = {},
				.pApplicationName   = "wayland-mcp-events",
				.applicationVersion = {},
				.pEngineName        = {},
				.engineVersion      = {},
				.apiVersion         = VK_API_VERSION_1_1,
		};
		VkInstanceCreateInfo const ci = {
				.sType                   = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
				.pNext                   = {},
				.flags                   = {},
				.pApplicationInfo        = &app,
				.enabledLayerCount       = {},
				.ppEnabledLayerNames     = {},
				.enabledExtensionCount   = {},
				.ppEnabledExtensionNames = {},
		};
		VISUAL_TRY(vk_status(vkCreateInstance(&ci, nullptr, &_instance), "vkCreateInstance"));
		uint32_t count = 0;
		VISUAL_TRY(vk_status(vkEnumeratePhysicalDevices(_instance, &count, nullptr), "enumerate devices"));
		std::vector<VkPhysicalDevice> devices(count);
		VISUAL_TRY(vk_status(vkEnumeratePhysicalDevices(_instance, &count, devices.data()), "enumerate devices"));
		// Never try imports on arbitrary GPUs. Match the compositor's DRM affinity
		// first.
		for (auto candidate: devices) {
			VkPhysicalDeviceDrmPropertiesEXT drm = {
					.sType        = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRM_PROPERTIES_EXT,
					.pNext        = {},
					.hasPrimary   = {},
					.hasRender    = {},
					.primaryMajor = {},
					.primaryMinor = {},
					.renderMajor  = {},
					.renderMinor  = {},
			};
			VkPhysicalDeviceProperties2 props = {
					.sType      = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2,
					.pNext      = &drm,
					.properties = {},
			};
			vkGetPhysicalDeviceProperties2(candidate, &props);
			if ((drm.hasRender && std::cmp_equal(drm.renderMajor, major) && std::cmp_equal(drm.renderMinor, minor)) ||
				(drm.hasPrimary && std::cmp_equal(drm.primaryMajor, major) &&
				 std::cmp_equal(drm.primaryMinor, minor))) {
				_physical = candidate;
				break;
			}
		}
		if (!_physical) {
			return std::unexpected("no Vulkan physical device matching DRM affinity");
		}
		vkGetPhysicalDeviceMemoryProperties(_physical, &_memory);
		uint32_t qn = 0;
		vkGetPhysicalDeviceQueueFamilyProperties(_physical, &qn, nullptr);
		std::vector<VkQueueFamilyProperties> qs(qn);
		vkGetPhysicalDeviceQueueFamilyProperties(_physical, &qn, qs.data());
		bool found = false;
		for (uint32_t i = 0; i < qn; i++) {
			if (qs[i].queueFlags & VK_QUEUE_COMPUTE_BIT) {
				_family = i;
				found   = true;
				break;
			}
		}
		if (!found) {
			return std::unexpected("DRM device has no compute queue");
		}
		float const                   priority = 1;
		VkDeviceQueueCreateInfo const qi       = {
				.sType            = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
				.pNext            = {},
				.flags            = {},
				.queueFamilyIndex = _family,
				.queueCount       = 1,
				.pQueuePriorities = &priority,
		};
		std::array<const char *, 7> exts{
				{
						"VK_KHR_external_memory_fd",
						"VK_EXT_external_memory_dma_buf",
						"VK_EXT_image_drm_format_modifier",
						"VK_EXT_physical_device_drm",
						"VK_EXT_queue_family_foreign",
						"VK_KHR_external_semaphore_fd",
						"VK_KHR_image_format_list",
				},
		};
		VkPhysicalDeviceFeatures supported{};
		vkGetPhysicalDeviceFeatures(_physical, &supported);
		VkPhysicalDeviceFeatures const enabled = {
				.robustBufferAccess                      = supported.robustBufferAccess,
				.fullDrawIndexUint32                     = {},
				.imageCubeArray                          = {},
				.independentBlend                        = {},
				.geometryShader                          = {},
				.tessellationShader                      = {},
				.sampleRateShading                       = {},
				.dualSrcBlend                            = {},
				.logicOp                                 = {},
				.multiDrawIndirect                       = {},
				.drawIndirectFirstInstance               = {},
				.depthClamp                              = {},
				.depthBiasClamp                          = {},
				.fillModeNonSolid                        = {},
				.depthBounds                             = {},
				.wideLines                               = {},
				.largePoints                             = {},
				.alphaToOne                              = {},
				.multiViewport                           = {},
				.samplerAnisotropy                       = {},
				.textureCompressionETC2                  = {},
				.textureCompressionASTC_LDR              = {},
				.textureCompressionBC                    = {},
				.occlusionQueryPrecise                   = {},
				.pipelineStatisticsQuery                 = {},
				.vertexPipelineStoresAndAtomics          = {},
				.fragmentStoresAndAtomics                = {},
				.shaderTessellationAndGeometryPointSize  = {},
				.shaderImageGatherExtended               = {},
				.shaderStorageImageExtendedFormats       = {},
				.shaderStorageImageMultisample           = {},
				.shaderStorageImageReadWithoutFormat     = {},
				.shaderStorageImageWriteWithoutFormat    = {},
				.shaderUniformBufferArrayDynamicIndexing = {},
				.shaderSampledImageArrayDynamicIndexing  = {},
				.shaderStorageBufferArrayDynamicIndexing = {},
				.shaderStorageImageArrayDynamicIndexing  = {},
				.shaderClipDistance                      = {},
				.shaderCullDistance                      = {},
				.shaderFloat64                           = {},
				.shaderInt64                             = {},
				.shaderInt16                             = {},
				.shaderResourceResidency                 = {},
				.shaderResourceMinLod                    = {},
				.sparseBinding                           = {},
				.sparseResidencyBuffer                   = {},
				.sparseResidencyImage2D                  = {},
				.sparseResidencyImage3D                  = {},
				.sparseResidency2Samples                 = {},
				.sparseResidency4Samples                 = {},
				.sparseResidency8Samples                 = {},
				.sparseResidency16Samples                = {},
				.sparseResidencyAliased                  = {},
				.variableMultisampleRate                 = {},
				.inheritedQueries                        = {},
		};

		VkDeviceCreateInfo const di = {
				.sType                   = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
				.pNext                   = {},
				.flags                   = {},
				.queueCreateInfoCount    = 1,
				.pQueueCreateInfos       = &qi,
				.enabledLayerCount       = {},
				.ppEnabledLayerNames     = {},
				.enabledExtensionCount   = 7,
				.ppEnabledExtensionNames = exts.data(),
				.pEnabledFeatures        = &enabled,
		};
		VISUAL_TRY(vk_status(vkCreateDevice(_physical, &di, nullptr, &_device), "vkCreateDevice"));
		vkGetDeviceQueue(_device, _family, 0, &_queue);
		std::array<VkDescriptorSetLayoutBinding, 6> bindings{};
		for (uint32_t i = 0; i < 6; i++) {
			bindings[i] = {
					.binding            = i,
					.descriptorType     = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
					.descriptorCount    = 1,
					.stageFlags         = VK_SHADER_STAGE_COMPUTE_BIT,
					.pImmutableSamplers = nullptr,
			};
		}
		VkDescriptorSetLayoutCreateInfo const li = {
				.sType        = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
				.pNext        = {},
				.flags        = {},
				.bindingCount = 6,
				.pBindings    = bindings.data(),
		};
		VISUAL_TRY(vk_status(vkCreateDescriptorSetLayout(_device, &li, nullptr, &_layout), "create descriptors"));
		VkPushConstantRange const push{
				.stageFlags = VK_SHADER_STAGE_COMPUTE_BIT,
				.offset     = 0,
				.size       = 4,
		};
		VkPipelineLayoutCreateInfo const pi = {
				.sType                  = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
				.pNext                  = {},
				.flags                  = {},
				.setLayoutCount         = 1,
				.pSetLayouts            = &_layout,
				.pushConstantRangeCount = 1,
				.pPushConstantRanges    = &push,
		};
		VISUAL_TRY(
				vk_status(vkCreatePipelineLayout(_device, &pi, nullptr, &_pipeline_layout), "create pipeline layout"));
		VkShaderModuleCreateInfo const si = {
				.sType    = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
				.pNext    = {},
				.flags    = {},
				.codeSize = sizeof(visual_spirv),
				.pCode    = visual_spirv.data(),
		};
		VkShaderModule shader{};
		VISUAL_TRY(vk_status(vkCreateShaderModule(_device, &si, nullptr, &shader), "create shader"));
		VkComputePipelineCreateInfo const pci = {
				.sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
				.pNext = {},
				.flags = {},
				.stage =
						{
								.sType               = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
								.pNext               = {},
								.flags               = {},
								.stage               = VK_SHADER_STAGE_COMPUTE_BIT,
								.module              = shader,
								.pName               = "main",
								.pSpecializationInfo = {},
						},
				.layout             = _pipeline_layout,
				.basePipelineHandle = {},
				.basePipelineIndex  = {},
		};
		VkResult const result = vkCreateComputePipelines(_device, VK_NULL_HANDLE, 1, &pci, nullptr, &_pipeline);
		vkDestroyShaderModule(_device, shader, nullptr);
		VISUAL_TRY(vk_status(result, "create compute pipeline"));
		return {};
	}
};
struct Buffer : visual::Non_Copyable {
	Context       *_c{};
	VkBuffer       _buffer{};
	VkDeviceMemory _memory{};
	VkDeviceSize   _size{};
	void          *_mapped{};
	~Buffer() {
		if (!_c) {
			return;
		}
		if (_mapped) {
			vkUnmapMemory(_c->_device, _memory);
		}
		vkDestroyBuffer(_c->_device, _buffer, nullptr);
		vkFreeMemory(_c->_device, _memory, nullptr);
	}
	[[nodiscard]] visual::Status init(Context *ctx, VkDeviceSize bytes, VkBufferUsageFlags usage, bool host = false) {
		_c                          = ctx;
		_size                       = bytes;
		VkBufferCreateInfo const bi = {
				.sType                 = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
				.pNext                 = {},
				.flags                 = {},
				.size                  = bytes,
				.usage                 = usage,
				.sharingMode           = {},
				.queueFamilyIndexCount = {},
				.pQueueFamilyIndices   = {},
		};
		VISUAL_TRY(vk_status(vkCreateBuffer(_c->_device, &bi, nullptr, &_buffer), "create buffer"));
		VkMemoryRequirements req{};
		vkGetBufferMemoryRequirements(_c->_device, _buffer, &req);
		auto memory_type_result =
				_c->memory_type(req.memoryTypeBits, host ? VkFlags{VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT} |
																	VkFlags{VK_MEMORY_PROPERTY_HOST_COHERENT_BIT}
														 : VkFlags{VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT});
		if (!memory_type_result) {
			return std::unexpected(std::move(memory_type_result.error()));
		}
		auto const                 memory_type = *memory_type_result;
		VkMemoryAllocateInfo const ai          = {
				.sType           = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
				.pNext           = {},
				.allocationSize  = req.size,
				.memoryTypeIndex = memory_type,
		};
		VISUAL_TRY(vk_status(vkAllocateMemory(_c->_device, &ai, nullptr, &_memory), "allocate buffer"));
		VISUAL_TRY(vk_status(vkBindBufferMemory(_c->_device, _buffer, _memory, 0), "bind buffer"));
		// Observer pixel/history/state buffers are never mapped, even on UMA.
		if (host) {
			VISUAL_TRY(vk_status(vkMapMemory(_c->_device, _memory, 0, bytes, 0, &_mapped),
								 "map host output/parameter buffer"));
		}
		return {};
	}
};
struct Imported : visual::Non_Copyable {
	Context                  *_c;
	VkImage                   _image{};
	VkDeviceMemory            _memory{};
	uint64_t                  _key{};
	uint32_t                  _width{}, _height{}, _format{};
	int                       _sync_fd = -1;
	dev_t                     _allocation_device{};
	std::vector<Visual_Plane> _plane_layout;
	explicit Imported(Context *c) : _c(c) {}
	~Imported() {
		if (_sync_fd >= 0) {
			close(_sync_fd);
		}
		vkDestroyImage(_c->_device, _image, nullptr);
		vkFreeMemory(_c->_device, _memory, nullptr);
	}
	[[nodiscard]] [[nodiscard]] bool matches(uint64_t k, uint32_t w, uint32_t h, uint32_t fmt,
											 std::span<const Visual_Plane> p) const {
		const auto n = p.size();
		if (_key != k || _width != w || _height != h || _format != fmt || _plane_layout.size() != n || !n) {
			return false;
		}
		struct stat allocation{};
		if (fstat(p[0]._fd, &allocation) < 0 || allocation.st_dev != _allocation_device ||
			static_cast<uint64_t>(allocation.st_ino) != _key) {
			return false;
		}
		for (uint32_t i = 0; i < n; i++) {
			struct stat plane{};
			if (fstat(p[i]._fd, &plane) < 0 || plane.st_dev != _allocation_device ||
				static_cast<uint64_t>(plane.st_ino) != _key || p[i]._offset != _plane_layout[i]._offset ||
				p[i]._stride != _plane_layout[i]._stride || p[i]._modifier != _plane_layout[i]._modifier) {
				return false;
			}
		}
		return true;
	}
	[[nodiscard]] visual::Status init(uint64_t k, uint32_t w, uint32_t h, uint32_t fmt,
									  std::span<const Visual_Plane> planes, bool /*screenshot*/ = false) {
		const auto count = planes.size();
		_key             = k;
		_width           = w;
		_height          = h;
		_format          = fmt;
		if (count == 0 || count > 4) {
			return std::unexpected("invalid DMA-BUF plane count");
		}
		struct stat first{};
		if (fstat(planes[0]._fd, &first) < 0) {
			return std::unexpected("DMA-BUF identity failed");
		}
		for (uint32_t i = 1; i < count; i++) {
			struct stat plane{};
			if (fstat(planes[i]._fd, &plane) < 0 || plane.st_ino != first.st_ino || plane.st_dev != first.st_dev ||
				planes[i]._modifier != planes[0]._modifier) {
				return std::unexpected(
						"visual imports require one DMA-BUF allocation "
						"with consistent modifier planes");
			}
		}
		_allocation_device = first.st_dev;
		_plane_layout.assign_range(planes);
		_sync_fd = fcntl(planes[0]._fd, F_DUPFD_CLOEXEC, 3);
		if (_sync_fd < 0) {
			return std::unexpected("dup DMA-BUF sync fd failed");
		}
		VkFormat vf;
		if (fmt == 0x34325258u || fmt == 0x34325241u) {
			vf = VK_FORMAT_B8G8R8A8_UNORM;
		} else if (fmt == 0x34324258u || fmt == 0x34324241u) {
			vf = VK_FORMAT_R8G8B8A8_UNORM;
		} else if ((fmt == 0x30335258u || fmt == 0x30335241u)) {
			vf = VK_FORMAT_A2R10G10B10_UNORM_PACK32;
		} else if ((fmt == 0x30334258u || fmt == 0x30334241u)) {
			vf = VK_FORMAT_A2B10G10R10_UNORM_PACK32;
		} else if ((fmt == 0x48344258u || fmt == 0x48344241u)) {
			vf = VK_FORMAT_R16G16B16A16_SFLOAT;
		} else {
			return std::unexpected("unsupported RGB DMA-BUF format");
		}
		std::vector<VkSubresourceLayout> layouts;
		layouts.reserve(count);
		layouts.reserve(count);
		for (uint32_t i = 0; i < count; i++) {
			layouts.push_back({
					.offset     = planes[i]._offset,
					.size       = 0,
					.rowPitch   = planes[i]._stride,
					.arrayPitch = 0,
					.depthPitch = 0,
			});
		}
		VkImageDrmFormatModifierExplicitCreateInfoEXT mi = {
				.sType                       = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT,
				.pNext                       = {},
				.drmFormatModifier           = planes[0]._modifier,
				.drmFormatModifierPlaneCount = static_cast<uint32_t>(count),
				.pPlaneLayouts               = layouts.data(),
		};
		VkExternalMemoryImageCreateInfo ei = {
				.sType       = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
				.pNext       = &mi,
				.handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
		};
		VkImageCreateInfo const ii = {
				.sType     = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
				.pNext     = &ei,
				.flags     = {},
				.imageType = VK_IMAGE_TYPE_2D,
				.format    = vf,
				.extent =
						{
								.width  = w,
								.height = h,
								.depth  = 1,
						},
				.mipLevels             = 1,
				.arrayLayers           = 1,
				.samples               = VK_SAMPLE_COUNT_1_BIT,
				.tiling                = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
				.usage                 = VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
				.sharingMode           = {},
				.queueFamilyIndexCount = {},
				.pQueueFamilyIndices   = {},
				.initialLayout         = {},
		};
		VISUAL_TRY(vk_status(vkCreateImage(_c->_device, &ii, nullptr, &_image), "import image on affinity device"));
		VkMemoryRequirements req{};
		vkGetImageMemoryRequirements(_c->_device, _image, &req);
		Unique_Fd owned_fd(fcntl(planes[0]._fd, F_DUPFD_CLOEXEC, 3));
		int const fd = owned_fd._value;
		if (fd < 0) {
			return std::unexpected("dup DMA-BUF failed");
		}
		VkImportMemoryFdInfoKHR fi = {
				.sType      = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
				.pNext      = {},
				.handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
				.fd         = fd,
		};
		VkMemoryDedicatedAllocateInfo dedicated = {
				.sType  = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
				.pNext  = &fi,
				.image  = _image,
				.buffer = {},
		};
		auto get_props = reinterpret_cast<PFN_vkGetMemoryFdPropertiesKHR>(
				vkGetDeviceProcAddr(_c->_device, "vkGetMemoryFdPropertiesKHR"));
		if (!get_props) {
			return std::unexpected("DMA-BUF memory properties entry point unavailable");
		}
		VkMemoryFdPropertiesKHR fd_props = {
				.sType          = VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR,
				.pNext          = {},
				.memoryTypeBits = {},
		};
		VkResult const fd_result =
				get_props(_c->_device, VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT, fd, &fd_props);
		VISUAL_TRY(vk_status(fd_result, "DMA-BUF memory properties"));
		auto memory_type_result = _c->memory_type(req.memoryTypeBits & fd_props.memoryTypeBits, 0);
		if (!memory_type_result) {
			return std::unexpected(std::move(memory_type_result.error()));
		}
		auto const                 memory_type = *memory_type_result;
		VkMemoryAllocateInfo const ai          = {
				.sType           = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
				.pNext           = &dedicated,
				.allocationSize  = req.size,
				.memoryTypeIndex = memory_type,
		};
		VkResult const alloc = vkAllocateMemory(_c->_device, &ai, nullptr, &_memory);
		VISUAL_TRY(vk_status(alloc, "import DMA-BUF memory"));
		owned_fd.release(); // Vulkan consumes the fd only after successful import.
		VISUAL_TRY(vk_status(vkBindImageMemory(_c->_device, _image, _memory, 0), "bind imported image"));
		return {};
	}
};
struct Program : visual::Non_Copyable {
	Context                               *_c;
	uint32_t                               _count{};
	std::vector<Visual_Rect>               _rects;
	std::vector<Visual_Rule>               _rules;
	Buffer                                 _pixels, _history, _params, _state, _events, _source_color;
	Visual_Color_Profile                   _profile = default_visual_color();
	VkCommandPool                          _pool{};
	VkCommandBuffer                        _cmd{};
	VkFence                                _fence{};
	VkSemaphore                            _acquire{};
	VkDescriptorPool                       _descriptors{};
	VkDescriptorSet                        _set{};
	std::vector<std::unique_ptr<Imported>> _imports;
	bool                                   _initialized = false, _captured = false;
	uint32_t                               _source_format = 0x34325241u, _pixel_stride = 8;
	explicit Program(Context *c) : _c(c) {}
	~Program() {
		vkDestroySemaphore(_c->_device, _acquire, nullptr);
		vkDestroyFence(_c->_device, _fence, nullptr);
		vkDestroyCommandPool(_c->_device, _pool, nullptr);
		vkDestroyDescriptorPool(_c->_device, _descriptors, nullptr);
	}
	[[nodiscard]] visual::Status init_raw(uint32_t width, uint32_t height, uint32_t bytes_per_pixel = 4) {
		_pixel_stride = bytes_per_pixel;
		VISUAL_TRY(_source_color.init(_c, sizeof(_profile), VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, true));
		std::memcpy(_source_color._mapped, &_profile, sizeof(_profile));
		_count = 1;
		_rects.push_back({
				._x      = 0,
				._y      = 0,
				._width  = width,
				._height = height,
		});
		Visual_Rule const rule = {
				._offset    = {},
				._width     = width,
				._height    = height,
				._kind      = {},
				._threshold = {},
				._minimum   = {},
				._above     = {},
				._debounce  = {},
				._cooldown  = {},
		};
		_rules.push_back(rule);
		VISUAL_TRY(_pixels.init(_c, uint64_t(width) * height * bytes_per_pixel,
								VkFlags{VK_BUFFER_USAGE_TRANSFER_DST_BIT} | VkFlags{VK_BUFFER_USAGE_TRANSFER_SRC_BIT} |
										VkFlags{VK_BUFFER_USAGE_STORAGE_BUFFER_BIT}));
		VISUAL_TRY(init_commands());
		return {};
	}
	[[nodiscard]] visual::Status init_commands() {
		VkCommandPoolCreateInfo const ci = {
				.sType            = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
				.pNext            = {},
				.flags            = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
				.queueFamilyIndex = _c->_family,
		};
		VISUAL_TRY(vk_status(vkCreateCommandPool(_c->_device, &ci, nullptr, &_pool), "create command pool"));
		VkCommandBufferAllocateInfo const ai = {
				.sType              = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
				.pNext              = {},
				.commandPool        = _pool,
				.level              = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
				.commandBufferCount = 1,
		};
		VISUAL_TRY(vk_status(vkAllocateCommandBuffers(_c->_device, &ai, &_cmd), "allocate command buffer"));
		VkFenceCreateInfo const fi = {
				.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO,
				.pNext = {},
				.flags = {},
		};
		VISUAL_TRY(vk_status(vkCreateFence(_c->_device, &fi, nullptr, &_fence), "create fence"));
		VkSemaphoreCreateInfo const sci = {
				.sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO,
				.pNext = {},
				.flags = {},
		};
		VISUAL_TRY(vk_status(vkCreateSemaphore(_c->_device, &sci, nullptr, &_acquire), "create acquire semaphore"));
		return {};
	}
	[[nodiscard]] visual::Status init(std::span<const Visual_Rule> r, std::span<const Visual_Rect> rect) {
		if (r.empty() || r.size() > 16 || rect.size() != r.size()) {
			return std::unexpected("invalid visual rule count");
		}
		const auto n = static_cast<uint32_t>(r.size());
		_count       = n;
		_rects.assign_range(rect);
		_rules.assign_range(r);
		uint64_t bytes = 0;
		for (auto &rule: _rules) {
			rule._offset = bytes / 8;
			bytes += static_cast<uint64_t>(rule._width) * rule._height * 8;
		}
		VISUAL_TRY(_pixels.init(
				_c, bytes, VkFlags{VK_BUFFER_USAGE_TRANSFER_DST_BIT} | VkFlags{VK_BUFFER_USAGE_STORAGE_BUFFER_BIT}));
		VISUAL_TRY(
				_history.init(_c, bytes * 2,
							  VkFlags{VK_BUFFER_USAGE_TRANSFER_DST_BIT} | VkFlags{VK_BUFFER_USAGE_STORAGE_BUFFER_BIT}));
		VISUAL_TRY(_source_color.init(_c, sizeof(_profile), VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, true));
		std::memcpy(_source_color._mapped, &_profile, sizeof(_profile));
		VISUAL_TRY(_params.init(_c, n * sizeof(Visual_Rule), VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, true));
		std::memcpy(_params._mapped, _rules.data(), n * sizeof(Visual_Rule));
		VISUAL_TRY(
				_state.init(_c, VkDeviceSize{n} * 24,
							VkFlags{VK_BUFFER_USAGE_TRANSFER_DST_BIT} | VkFlags{VK_BUFFER_USAGE_STORAGE_BUFFER_BIT}));
		VISUAL_TRY(_events.init(_c, 16 + 16 * sizeof(Visual_Event),
								VkFlags{VK_BUFFER_USAGE_TRANSFER_DST_BIT} | VkFlags{VK_BUFFER_USAGE_STORAGE_BUFFER_BIT},
								true));
		VISUAL_TRY(init_commands());
		VkDescriptorPoolSize const ps{
				.type            = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
				.descriptorCount = 6,
		};
		VkDescriptorPoolCreateInfo const dpi = {
				.sType         = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
				.pNext         = {},
				.flags         = {},
				.maxSets       = 1,
				.poolSizeCount = 1,
				.pPoolSizes    = &ps,
		};
		VISUAL_TRY(
				vk_status(vkCreateDescriptorPool(_c->_device, &dpi, nullptr, &_descriptors), "create descriptor pool"));
		VkDescriptorSetAllocateInfo const dsi = {
				.sType              = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
				.pNext              = {},
				.descriptorPool     = _descriptors,
				.descriptorSetCount = 1,
				.pSetLayouts        = &_c->_layout,
		};
		VISUAL_TRY(vk_status(vkAllocateDescriptorSets(_c->_device, &dsi, &_set), "allocate descriptor set"));
		std::array<Buffer *, 6> buffers{
				{
						&_pixels,
						&_history,
						&_params,
						&_state,
						&_events,
						&_source_color,
				},
		};
		std::array<VkDescriptorBufferInfo, 6> infos{};
		std::array<VkWriteDescriptorSet, 6>   writes{};
		for (uint32_t i = 0; i < 6; i++) {
			infos[i] = {
					.buffer = buffers[i]->_buffer,
					.offset = 0,
					.range  = buffers[i]->_size,
			};
			writes[i] = {
					.sType            = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
					.pNext            = {},
					.dstSet           = _set,
					.dstBinding       = i,
					.dstArrayElement  = {},
					.descriptorCount  = 1,
					.descriptorType   = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
					.pImageInfo       = {},
					.pBufferInfo      = &infos[i],
					.pTexelBufferView = {},
			};
		}
		vkUpdateDescriptorSets(_c->_device, 6, writes.data(), 0, nullptr);
		return {};
	}
	void barrier(VkPipelineStageFlags src, VkPipelineStageFlags dst, VkAccessFlags sa, VkAccessFlags da) {
		VkMemoryBarrier const b = {
				.sType         = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
				.pNext         = {},
				.srcAccessMask = sa,
				.dstAccessMask = da,
		};
		vkCmdPipelineBarrier(_cmd, src, dst, 0, 1, &b, 0, nullptr, 0, nullptr);
	}
	[[nodiscard]] visual::Status begin_commands() {
		VISUAL_TRY(vk_status(vkResetCommandBuffer(_cmd, 0), "reset commands"));
		VISUAL_TRY(vk_status(vkResetFences(_c->_device, 1, &_fence), "reset fence"));
		VkCommandBufferBeginInfo const begin = {
				.sType            = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
				.pNext            = {},
				.flags            = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
				.pInheritanceInfo = {},
		};
		VISUAL_TRY(vk_status(vkBeginCommandBuffer(_cmd, &begin), "begin commands"));
		return {};
	}
	[[nodiscard]] visual::Status submit_wait(bool producer) {
		VISUAL_TRY(vk_status(vkEndCommandBuffer(_cmd), "end commands"));
		VkPipelineStageFlags wait_stage = VK_PIPELINE_STAGE_TRANSFER_BIT;
		VkSubmitInfo const   submit     = {
				.sType                = VK_STRUCTURE_TYPE_SUBMIT_INFO,
				.pNext                = {},
				.waitSemaphoreCount   = producer ? 1u : 0u,
				.pWaitSemaphores      = producer ? &_acquire : nullptr,
				.pWaitDstStageMask    = producer ? &wait_stage : nullptr,
				.commandBufferCount   = 1,
				.pCommandBuffers      = &_cmd,
				.signalSemaphoreCount = {},
				.pSignalSemaphores    = {},
		};
		VISUAL_TRY(vk_status(vkQueueSubmit(_c->_queue, 1, &submit, _fence), "submit visual work"));
		VkResult const completed = vkWaitForFences(_c->_device, 1, &_fence, VK_TRUE, 5'000'000'000ull);
		if (completed != VK_SUCCESS) {
			vkDeviceWaitIdle(_c->_device);
			VISUAL_TRY(vk_status(completed, "visual fence"));
		}
		return {};
	}
	[[nodiscard]] visual::Status capture(Imported *image, uint32_t pattern) {
		if (_captured) {
			return std::unexpected("visual acquisition is already pending analysis");
		}
		if (image) {
			dma_buf_export_sync_file sync{
					.flags = DMA_BUF_SYNC_READ,
					.fd    = {},
			};
			if (ioctl(image->_sync_fd, DMA_BUF_IOCTL_EXPORT_SYNC_FILE, &sync) < 0) {
				return std::unexpected("cannot export DMA-BUF producer fence");
			}
			VkImportSemaphoreFdInfoKHR const info = {
					.sType      = VK_STRUCTURE_TYPE_IMPORT_SEMAPHORE_FD_INFO_KHR,
					.pNext      = {},
					.semaphore  = _acquire,
					.flags      = VK_SEMAPHORE_IMPORT_TEMPORARY_BIT,
					.handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT,
					.fd         = sync.fd,
			};
			auto import = reinterpret_cast<PFN_vkImportSemaphoreFdKHR>(
					vkGetDeviceProcAddr(_c->_device, "vkImportSemaphoreFdKHR"));
			Unique_Fd owned_sync(sync.fd);
			if (!import) {
				return std::unexpected("producer fence import entry point unavailable");
			}
			VISUAL_TRY(vk_status(import(_c->_device, &info), "import producer fence"));
			owned_sync.release();
		}
		VISUAL_TRY(begin_commands());
		barrier(VkFlags{VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT} | VkFlags{VK_PIPELINE_STAGE_TRANSFER_BIT},
				VK_PIPELINE_STAGE_TRANSFER_BIT,
				VkFlags{VK_ACCESS_SHADER_READ_BIT} | VkFlags{VK_ACCESS_SHADER_WRITE_BIT} |
						VkFlags{VK_ACCESS_TRANSFER_READ_BIT} | VkFlags{VK_ACCESS_TRANSFER_WRITE_BIT},
				VK_ACCESS_TRANSFER_WRITE_BIT);
		_source_format = 0x34325241u;
		if (image) {
			_source_format               = image->_format;
			VkImageMemoryBarrier const b = {
					.sType               = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
					.pNext               = {},
					.srcAccessMask       = 0,
					.dstAccessMask       = VK_ACCESS_TRANSFER_READ_BIT,
					.oldLayout           = VK_IMAGE_LAYOUT_GENERAL,
					.newLayout           = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
					.srcQueueFamilyIndex = VK_QUEUE_FAMILY_FOREIGN_EXT,
					.dstQueueFamilyIndex = _c->_family,
					.image               = image->_image,
					.subresourceRange =
							{
									.aspectMask     = VK_IMAGE_ASPECT_COLOR_BIT,
									.baseMipLevel   = 0,
									.levelCount     = 1,
									.baseArrayLayer = 0,
									.layerCount     = 1,
							},
			};
			vkCmdPipelineBarrier(_cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, nullptr,
								 0, nullptr, 1, &b);
			std::vector<VkBufferImageCopy> copies;
			for (uint32_t i = 0; i < _count; i++) {
				auto const r = _rects[i];
				if (static_cast<uint64_t>(r._x) + r._width > image->_width ||
					static_cast<uint64_t>(r._y) + r._height > image->_height) {
					return std::unexpected("visual rect outside committed buffer");
				}
				VkBufferImageCopy const cp = {
						.bufferOffset      = static_cast<uint64_t>(_rules[i]._offset) * _pixel_stride,
						.bufferRowLength   = {},
						.bufferImageHeight = {},
						.imageSubresource =
								{
										.aspectMask     = VK_IMAGE_ASPECT_COLOR_BIT,
										.mipLevel       = 0,
										.baseArrayLayer = 0,
										.layerCount     = 1,
								},
						.imageOffset =
								{
										.x = static_cast<int32_t>(r._x),
										.y = static_cast<int32_t>(r._y),
										.z = 0,
								},
						.imageExtent =
								{
										.width  = r._width,
										.height = r._height,
										.depth  = 1,
								},
				};
				copies.push_back(cp);
			}
			vkCmdCopyImageToBuffer(_cmd, image->_image, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, _pixels._buffer, _count,
								   copies.data());
			VkImageMemoryBarrier const release = {
					.sType               = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
					.pNext               = {},
					.srcAccessMask       = VK_ACCESS_TRANSFER_READ_BIT,
					.dstAccessMask       = 0,
					.oldLayout           = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
					.newLayout           = VK_IMAGE_LAYOUT_GENERAL,
					.srcQueueFamilyIndex = _c->_family,
					.dstQueueFamilyIndex = VK_QUEUE_FAMILY_FOREIGN_EXT,
					.image               = image->_image,
					.subresourceRange =
							{
									.aspectMask     = VK_IMAGE_ASPECT_COLOR_BIT,
									.baseMipLevel   = 0,
									.levelCount     = 1,
									.baseArrayLayer = 0,
									.layerCount     = 1,
							},
			};
			vkCmdPipelineBarrier(_cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0,
								 nullptr, 0, nullptr, 1, &release);
		} else {
			vkCmdFillBuffer(_cmd, _pixels._buffer, 0, _pixels._size, pattern);
		}
		// Only source acquisition delays forwarding. No agent computation or
		// GPU-derived CPU readback runs while foreign ownership is held.
		VISUAL_TRY(submit_wait(image != nullptr));
		_captured = true;
		return {};
	}
	[[nodiscard]] visual::Result<int> analyze(Visual_Event *output) {
		if (!_captured) {
			return std::unexpected("visual analysis requires an owned snapshot");
		}
		VISUAL_TRY(begin_commands());
		barrier(VkFlags{VK_PIPELINE_STAGE_TRANSFER_BIT} | VkFlags{VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT} |
						VkFlags{VK_PIPELINE_STAGE_HOST_BIT},
				VkFlags{VK_PIPELINE_STAGE_TRANSFER_BIT} | VkFlags{VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT},
				VkFlags{VK_ACCESS_TRANSFER_WRITE_BIT} | VkFlags{VK_ACCESS_SHADER_WRITE_BIT} |
						VkFlags{VK_ACCESS_HOST_WRITE_BIT},
				VkFlags{VK_ACCESS_TRANSFER_WRITE_BIT} | VkFlags{VK_ACCESS_SHADER_READ_BIT} |
						VkFlags{VK_ACCESS_SHADER_WRITE_BIT});
		vkCmdFillBuffer(_cmd, _events._buffer, 0, _events._size, 0);
		if (!_initialized) {
			vkCmdFillBuffer(_cmd, _state._buffer, 0, _state._size, 0);
			vkCmdFillBuffer(_cmd, _history._buffer, 0, _history._size, 0);
		}
		barrier(VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_ACCESS_TRANSFER_WRITE_BIT,
				VkFlags{VK_ACCESS_SHADER_READ_BIT} | VkFlags{VK_ACCESS_SHADER_WRITE_BIT});
		vkCmdBindPipeline(_cmd, VK_PIPELINE_BIND_POINT_COMPUTE, _c->_pipeline);
		vkCmdBindDescriptorSets(_cmd, VK_PIPELINE_BIND_POINT_COMPUTE, _c->_pipeline_layout, 0, 1, &_set, 0, nullptr);
		vkCmdPushConstants(_cmd, _c->_pipeline_layout, VK_SHADER_STAGE_COMPUTE_BIT, 0, 4, &_source_format);
		vkCmdDispatch(_cmd, _count, 1, 1);
		barrier(VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_HOST_BIT, VK_ACCESS_SHADER_WRITE_BIT,
				VK_ACCESS_HOST_READ_BIT);
		VISUAL_TRY(submit_wait(false));
		_initialized     = true;
		_captured        = false;
		uint32_t const n = *static_cast<uint32_t *>(_events._mapped);
		if (n > 16) {
			return std::unexpected("GPU event overflow");
		}
		std::memcpy(output, static_cast<char *>(_events._mapped) + 16, n * sizeof(Visual_Event));
		return static_cast<int>(n);
	}
	[[nodiscard]] visual::Status capture_buffer(uint64_t key, uint32_t w, uint32_t h, uint32_t fmt,
												const Visual_Plane *p, uint32_t n, bool screenshot = false) {
		if (!p || !n || n > 4) {
			return std::unexpected("invalid DMA-BUF planes");
		}
		Imported *image = nullptr;
		for (auto &entry: _imports) {
			if (entry->matches(key, w, h, fmt, std::span(p, n))) {
				image = entry.get();
				break;
			}
		}
		if (!image) {
			if (_imports.size() >= 8) {
				_imports.erase(_imports.begin());
			}
			auto entry = std::make_unique<Imported>(_c);
			VISUAL_TRY(entry->init(key, w, h, fmt, std::span(p, n), screenshot));
			image = entry.get();
			_imports.push_back(std::move(entry));
		}
		VISUAL_TRY(capture(image, 0));
		return {};
	}
	void set_color(const Visual_Color_Profile &next) {
		if (!same_color(_profile, next)) {
			_initialized = false;
		}
		_profile = next;
		std::memcpy(_source_color._mapped, &_profile, sizeof(_profile));
	}
	[[nodiscard]] visual::Status read_snapshot(uint8_t *raw, uint32_t output_bytes) {
		if (!_captured || !raw || output_bytes != _pixels._size) {
			return std::unexpected("invalid screenshot output/owned source");
		}
		Buffer output;
		VISUAL_TRY(output.init(_c, _pixels._size, VK_BUFFER_USAGE_TRANSFER_DST_BIT, true));
		VISUAL_TRY(begin_commands());
		barrier(VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_ACCESS_TRANSFER_WRITE_BIT,
				VK_ACCESS_TRANSFER_READ_BIT);
		VkBufferCopy const copy{
				.srcOffset = 0,
				.dstOffset = 0,
				.size      = output._size,
		};
		vkCmdCopyBuffer(_cmd, _pixels._buffer, output._buffer, 1, &copy);
		barrier(VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT, VK_ACCESS_TRANSFER_WRITE_BIT,
				VK_ACCESS_HOST_READ_BIT);
		VISUAL_TRY(submit_wait(false));
		std::memcpy(raw, output._mapped, static_cast<size_t>(output._size));
		return {};
	}
	[[nodiscard]] visual::Result<int> run(Imported *image, uint32_t pattern, Visual_Event *output) {
		VISUAL_TRY(capture(image, pattern));
		return analyze(output);
	}
	[[nodiscard]] visual::Result<int> process(uint64_t key, uint32_t w, uint32_t h, uint32_t fmt, const Visual_Plane *p,
											  uint32_t n, Visual_Event *output) {
		VISUAL_TRY(capture_buffer(key, w, h, fmt, p, n));
		return analyze(output);
	}
};

struct Cached_Snapshot_Native : visual::Non_Copyable {
	Program  _raw;
	uint32_t _width, _height, _format;
	Cached_Snapshot_Native(Context *c, uint32_t w, uint32_t h, uint32_t fmt)
		: _raw(c), _width(w), _height(h), _format(fmt) {}
	[[nodiscard]] visual::Status init() {
		if (!_width || !_height || static_cast<uint64_t>(_width) * _height > 8'388'608) {
			return std::unexpected("invalid GPU snapshot extent");
		}
		return _raw.init_raw(_width, _height, (_format == 0x48344258u || _format == 0x48344241u) ? 8 : 4);
	}
};
