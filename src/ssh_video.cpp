#include "ssh_video.h"
#include "h265/encoder.hpp"
#include "h265/frame_vaapi.hpp"
#include "ssh_video_spirv.h"
#include "ssh_preview_spirv.h"
#include "visual_gpu_internal.h"

#include <algorithm>
#include <array>
#include <cerrno>
#include <chrono>
#include <cmath>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <drm/drm_fourcc.h>
#include <fcntl.h>
#include <limits>
#include <linux/udmabuf.h>
#include <poll.h>
#include <memory>
#include <optional>
#include <utility>
#include <unordered_map>
#include <variant>
#include <sys/ioctl.h>
#include <sys/sysmacros.h>
#include <sys/mman.h>
#include <sys/types.h>
#include <unistd.h>
#include <va/va_drmcommon.h>
#include <va/va_vpp.h>
#include <vulkan/vulkan_core.h>

namespace {
	namespace hevc = video_editor::h265;
	namespace encode = video_editor::encode;
	using video_editor::av1::Color_Primaries;
	using video_editor::av1::Color_Range;
	using video_editor::av1::Matrix_Coefficients;
	using video_editor::av1::Transfer_Function;

	constexpr std::uint32_t DECODE = 1;
	constexpr std::uint32_t WITH_ALPHA = 2;
	constexpr std::uint32_t ALPHA_PICTURE = 4;
	constexpr std::uint32_t PREDICTIVE_PLAN = 64;
	constexpr std::uint32_t ALPHA_ONLY = 1024;
	constexpr std::uint32_t RGB_OUTPUT = 2048;
	constexpr std::uint32_t IDENTITY_SDR = 4096;
	constexpr std::uint32_t COMPARE_ALPHA = 8192;
	constexpr std::uint32_t SDR_TRANSFER_TABLE = 16384;
	constexpr std::uint32_t COLLECT_ALPHA = 32768;
	constexpr std::size_t TRANSFER_STEPS = 4096;
	constexpr std::uint32_t INDEPENDENT = UINT32_MAX;
	constexpr std::uint32_t GROUP_LENGTH = 240;
	constexpr std::uint8_t VAAPI_ALPHA_QP = 12;

	[[nodiscard]] visual::Status reserve_buffer(std::unique_ptr<Buffer> &buffer, Context *context, VkDeviceSize bytes,
											 VkBufferUsageFlags usage, bool host = false,
											 VkMemoryPropertyFlags preferred = 0) {
		if (!buffer || buffer->_size < bytes) {
			auto next = std::make_unique<Buffer>();
			VISUAL_TRY(next->init(context, bytes, usage, host, preferred));
			buffer = std::move(next);
		}
		return {};
	}

	VkBufferImageCopy packed_region(std::uint32_t width, std::uint32_t height) {
		return {
			.bufferOffset = 0, .bufferRowLength = 0, .bufferImageHeight = 0,
			.imageSubresource = {.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT, .mipLevel = 0, .baseArrayLayer = 0, .layerCount = 1},
			.imageOffset = {}, .imageExtent = {.width = width, .height = height, .depth = 1},
		};
	}

	std::array<VkBufferImageCopy, 2> video_regions(hevc::Frame_Layout layout) {
		if (layout.chroma != 420) {
			return {packed_region(layout.chroma == 422 ? layout.width / 2 : layout.width, layout.height), {}};
		}
		return {
			VkBufferImageCopy{
				.bufferOffset = 0, .bufferRowLength = 0, .bufferImageHeight = 0,
				.imageSubresource = {.aspectMask = VK_IMAGE_ASPECT_PLANE_0_BIT, .mipLevel = 0, .baseArrayLayer = 0, .layerCount = 1},
				.imageOffset = {}, .imageExtent = {.width = layout.width, .height = layout.height, .depth = 1},
			},
			VkBufferImageCopy{
				.bufferOffset = VkDeviceSize{layout.width} * layout.height * (layout.depth == 10 ? 2 : 1),
				.bufferRowLength = 0, .bufferImageHeight = 0,
				.imageSubresource = {.aspectMask = VK_IMAGE_ASPECT_PLANE_1_BIT, .mipLevel = 0, .baseArrayLayer = 0, .layerCount = 1},
				.imageOffset = {}, .imageExtent = {.width = layout.width / 2, .height = layout.height / 2, .depth = 1},
			},
		};
	}

	struct Va_Planes {
		struct Cache_Entry {
			std::uint64_t owner;
			VASurfaceID surface;
			std::unique_ptr<Imported> image;
		};
		std::vector<Cache_Entry> images;
		[[nodiscard]] Imported &current() const { return *images.back().image; }
		std::unique_ptr<Buffer> samples;

		[[nodiscard]] visual::Status prepare(Context &context, hevc::Va_Surface frame,
											hevc::Frame_Layout layout, bool alpha,
											VkImageUsageFlags usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT) {
			auto existing = std::ranges::find_if(images, [&](const auto &entry) {
				return entry.owner == frame.owner && entry.surface == frame.surface;
			});
			if (existing != images.end()) {
				std::rotate(existing, existing + 1, images.end());
				return reserve_samples(context, layout, alpha, usage);
			}
			struct Export {
				VADRMPRIMESurfaceDescriptor descriptor{};
				~Export() {
					for (std::uint32_t i = 0; i < descriptor.num_objects && i < std::size(descriptor.objects); ++i) {
						close(descriptor.objects[i].fd);
					}
				}
			} exported;
			auto &descriptor = exported.descriptor;
			const auto status = vaExportSurfaceHandle(frame.display, frame.surface, VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
				(usage == VK_IMAGE_USAGE_TRANSFER_SRC_BIT ? VA_EXPORT_SURFACE_READ_ONLY : VA_EXPORT_SURFACE_WRITE_ONLY) |
				VA_EXPORT_SURFACE_COMPOSED_LAYERS, &descriptor);
			if (status != VA_STATUS_SUCCESS) { return std::unexpected("cannot export VA surface"); }
			const auto &layer = descriptor.layers[0];
			const auto fourcc = layout.fourcc();
			const auto format = layout.chroma == 420 ? (layout.depth == 10
					? VK_FORMAT_G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16 : VK_FORMAT_G8_B8R8_2PLANE_420_UNORM)
				: layout.chroma == 422 ? VK_FORMAT_R16G16B16A16_UNORM
				: layout.depth == 10 ? VK_FORMAT_A2B10G10R10_UNORM_PACK32 : VK_FORMAT_R8G8B8A8_UNORM;
			const std::size_t count = layout.chroma == 420 ? 2 : 1;
			if (descriptor.num_objects != 1 || descriptor.num_layers != 1 || layer.drm_format != fourcc ||
				layer.num_planes != count || descriptor.width != layout.width || descriptor.height != layout.height) {
				return std::unexpected("unsupported VA surface plane geometry: " +
					std::to_string(layer.drm_format) + "/" + std::to_string(layer.num_planes));
			}
			const auto &object = descriptor.objects[0];
			std::array<Visual_Plane, 2> descriptors{};
			for (std::size_t i = 0; i < count; ++i) {
				if (layer.object_index[i] != 0) { return std::unexpected("unsupported VA surface allocation"); }
				descriptors[i] = {
					._fd = object.fd, ._offset = layer.offset[i], ._stride = layer.pitch[i], ._modifier = object.drm_format_modifier,
				};
			}
			const auto planes = std::span(descriptors).first(count);
			struct stat allocation{};
			if (fstat(object.fd, &allocation) != 0) { return std::unexpected("cannot identify VA-API DMA-BUF"); }
			auto image = std::make_unique<Imported>(&context);
			// Y210 has four 16-bit words per two pixels. The RGB alias imports the
			// same byte layout on drivers without transfer support for Y210.
			const auto image_width = layout.chroma == 422 ? layout.width / 2 : layout.width;
			VISUAL_TRY(image->init(allocation.st_ino, image_width, layout.height, fourcc, planes, false, format, usage));
			if (images.size() == 20) { images.erase(images.begin()); }
			images.push_back({.owner = frame.owner, .surface = frame.surface, .image = std::move(image)});
			return reserve_samples(context, layout, alpha, usage);
		}

		[[nodiscard]] visual::Status reserve_samples(Context &context, hevc::Frame_Layout layout, bool alpha,
				VkImageUsageFlags usage) {
			if (usage == VK_IMAGE_USAGE_TRANSFER_DST_BIT) { return {}; }
			const auto luma_bytes = VkDeviceSize{layout.width} * layout.height * (layout.depth == 10 ? 2 : 1);
			const auto bytes = layout.chroma == 420 ? (alpha ? luma_bytes : luma_bytes + luma_bytes / 2)
				: VkDeviceSize{layout.width} * layout.height * 4;
			return reserve_buffer(samples, &context, bytes,
								  VK_BUFFER_USAGE_STORAGE_BUFFER_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT);
		}

		void read(Program &commands, hevc::Frame_Layout layout, bool alpha) const {
			const auto family = commands._c->_family;
			const auto &image = current();
			const VkImageSubresourceRange range{
				.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT, .baseMipLevel = 0, .levelCount = 1, .baseArrayLayer = 0, .layerCount = 1,
			};
			const VkImageMemoryBarrier acquire{
				.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, .pNext = nullptr,
				.srcAccessMask = 0, .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT,
				.oldLayout = VK_IMAGE_LAYOUT_GENERAL, .newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
				.srcQueueFamilyIndex = VK_QUEUE_FAMILY_FOREIGN_EXT, .dstQueueFamilyIndex = family,
				.image = image._image, .subresourceRange = range,
			};
			vkCmdPipelineBarrier(commands._cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT,
				0, 0, nullptr, 0, nullptr, 1, &acquire);
			const auto copies = video_regions(layout);
			vkCmdCopyImageToBuffer(commands._cmd, image._image, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, samples->_buffer,
								   alpha || layout.chroma != 420 ? 1 : 2, copies.data());
			const VkImageMemoryBarrier release{
				.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, .pNext = nullptr,
				.srcAccessMask = VK_ACCESS_TRANSFER_READ_BIT, .dstAccessMask = 0,
				.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, .newLayout = VK_IMAGE_LAYOUT_GENERAL,
				.srcQueueFamilyIndex = family, .dstQueueFamilyIndex = VK_QUEUE_FAMILY_FOREIGN_EXT,
				.image = image._image, .subresourceRange = range,
			};
			vkCmdPipelineBarrier(commands._cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT,
				0, 0, nullptr, 0, nullptr, 1, &release);
		}
	};

	void upload_image(Program &commands, const Buffer &source, const Imported &target,
						 std::span<const VkBufferImageCopy> copies) {
		const auto family = commands._c->_family;
		const VkImageSubresourceRange range{
			.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT, .baseMipLevel = 0, .levelCount = 1, .baseArrayLayer = 0, .layerCount = 1,
		};
		const VkImageMemoryBarrier acquire{
			.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, .pNext = nullptr,
			.srcAccessMask = 0, .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
			.oldLayout = VK_IMAGE_LAYOUT_GENERAL, .newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
			.srcQueueFamilyIndex = VK_QUEUE_FAMILY_FOREIGN_EXT, .dstQueueFamilyIndex = family,
			.image = target._image, .subresourceRange = range,
		};
		vkCmdPipelineBarrier(commands._cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT,
			0, 0, nullptr, 0, nullptr, 1, &acquire);
		vkCmdCopyBufferToImage(commands._cmd, source._buffer, target._image, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
			copies.size(), copies.data());
		const VkImageMemoryBarrier release{
			.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, .pNext = nullptr,
			.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT, .dstAccessMask = 0,
			.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, .newLayout = VK_IMAGE_LAYOUT_GENERAL,
			.srcQueueFamilyIndex = family, .dstQueueFamilyIndex = VK_QUEUE_FAMILY_FOREIGN_EXT,
			.image = target._image, .subresourceRange = range,
		};
		vkCmdPipelineBarrier(commands._cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT,
			0, 0, nullptr, 0, nullptr, 1, &release);
	}

	struct Host_Image {
		struct Mapping {
			void *address = MAP_FAILED;
			std::size_t size{};
			~Mapping() { if (address != MAP_FAILED) { munmap(address, size); } }
		} map;
		Unique_Fd _source_fd{-1};
		Buffer buffer;
		dev_t device{};
		ino_t inode{};
		std::uint64_t start{};
		bool writing{};

		[[nodiscard]] visual::Status init(Context &context, int fd, std::uint64_t offset, VkDeviceSize bytes) {
			buffer._c = &context;
			buffer._size = bytes;
			const VkExternalMemoryBufferCreateInfo external{
				.sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO, .pNext = nullptr,
				.handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT,
			};
			const VkBufferCreateInfo info{
				.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .pNext = &external, .flags = 0,
				.size = bytes, .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
				.queueFamilyIndexCount = 0, .pQueueFamilyIndices = nullptr,
			};
			VISUAL_TRY(vk_status(vkCreateBuffer(context._device, &info, nullptr, &buffer._buffer), "create Wayland SHM GPU buffer"));
			VkMemoryRequirements req{};
			vkGetBufferMemoryRequirements(context._device, buffer._buffer, &req);
			map.size = std::max(req.size, bytes);
			map.address = mmap(nullptr, map.size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, static_cast<off_t>(offset));
			if (map.address == MAP_FAILED) { return std::unexpected("map Wayland SHM for GPU import"); }
			if (reinterpret_cast<std::uintptr_t>(map.address) % context._host_pointer_alignment != 0) {
				return std::unexpected("Wayland SHM mapping does not meet GPU pointer alignment");
			}
			const auto props = reinterpret_cast<PFN_vkGetMemoryHostPointerPropertiesEXT>(
				vkGetDeviceProcAddr(context._device, "vkGetMemoryHostPointerPropertiesEXT"));
			VkMemoryHostPointerPropertiesEXT properties{
				.sType = VK_STRUCTURE_TYPE_MEMORY_HOST_POINTER_PROPERTIES_EXT, .pNext = nullptr, .memoryTypeBits = 0,
			};
			if (!props) { return std::unexpected("GPU host memory import entry point unavailable"); }
			VISUAL_TRY(vk_status(props(context._device, VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT,
				map.address, &properties), "query Wayland SHM host pointer"));
			const auto type = context.memory_type(req.memoryTypeBits & properties.memoryTypeBits,
				VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
			if (!type) { return std::unexpected(type.error()); }
			const VkImportMemoryHostPointerInfoEXT import{
				.sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_HOST_POINTER_INFO_EXT, .pNext = nullptr,
				.handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT, .pHostPointer = map.address,
			};
			const VkMemoryAllocateInfo allocate{
				.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .pNext = &import,
				.allocationSize = map.size, .memoryTypeIndex = *type,
			};
			VISUAL_TRY(vk_status(vkAllocateMemory(context._device, &allocate, nullptr, &buffer._memory), "import Wayland SHM into GPU"));
			return vk_status(vkBindBufferMemory(context._device, buffer._buffer, buffer._memory, 0), "bind Wayland SHM GPU buffer");
		}

		[[nodiscard]] visual::Status import_dma_buf(Context &context, int fd, std::uint64_t offset, VkDeviceSize bytes,
			VkDeviceSize visible_end) {
			// The udmabuf exporter requires sealed backing. Vulkan host-memory
			// import can use an ordinary wl_shm mapping directly.
			const auto seals = fcntl(fd, F_GET_SEALS);
			if (seals < 0 || ((seals & F_SEAL_SHRINK) == 0 && fcntl(fd, F_ADD_SEALS, F_SEAL_SHRINK) != 0)) {
				return std::unexpected("Wayland SHM needs a shrink seal for DMA-BUF export");
			}
			struct stat storage{};
			if (fstat(fd, &storage) != 0 || storage.st_size < static_cast<off_t>(visible_end)) {
				return std::unexpected("Wayland SHM extent is outside the sealed file");
			}
			// udmabuf pins whole pages and rejects a range past EOF. Reserve
			// only the tail padding and keep any concurrent pool growth.
			const auto end = static_cast<off_t>(offset + bytes);
			if (storage.st_size < end && fallocate(fd, 0, end - 1, 1) != 0) {
				return std::unexpected(std::string("page-align Wayland SHM for DMA-BUF export: ") + std::strerror(errno));
			}
			// Keep the cached source inode live after the client closes its memfd.
			_source_fd._value = fcntl(fd, F_DUPFD_CLOEXEC, 3);
			if (_source_fd._value < 0) { return std::unexpected("duplicate Wayland SHM memfd failed"); }
			const Unique_Fd exporter(open("/dev/udmabuf", O_RDWR | O_CLOEXEC));
			if (exporter._value < 0) { return std::unexpected("open /dev/udmabuf failed"); }
			const udmabuf_create range{
				.memfd = static_cast<std::uint32_t>(_source_fd._value), .flags = UDMABUF_FLAGS_CLOEXEC,
				.offset = offset, .size = bytes,
			};
			Unique_Fd dma_buf(ioctl(exporter._value, UDMABUF_CREATE, &range));
			if (dma_buf._value < 0) {
				return std::unexpected(std::string("export sealed Wayland SHM as DMA-BUF: ") + std::strerror(errno));
			}
			constexpr auto HANDLE_TYPE = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT;
			const VkPhysicalDeviceExternalBufferInfo query{
				.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_BUFFER_INFO, .pNext = nullptr,
				.flags = 0, .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, .handleType = HANDLE_TYPE,
			};
			VkExternalBufferProperties support{
				.sType = VK_STRUCTURE_TYPE_EXTERNAL_BUFFER_PROPERTIES, .pNext = nullptr, .externalMemoryProperties = {},
			};
			vkGetPhysicalDeviceExternalBufferProperties(context._physical, &query, &support);
			if ((support.externalMemoryProperties.externalMemoryFeatures & VK_EXTERNAL_MEMORY_FEATURE_IMPORTABLE_BIT) == 0) {
				return std::unexpected("GPU does not import DMA-BUF storage buffers");
			}
			buffer._c = &context;
			buffer._size = bytes;
			const VkExternalMemoryBufferCreateInfo external{
				.sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO, .pNext = nullptr,
				.handleTypes = HANDLE_TYPE,
			};
			const VkBufferCreateInfo info{
				.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .pNext = &external, .flags = 0,
				.size = bytes, .usage = query.usage, .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
				.queueFamilyIndexCount = 0, .pQueueFamilyIndices = nullptr,
			};
			VISUAL_TRY(vk_status(vkCreateBuffer(context._device, &info, nullptr, &buffer._buffer), "create Wayland SHM DMA-BUF buffer"));
			VkMemoryRequirements requirements{};
			vkGetBufferMemoryRequirements(context._device, buffer._buffer, &requirements);
			if (requirements.size > bytes) { return std::unexpected("Wayland SHM DMA-BUF is smaller than the Vulkan buffer"); }
			const auto props = reinterpret_cast<PFN_vkGetMemoryFdPropertiesKHR>(
				vkGetDeviceProcAddr(context._device, "vkGetMemoryFdPropertiesKHR"));
			if (!props) { return std::unexpected("GPU DMA-BUF memory properties entry point unavailable"); }
			VkMemoryFdPropertiesKHR properties{
				.sType = VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR, .pNext = nullptr, .memoryTypeBits = 0,
			};
			VISUAL_TRY(vk_status(props(context._device, HANDLE_TYPE, dma_buf._value, &properties), "query Wayland SHM DMA-BUF"));
			const auto type = context.memory_type(requirements.memoryTypeBits & properties.memoryTypeBits, 0);
			if (!type) { return std::unexpected(type.error()); }
			const VkImportMemoryFdInfoKHR import{
				.sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR, .pNext = nullptr,
				.handleType = HANDLE_TYPE, .fd = dma_buf._value,
			};
			const VkMemoryDedicatedAllocateInfo dedicated{
				.sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO, .pNext = &import,
				.image = nullptr, .buffer = buffer._buffer,
			};
			const VkMemoryAllocateInfo allocate{
				.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .pNext = &dedicated,
				.allocationSize = bytes, .memoryTypeIndex = *type,
			};
			VISUAL_TRY(vk_status(vkAllocateMemory(context._device, &allocate, nullptr, &buffer._memory), "import Wayland SHM DMA-BUF"));
			dma_buf.release();
			return vk_status(vkBindBufferMemory(context._device, buffer._buffer, buffer._memory, 0), "bind Wayland SHM DMA-BUF");
		}
	};

	[[nodiscard]] visual::Status create_pipeline(Context &context, VkPipeline &pipeline, std::span<const std::uint32_t> code,
			const VkSpecializationInfo *specialization = nullptr) {
		if (pipeline) { return {}; }
		const VkShaderModuleCreateInfo module_info{
			.sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, .pNext = nullptr, .flags = 0,
			.codeSize = code.size_bytes(), .pCode = code.data(),
		};
		VkShaderModule shader{};
		VISUAL_TRY(vk_status(vkCreateShaderModule(context._device, &module_info, nullptr, &shader), "create video shader"));
		const VkComputePipelineCreateInfo pipeline_info{
			.sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO, .pNext = nullptr, .flags = 0,
			.stage = {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .pNext = nullptr, .flags = 0,
				.stage = VK_SHADER_STAGE_COMPUTE_BIT, .module = shader, .pName = "main", .pSpecializationInfo = specialization},
			.layout = context._pipeline_layout, .basePipelineHandle = {}, .basePipelineIndex = -1,
		};
		const auto status = vkCreateComputePipelines(context._device, {}, 1, &pipeline_info, nullptr, &pipeline);
		vkDestroyShaderModule(context._device, shader, nullptr);
		return vk_status(status, "create video pipeline");
	}

	struct Video_Converter : visual::Non_Copyable {
		struct Source_Submission {
			Program *_commands{};
			bool _wait_for_producer{};
			const Buffer *_planar_alpha{};
			const Imported *_planar_output{};
			const Buffer *_buffer_output{};
			const Buffer *_alpha_history{};
		};

		Context *context;
		Program commands;
		VkDescriptorPool pool{};
		VkDescriptorSet set{};
		Buffer color, options, alpha_status;
		std::uint32_t _table_transfer{};
		struct Conversion_Pipeline {
			std::array<std::uint32_t, 4> _parameters;
			VkPipeline _pipeline{};
		};
		std::vector<Conversion_Pipeline> _conversion_pipelines;
		std::unique_ptr<Buffer> device_output;
		std::unique_ptr<Buffer> preview_pixels, compressed_blocks, block_info;

		explicit Video_Converter(Context *c) : context(c), commands(c) {}
		~Video_Converter() {
			for (const auto &variant : _conversion_pipelines) {
				vkDestroyPipeline(context->_device, variant._pipeline, nullptr);
			}
			if (pool) { vkDestroyDescriptorPool(context->_device, pool, nullptr); }
		}

		[[nodiscard]] visual::Status init() {
			VISUAL_TRY(commands.init_commands());
			VISUAL_TRY(color.init(context, sizeof(Visual_Color_Profile) + 2 * (TRANSFER_STEPS + 1) * sizeof(float),
				VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, true));
			VISUAL_TRY(options.init(context, sizeof(Video_Options), VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, true));
			VISUAL_TRY(alpha_status.init(context, sizeof(std::uint32_t), VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, true));
			VISUAL_TRY(create_pipeline(*context, context->_video_pipeline, ssh_video_spirv));
			const VkDescriptorPoolSize size{.type = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .descriptorCount = 6};
			const VkDescriptorPoolCreateInfo pool_info{
				.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO, .pNext = nullptr, .flags = 0,
				.maxSets = 1, .poolSizeCount = 1, .pPoolSizes = &size,
			};
			VISUAL_TRY(vk_status(vkCreateDescriptorPool(context->_device, &pool_info, nullptr, &pool),
								"create video descriptor pool"));
			const VkDescriptorSetAllocateInfo allocation{
				.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO, .pNext = nullptr,
				.descriptorPool = pool, .descriptorSetCount = 1, .pSetLayouts = &context->_layout,
			};
			return vk_status(vkAllocateDescriptorSets(context->_device, &allocation, &set), "allocate video descriptors");
		}

		void prepare_transfer_table(Video_Options &config, const Visual_Color_Profile &profile) {
			if ((config.mode & (DECODE | ALPHA_ONLY | IDENTITY_SDR)) != 0 || config.tone_map != 0 ||
				config.exposure != 1.0f || config.background != 0 || profile._power != 0.0f ||
				(profile._transfer != 2 && profile._transfer != 9 && profile._transfer != 14)) {
				return;
			}
			switch (config.format) {
				case DRM_FORMAT_XRGB8888:
				case DRM_FORMAT_XBGR8888:
				case DRM_FORMAT_XRGB2101010:
				case DRM_FORMAT_XBGR2101010:
				case DRM_FORMAT_ARGB8888:
				case DRM_FORMAT_ABGR8888:
				case DRM_FORMAT_ARGB2101010:
				case DRM_FORMAT_ABGR2101010:
					break;
				default:
					return;
			}
			config.mode |= SDR_TRANSFER_TABLE;
			if (_table_transfer == profile._transfer) { return; }
			// Only transfer-function constants are populated here. Frame sampling
			// and color conversion stay in the shader, including the source primaries.
			auto *table = reinterpret_cast<float *>(static_cast<std::byte *>(color._mapped) + sizeof(profile));
			for (std::size_t i = 0; i <= TRANSFER_STEPS; ++i) {
				const auto input = static_cast<float>(i) / TRANSFER_STEPS;
				table[i] = profile._transfer == 2 ? std::pow(input, 2.2f) :
					input <= 0.04045f ? input / 12.92f : std::pow((input + 0.055f) / 1.055f, 2.4f);
				table[TRANSFER_STEPS + 1 + i] = input <= 0.0031308f ? 12.92f * input :
					1.055f * std::pow(input, 1.0f / 2.4f) - 0.055f;
			}
			_table_transfer = profile._transfer;
		}

		void set_color(const Visual_Color_Profile &profile) {
			constexpr float BT2020_TO_BT709[3][3] = {
				{1.660491f, -.587641f, -.072850f},
				{-.124550f, 1.132900f, -.008349f},
				{-.018151f, -.100579f, 1.118730f},
			};
			auto display_profile = profile;
			for (std::size_t row = 0; row < 3; ++row) {
				for (std::size_t channel = 0; channel < 3; ++channel) {
					display_profile._matrix[row][channel] = BT2020_TO_BT709[row][0] * profile._matrix[0][channel] +
						BT2020_TO_BT709[row][1] * profile._matrix[1][channel] +
						BT2020_TO_BT709[row][2] * profile._matrix[2][channel];
				}
			}
			std::memcpy(color._mapped, &display_profile, sizeof(display_profile));
		}

		[[nodiscard]] visual::Result<VkPipeline> pipeline(const Video_Options &config) {
			if ((config.mode & SDR_TRANSFER_TABLE) == 0 || config.depth != 8 || config.chroma != 420) {
				return context->_video_pipeline;
			}
			const std::array parameters{config.mode, config.format, config.depth, config.chroma};
			for (const auto &variant : _conversion_pipelines) {
				if (variant._parameters == parameters) { return variant._pipeline; }
			}
			constexpr std::array ENTRIES{
				VkSpecializationMapEntry{.constantID = 0, .offset = 0, .size = sizeof(std::uint32_t)},
				VkSpecializationMapEntry{.constantID = 1, .offset = 4, .size = sizeof(std::uint32_t)},
				VkSpecializationMapEntry{.constantID = 2, .offset = 8, .size = sizeof(std::uint32_t)},
				VkSpecializationMapEntry{.constantID = 3, .offset = 12, .size = sizeof(std::uint32_t)},
			};
			const VkSpecializationInfo specialization{
				.mapEntryCount = ENTRIES.size(), .pMapEntries = ENTRIES.data(),
				.dataSize = sizeof(parameters), .pData = parameters.data(),
			};
			Conversion_Pipeline variant{._parameters = parameters, ._pipeline = nullptr};
			VISUAL_TRY(create_pipeline(*context, variant._pipeline, ssh_video_spirv, &specialization));
			if (_conversion_pipelines.size() == 12) {
				vkDestroyPipeline(context->_device, _conversion_pipelines.front()._pipeline, nullptr);
				_conversion_pipelines.erase(_conversion_pipelines.begin());
			}
			_conversion_pipelines.push_back(variant);
			return variant._pipeline;
		}

		[[nodiscard]] visual::Status reserve_planar_output(const Video_Options &config) {
			const VkDeviceSize pixels = (config.mode & RGB_OUTPUT) != 0
				? VkDeviceSize{config.width} * config.height : VkDeviceSize{config.coded_width} * config.coded_height;
			const auto bytes = (config.mode & RGB_OUTPUT) != 0 || config.chroma != 420 ? pixels * 4
				: (config.depth == 10 ? pixels * 3 : pixels * 3 / 2);
			return reserve_buffer(device_output, context, bytes, VK_BUFFER_USAGE_STORAGE_BUFFER_BIT | VK_BUFFER_USAGE_TRANSFER_SRC_BIT);
		}

		template<std::size_t N>
		void bind_buffers(const std::array<const Buffer *, N> &buffers) const {
			std::array<VkDescriptorBufferInfo, N> infos{};
			std::array<VkWriteDescriptorSet, N> writes{};
			for (std::uint32_t i = 0; i < N; ++i) {
				infos[i] = {.buffer = buffers[i]->_buffer, .offset = 0, .range = buffers[i]->_size};
				writes[i] = {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .pNext = nullptr, .dstSet = set,
							 .dstBinding = i, .dstArrayElement = 0, .descriptorCount = 1,
							 .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .pImageInfo = nullptr,
							 .pBufferInfo = &infos[i], .pTexelBufferView = nullptr};
			}
			vkUpdateDescriptorSets(context->_device, writes.size(), writes.data(), 0, nullptr);
		}

		[[nodiscard]] visual::Status convert(const Buffer &input, Video_Options config,
				const Visual_Color_Profile &profile, const Source_Submission &source) {
			if (source._planar_output) { VISUAL_TRY(reserve_planar_output(config)); }
			const auto *destination = source._buffer_output ? source._buffer_output : device_output.get();
			if (!destination) { return std::unexpected("video conversion is missing a GPU target"); }
			prepare_transfer_table(config, profile);
			const auto compute = pipeline(config);
			if (!compute) { return std::unexpected(compute.error()); }
			set_color(profile);
			std::memcpy(options._mapped, &config, sizeof(config));
			const auto *alpha = source._alpha_history ? source._alpha_history : source._planar_alpha;
			if ((config.mode & (ALPHA_ONLY | COLLECT_ALPHA)) != 0) { *static_cast<std::uint32_t *>(alpha_status._mapped) = 0; }
			const std::array<const Buffer *, 6> buffers{&input, destination, &color, &options, alpha ? alpha : &options, &alpha_status};
			bind_buffers(buffers);
			auto &submission = source._commands ? *source._commands : commands;
			if (!source._commands) {
				VISUAL_TRY(submission.begin_commands());
			}
			submission.barrier(VK_PIPELINE_STAGE_TRANSFER_BIT | VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
				VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_ACCESS_TRANSFER_WRITE_BIT | VK_ACCESS_SHADER_WRITE_BIT,
				VK_ACCESS_SHADER_READ_BIT | VK_ACCESS_SHADER_WRITE_BIT);
			vkCmdBindPipeline(submission._cmd, VK_PIPELINE_BIND_POINT_COMPUTE, *compute);
			vkCmdBindDescriptorSets(submission._cmd, VK_PIPELINE_BIND_POINT_COMPUTE, context->_pipeline_layout,
									0, 1, &set, 0, nullptr);
			const bool decoding = (config.mode & DECODE) != 0;
			const auto width = decoding ? config.width : config.coded_width /
				(config.chroma == 444 ? 1 : config.depth == 8 && config.chroma == 420 ? 4 : 2);
			const auto height = decoding ? config.height : config.coded_height / (config.chroma == 420 ? 2 : 1);
			vkCmdDispatch(submission._cmd, (width + 15) / 16, (height + 15) / 16, 1);
			if (source._planar_output) {
				submission.barrier(VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT,
					VK_ACCESS_SHADER_WRITE_BIT, VK_ACCESS_TRANSFER_READ_BIT);
				if ((config.mode & RGB_OUTPUT) != 0) {
					const std::array copies{packed_region(config.width, config.height)};
					upload_image(submission, *device_output, *source._planar_output, copies);
				} else {
					const auto copies = video_regions({
						.width = config.coded_width, .height = config.coded_height,
						.depth = config.depth, .chroma = config.chroma,
					});
					upload_image(submission, *device_output, *source._planar_output,
						std::span(copies).first(config.chroma == 420 ? 2 : 1));
				}
			}
			if (source._buffer_output || (config.mode & (ALPHA_ONLY | COLLECT_ALPHA)) != 0) {
				submission.barrier(VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_HOST_BIT,
					VK_ACCESS_SHADER_WRITE_BIT, VK_ACCESS_HOST_READ_BIT);
			}
			using Clock = std::chrono::steady_clock;
			static const bool trace = std::getenv("WAYLAND_MCP_TRACE_TIMING") != nullptr;
			const auto submitted = trace ? Clock::now() : Clock::time_point{};
			Program::Submission_Timing timing{};
			VISUAL_TRY(submission.submit_wait(source._wait_for_producer, trace ? &timing : nullptr));
			if (trace) {
				using Milliseconds = std::chrono::duration<double, std::milli>;
				std::fprintf(stderr, "SSH convert %s %ux%u fence_ms=%.3f finish_ms=%.3f submit_ms=%.3f wait_ms=%.3f\n",
					decoding ? "decode" : "encode", config.width, config.height,
					Milliseconds(Clock::now() - submitted).count(), timing.finish_ms, timing.submit_ms, timing.wait_ms);
			}
			return {};
		}
		[[nodiscard]] visual::Result<std::vector<std::uint8_t>> deflate_preview(std::uint32_t width, std::uint32_t height) {
			constexpr std::uint32_t chunk_size = 2048;
			constexpr std::uint32_t slot_size = chunk_size * 9 / 8 + 12;
			constexpr std::uint32_t adler_prime = 65521;
			const auto blocks = ((width * 3 + 1) * height + chunk_size - 1) / chunk_size;
			struct Block {
				std::uint32_t encoded, decoded, sum, weighted;
			};
			static_assert(sizeof(Block) == 16);
			constexpr auto storage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT;
			VISUAL_TRY(reserve_buffer(compressed_blocks, context, VkDeviceSize{blocks} * slot_size, storage, true,
				VK_MEMORY_PROPERTY_HOST_CACHED_BIT));
			VISUAL_TRY(reserve_buffer(block_info, context, VkDeviceSize{blocks} * sizeof(Block), storage, true));
			VISUAL_TRY(create_pipeline(*context, context->_preview_pipeline, ssh_preview_spirv));
			const std::array extent{width, height};
			std::memcpy(options._mapped, extent.data(), sizeof(extent));
			bind_buffers(std::array<const Buffer *, 4>{preview_pixels.get(), compressed_blocks.get(), block_info.get(), &options});
			VISUAL_TRY(commands.begin_commands());
			commands.barrier(VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
				VK_ACCESS_SHADER_WRITE_BIT, VK_ACCESS_SHADER_READ_BIT);
			vkCmdBindPipeline(commands._cmd, VK_PIPELINE_BIND_POINT_COMPUTE, context->_preview_pipeline);
			vkCmdBindDescriptorSets(commands._cmd, VK_PIPELINE_BIND_POINT_COMPUTE, context->_pipeline_layout, 0, 1, &set, 0, nullptr);
			vkCmdDispatch(commands._cmd, (blocks + 31) / 32, 1, 1);
			commands.barrier(VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_HOST_BIT,
				VK_ACCESS_SHADER_WRITE_BIT, VK_ACCESS_HOST_READ_BIT);
			VISUAL_TRY(commands.submit_wait(false));

			const auto *const compressed = static_cast<const std::uint8_t *>(compressed_blocks->_mapped);
			const auto *const info = static_cast<const Block *>(block_info->_mapped);
			std::uint32_t sum = 1;
			std::uint32_t weighted = 0;
			std::vector<std::uint8_t> zlib{0x78, 0x01};
			std::size_t total = 11;
			for (std::uint32_t i = 0; i < blocks; ++i) {
				if (info[i].encoded == 0 || info[i].encoded > slot_size || info[i].decoded == 0 || info[i].decoded > chunk_size ||
					info[i].sum >= adler_prime || info[i].weighted >= adler_prime) {
					return std::unexpected("invalid GPU PNG block");
				}
				total += info[i].encoded;
			}
			zlib.reserve(total);
			for (std::uint32_t i = 0; i < blocks; ++i) {
				const auto begin = compressed + VkDeviceSize{i} * slot_size;
				zlib.insert(zlib.end(), begin, begin + info[i].encoded);
				weighted = (weighted + info[i].weighted + info[i].decoded * (sum + adler_prime - 1)) % adler_prime;
				sum = (sum + info[i].sum + adler_prime - 1) % adler_prime;
			}
			const auto adler = (weighted << 16) | sum;
			zlib.insert(zlib.end(), {0x01, 0x00, 0x00, 0xff, 0xff,
				static_cast<std::uint8_t>(adler >> 24), static_cast<std::uint8_t>(adler >> 16),
				static_cast<std::uint8_t>(adler >> 8), static_cast<std::uint8_t>(adler)});
			return zlib;
		}
	};

	[[nodiscard]] visual::Result<Imported *> import_output(Context &context, std::vector<std::unique_ptr<Imported>> &images,
			const Video_Options &options, const Visual_Plane &destination, std::uint32_t destination_width) {
		struct stat allocation{};
		if (fstat(destination._fd, &allocation) != 0) {
			return std::unexpected("cannot identify the destination DMA-BUF");
		}
		const auto planes = std::span(&destination, 1);
		auto existing = std::ranges::find_if(images, [&](const auto &entry) {
			return entry->matches(allocation.st_ino, destination_width, options.height, options.format, planes);
		});
		if (existing != images.end()) {
			std::rotate(existing, existing + 1, images.end());
		} else {
			auto image = std::make_unique<Imported>(&context);
			VISUAL_TRY(image->init(allocation.st_ino, destination_width, options.height, options.format, planes,
				false, VK_FORMAT_UNDEFINED, VK_IMAGE_USAGE_TRANSFER_DST_BIT));
			if (images.size() == 16) { images.erase(images.begin()); }
			images.push_back(std::move(image));
		}
		return images.back().get();
	}

	struct Snapshot_Converter {
		Video_Converter converter;
		Va_Planes input;
		std::vector<std::unique_ptr<Imported>> staging;
		explicit Snapshot_Converter(Context *context) : converter(context) {}
	};

	visual::Result<Snapshot_Converter *> snapshot_converter(Context &context) {
		if (!context._snapshot_converter) {
			auto next = std::make_shared<Snapshot_Converter>(&context);
			VISUAL_TRY(next->converter.init());
			context._snapshot_converter = std::move(next);
		}
		return static_cast<Snapshot_Converter *>(context._snapshot_converter.get());
	}

	using Video_Packet = std::vector<std::uint8_t>;

	struct Device_Converter {
		dev_t device;
		Context context;
		std::unique_ptr<Video_Converter> converter;
		Va_Planes va_color, va_alpha, va_input;
		std::vector<std::unique_ptr<Imported>> display_images;
		std::array<std::unique_ptr<Cached_Snapshot_Native>, 2> _snapshots;
		explicit Device_Converter(dev_t node) : device(node) {}

		std::vector<std::unique_ptr<Host_Image>> shm_images;

		[[nodiscard]] visual::Result<Cached_Snapshot_Native *> snapshot(const Video_Options &options) {
			const auto matches = [&](const std::unique_ptr<Cached_Snapshot_Native> &entry) {
				return entry && entry->_width == options.width && entry->_height == options.height &&
					   entry->_format == options.format;
			};
			auto &current = _snapshots.front();
			if (!matches(current) && matches(_snapshots.back())) { std::swap(current, _snapshots.back()); }
			if (!matches(current)) {
				auto next = std::make_unique<Cached_Snapshot_Native>(&context, options.width, options.height, options.format);
				VISUAL_TRY(next->init());
				_snapshots.back() = std::move(current);
				current = std::move(next);
			}
			return current.get();
		}


		[[nodiscard]] visual::Result<Host_Image *> import_shm(const Video_Shm_Source &source,
				std::uint32_t width, std::uint32_t height, bool output, std::uint32_t &offset) {
			struct stat allocation{};
			const auto bytes = std::uint64_t(height - 1) * source._stride + std::uint64_t(width) * 4;
			if (source._stride < std::uint64_t(width) * 4 || (output && (source._stride != width * 4 || source._offset % 4 != 0)) ||
				fstat(source._fd, &allocation) != 0 || allocation.st_size < 0 ||
				source._offset > static_cast<std::uint64_t>(allocation.st_size) ||
				bytes > static_cast<std::uint64_t>(allocation.st_size) - source._offset) {
				return std::unexpected("invalid Wayland SHM GPU import extent");
			}
			const auto page_size = sysconf(_SC_PAGESIZE);
			if (page_size <= 0) {
				return std::unexpected("could not determine Wayland SHM page size");
			}
			const auto alignment = std::max(context._host_pointer_alignment, static_cast<VkDeviceSize>(page_size));
			if (alignment > UINT32_MAX) { return std::unexpected("unsupported GPU host-pointer alignment"); }
			const auto start = source._offset - source._offset % alignment;
			offset = static_cast<std::uint32_t>(source._offset - start);
			const auto size = (offset + bytes + alignment - 1) / alignment * alignment;
			VkPhysicalDeviceProperties props{};
			vkGetPhysicalDeviceProperties(context._physical, &props);
			if (size > props.limits.maxStorageBufferRange) { return std::unexpected("Wayland SHM GPU buffer exceeds storage limit"); }
			auto existing = std::ranges::find_if(shm_images, [&](const auto &image) {
				return image->device == allocation.st_dev && image->inode == allocation.st_ino &&
					image->start == start && image->buffer._size == size && image->writing == output;
			});
			if (existing != shm_images.end()) {
				std::rotate(existing, existing + 1, shm_images.end());
			} else {
				auto image = std::make_unique<Host_Image>();
				const auto mapped = context._host_pointer_alignment ? image->init(context, source._fd, start, size)
					: visual::Status(std::unexpected("GPU host memory import unavailable"));
				if (!mapped) {
					// Try exporting the same pages when the driver rejects a host-pointer import.
					image = std::make_unique<Host_Image>();
					const auto imported = image->import_dma_buf(context, source._fd, start, size, source._offset + bytes);
					if (!imported) { return std::unexpected(mapped.error() + "; " + imported.error()); }
					if (std::getenv("WAYLAND_MCP_TRACE_TIMING")) {
						std::fprintf(stderr, "SSH Wayland SHM DMA-BUF import %ux%u\n", width, height);
					}
				}
				image->device = allocation.st_dev;
				image->inode = allocation.st_ino;
				image->start = start;
				image->writing = output;
				if (shm_images.size() == 16) { shm_images.erase(shm_images.begin()); }
				shm_images.push_back(std::move(image));
			}
			return shm_images.back().get();
		}
	};

	thread_local std::unordered_map<dev_t, std::unique_ptr<Device_Converter>> cached_converters;

	visual::Result<std::unique_ptr<Device_Converter>> make_converter(dev_t device) {
		auto next = std::make_unique<Device_Converter>(device);
		VISUAL_TRY(next->context.init(major(device), minor(device)));
		next->converter = std::make_unique<Video_Converter>(&next->context);
		VISUAL_TRY(next->converter->init());
		return next;
	}

	visual::Result<Device_Converter *> device_converter(const char *device) {
		struct stat node{};
		if (stat(device, &node) != 0 || !S_ISCHR(node.st_mode)) {
			return std::unexpected("conversion device is not a DRM character device");
		}
		auto &cached_converter = cached_converters[node.st_rdev];
		if (!cached_converter) {
			auto next = make_converter(node.st_rdev);
			if (!next) { return std::unexpected(next.error()); }
			cached_converter = std::move(*next);
		}
		return cached_converter.get();
	}

	encode::Chroma_Layout stream_chroma(std::uint32_t chroma) {
		return chroma == 444 ? encode::Chroma_Layout::YUV444 :
			   chroma == 422 ? encode::Chroma_Layout::YUV422 : encode::Chroma_Layout::YUV420;
	}

	encode::Rate_Control_Mode driver_rate(Video_Rate_Mode mode) {
		switch (mode) {
			case Video_Rate_Mode::Fixed_Qp: return encode::Rate_Control_Mode::CQP;
			case Video_Rate_Mode::Vbr: return encode::Rate_Control_Mode::VBR;
			case Video_Rate_Mode::Constant_Quality: return encode::Rate_Control_Mode::ICQ;
		}
		std::unreachable();
	}

	bool can_encode(const encode::Encode_Mode_Restrictions &mode, const hevc::Frame_Format &format,
					Video_Rate_Mode rate) {
		return mode.chroma_ == stream_chroma(format.chroma) && mode.bit_depth_ == format.depth &&
			   mode.screen_content_ == (format.screen_content != 0) &&
			   std::ranges::find(mode.implemented_source_formats_, VK_FORMAT_R32_UINT) !=
					   mode.implemented_source_formats_.end() &&
			   encode::contains_rate_control(mode.rate_control_, driver_rate(rate));
	}

	struct Va_Processor : visual::Non_Copyable {
		struct Rgb_Surface {
			dev_t device;
			ino_t inode;
			std::uint32_t width, height, format, offset, stride;
			std::uint64_t modifier;
			VASurfaceID surface;
		};
		VADisplay display;
		std::shared_ptr<void> lifetime;
		VAConfigID config = VA_INVALID_ID;
		VAContextID context = VA_INVALID_ID;
		std::vector<Rgb_Surface> rgb;
		explicit Va_Processor(const hevc::Va_Surface &source) : display(source.display), lifetime(source.lifetime) {}
		~Va_Processor() {
			if (context != VA_INVALID_ID) { (void) vaDestroyContext(display, context); }
			for (auto &entry : rgb) { (void) vaDestroySurfaces(display, &entry.surface, 1); }
			if (config != VA_INVALID_ID) { (void) vaDestroyConfig(display, config); }
		}

		[[nodiscard]] bool init(std::uint32_t width, std::uint32_t height) {
			return vaCreateConfig(display, VAProfileNone, VAEntrypointVideoProc, nullptr, 0, &config) == VA_STATUS_SUCCESS &&
				vaCreateContext(display, config, static_cast<int>(width), static_cast<int>(height), VA_PROGRESSIVE, nullptr, 0, &context) == VA_STATUS_SUCCESS;
		}

		[[nodiscard]] VASurfaceID import(const Visual_Plane &plane, std::uint32_t width, std::uint32_t height, std::uint32_t format) {
			struct stat allocation{};
			if (fstat(plane._fd, &allocation) != 0 || allocation.st_size <= 0 || allocation.st_size > UINT32_MAX) {
				return VA_INVALID_SURFACE;
			}
			auto found = std::ranges::find_if(rgb, [&](const auto &entry) {
				return entry.device == allocation.st_dev && entry.inode == allocation.st_ino &&
					entry.width == width && entry.height == height && entry.format == format &&
					entry.offset == plane._offset && entry.stride == plane._stride && entry.modifier == plane._modifier;
			});
			if (found != rgb.end()) {
				std::rotate(found, found + 1, rgb.end());
				return rgb.back().surface;
			}
			const auto va_format = [&] -> std::uint32_t {
				switch (format) {
					case DRM_FORMAT_XRGB8888: return VA_FOURCC_BGRX;
					case DRM_FORMAT_XBGR8888: return VA_FOURCC_RGBX;
					case DRM_FORMAT_XRGB2101010: return VA_FOURCC_X2R10G10B10;
					case DRM_FORMAT_XBGR2101010: return VA_FOURCC_X2B10G10R10;
					default: return 0;
				}
			}();
			if (!va_format) { return VA_INVALID_SURFACE; }
			const auto rt_format = format == DRM_FORMAT_XRGB8888 || format == DRM_FORMAT_XBGR8888
				? VA_RT_FORMAT_RGB32 : VA_RT_FORMAT_RGB32_10;

			VADRMPRIMESurfaceDescriptor desc{};
			desc.fourcc = va_format;
			desc.width = width;
			desc.height = height;
			desc.num_objects = 1;
			desc.num_layers = 1;
			desc.objects[0].fd = plane._fd;
			desc.objects[0].size = static_cast<std::uint32_t>(allocation.st_size);
			desc.objects[0].drm_format_modifier = plane._modifier;
			desc.layers[0].drm_format = format;
			desc.layers[0].num_planes = 1;
			desc.layers[0].offset[0] = plane._offset;
			desc.layers[0].pitch[0] = plane._stride;
			std::array attributes{
				VASurfaceAttrib{
					.type = VASurfaceAttribMemoryType, .flags = VA_SURFACE_ATTRIB_SETTABLE,
					.value = {.type = VAGenericValueTypeInteger, .value = {.i = VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2}},
				},
				VASurfaceAttrib{
					.type = VASurfaceAttribExternalBufferDescriptor, .flags = VA_SURFACE_ATTRIB_SETTABLE,
					.value = {.type = VAGenericValueTypePointer, .value = {.p = &desc}},
				},
			};
			VASurfaceID surface = VA_INVALID_SURFACE;
			if (vaCreateSurfaces(display, rt_format, width, height, &surface, 1, attributes.data(), attributes.size()) != VA_STATUS_SUCCESS) {
				return VA_INVALID_SURFACE;
			}
			if (rgb.size() == 8) {
				(void) vaDestroySurfaces(display, &rgb.front().surface, 1);
				rgb.erase(rgb.begin());
			}
			rgb.push_back({
				.device = allocation.st_dev, .inode = allocation.st_ino, .width = width, .height = height,
				.format = format, .offset = plane._offset, .stride = plane._stride, .modifier = plane._modifier, .surface = surface,
			});
			return surface;
		}

		[[nodiscard]] bool convert(VASurfaceID input, VASurfaceID output, std::uint32_t width, std::uint32_t height, bool to_yuv) const {
			const VARectangle visible{.x = 0, .y = 0, .width = static_cast<std::uint16_t>(width), .height = static_cast<std::uint16_t>(height)};
			VAProcColorProperties rgb_color{};
			rgb_color.color_range = VA_SOURCE_RANGE_FULL;
			rgb_color.colour_primaries = 1;
			rgb_color.transfer_characteristics = 13;
			auto yuv_color = rgb_color;
			yuv_color.matrix_coefficients = 1;
			yuv_color.chroma_sample_location = VA_CHROMA_SITING_HORIZONTAL_CENTER | VA_CHROMA_SITING_VERTICAL_CENTER;
			VAProcPipelineParameterBuffer parameters{};
			parameters.surface = input;
			parameters.surface_region = &visible;
			parameters.output_region = &visible;
			parameters.surface_color_standard = VAProcColorStandardExplicit;
			parameters.output_color_standard = VAProcColorStandardExplicit;
			parameters.input_color_properties = to_yuv ? rgb_color : yuv_color;
			parameters.output_color_properties = to_yuv ? yuv_color : rgb_color;
			VABufferID buffer = VA_INVALID_ID;
			if (vaCreateBuffer(display, context, VAProcPipelineParameterBufferType, sizeof(parameters), 1, &parameters, &buffer) != VA_STATUS_SUCCESS) {
				return false;
			}
			bool complete = false;
			if (vaBeginPicture(display, context, output) == VA_STATUS_SUCCESS) {
				const auto render = vaRenderPicture(display, context, &buffer, 1);
				const auto end = vaEndPicture(display, context);
				complete = render == VA_STATUS_SUCCESS && end == VA_STATUS_SUCCESS &&
					vaSyncSurface2(display, output, 2'000'000'000ULL) == VA_STATUS_SUCCESS;
			}
			(void) vaDestroyBuffer(display, buffer);
			return complete;
		}
	};

	struct Encoder {
		std::string device;
		hevc::Frame_Layout layout;
		bool scc;
		Video_Rate _rate;
		std::uint8_t minimum_qp{};
		bool predictive{};
		std::optional<std::uint32_t> previous_poc;
		std::unique_ptr<hevc::H265_Encode_Context> context;
		hevc::encoder::Va_Encode_Context *va_context{};
		bool prepared_source{};
		std::unique_ptr<Va_Processor> processor;
		bool tried_processor{};


		[[nodiscard]] visual::Status init_vaapi(bool alpha) {
			const auto snapshot = hevc::encoder::query_encode_plan_restrictions(device);
			if (!snapshot) { return std::unexpected(snapshot.error().message_); }
			std::string error = "requested H.265 profile, chroma or depth is unavailable on the encode GPU";
			for (const auto &mode : snapshot->modes_) {
				const auto &capability = mode.public_;
				if (!can_encode(capability, {.chroma = layout.chroma, .depth = layout.depth, .screen_content = scc},
								_rate._mode) || (predictive && capability.max_reference_l0_ == 0)) {
					continue;
				}
				// Keep VA-API mask QP above the very low values that
				// fail or reconstruct incorrectly on Intel iHD.
				auto rate_control = hevc::H265_Rate_Control{};
				if (predictive && mode.max_quality_level_ >= 7) {
					// The driver's speed preset is independent of the ICQ factor.
					rate_control.driver_quality_level_ = 7;
				}
				if (alpha) {
					rate_control.initial_qp_ = VAAPI_ALPHA_QP;
					rate_control.minimum_qp_ = VAAPI_ALPHA_QP;
				}
				if (_rate._mode == Video_Rate_Mode::Vbr) {
					rate_control.target_bitrate_ = _rate._target_bitrate;
					rate_control.maximum_bitrate_ = _rate._target_bitrate * 2;
					rate_control.target_percentage_ = 50;
				}
				if (_rate._mode == Video_Rate_Mode::Constant_Quality) {
					rate_control.initial_qp_ =
						std::max(static_cast<std::uint8_t>(_rate._quality), rate_control.minimum_qp_);
				}
				const hevc::H265_Encode_Plan plan{
					.stream_ = {.restrictions_identity_ = snapshot->public_.identity_, .mode_ = capability.id_,
								.width_ = layout.width, .height_ = layout.height, .framerate_num_ = _rate._frame_rate,
								.framerate_den_ = 1, .source_format_ = VK_FORMAT_R32_UINT,
								.rate_control_ = driver_rate(_rate._mode)},
					.output_color_ = {.bit_depth_ = static_cast<std::uint8_t>(layout.depth), .monochrome_ = false,
									  .subsampling_x_ = layout.chroma != 444, .subsampling_y_ = layout.chroma == 420,
									  .range_ = Color_Range::FULL, .matrix_ = Matrix_Coefficients::BT709,
									  .primaries_ = Color_Primaries::BT709, .transfer_ = Transfer_Function::SRGB},
					.rate_control_ = rate_control,
					.intra_period_ = predictive ? GROUP_LENGTH : 1,
					.intra_idr_period_ = predictive ? GROUP_LENGTH : 1, .ip_period_ = 1,
					.level_idc_ = _rate._mode == Video_Rate_Mode::Fixed_Qp ? std::uint8_t{153} : std::uint8_t{156},
					.high_tier_ = _rate._mode == Video_Rate_Mode::Vbr,
					.sample_adaptive_offset_ = !alpha && mode.va_sao_supported_, .temporal_mvp_ = false,
				};
				auto created = hevc::encoder::create_encode_context(plan, *snapshot, device, predictive ? 1U : 0U);
				if (created) {
					va_context = created->get();
					context = std::move(*created);
					minimum_qp = alpha ? VAAPI_ALPHA_QP : 0;
					return {};
				}
				error = created.error().message_;
			}
			return std::unexpected(error);
		}
	};

	// A connection can interleave a window and its subsurfaces. Retain the
	// two most recent layouts independently for color and alpha.
	using Encoder_Cache = std::array<std::unique_ptr<Encoder>, 2>;
	struct Video_Stream {
		std::array<Encoder_Cache, 2> encoders;
		std::unique_ptr<hevc::Decoder_State, decltype(&hevc::destroy_decoder_state)> decoders{
			hevc::create_decoder_state(), hevc::destroy_decoder_state,
		};
		std::unique_ptr<Va_Processor> presentation{};
		std::uint64_t presentation_owner{};
		std::optional<hevc::Va_Surface> last_alpha{};
		std::unique_ptr<Buffer> alpha_history{};
		dev_t alpha_device{};

	};
		thread_local Video_Stream capture_stream;
	Video_Stream &stream_state(void *stream) {
		return *static_cast<Video_Stream *>(stream);
	}

	visual::Status prepare_encoder(const char *device, hevc::Frame_Layout layout, bool scc, bool alpha, bool predictive,
								   const Video_Rate *requested_rate, Video_Stream &stream) {
		const auto rate = requested_rate ? *requested_rate : Video_Rate{
			._mode = Video_Rate_Mode::Fixed_Qp, ._frame_rate = 60, ._target_bitrate = 0, ._quality = 0,
			._automatic_bitrate = 0,
		};
		auto &cache = stream.encoders[alpha];
		const auto matches = [&](const std::unique_ptr<Encoder> &encoder) {
			return encoder && encoder->device == device && encoder->layout == layout && encoder->scc == scc &&
				   encoder->predictive == predictive && encoder->_rate == rate;
		};
		if (matches(cache.front())) { return {}; }
		if (matches(cache.back())) {
			std::swap(cache.front(), cache.back());
			return {};
		}
		auto encoder = std::make_unique<Encoder>(Encoder{
			.device = device, .layout = layout, .scc = scc, ._rate = rate,
			.predictive = predictive, .previous_poc = std::nullopt, .context = {}, .processor = {},
		});
		VISUAL_TRY(encoder->init_vaapi(alpha));
		if (cache.front()) {
			cache.back() = std::move(cache.front());
		}
		cache.front() = std::move(encoder);
		return {};
	}

	hevc::Frame_Layout layout_for(const Video_Options &options) {
		return {.width = options.coded_width, .height = options.coded_height,
				.depth = options.depth, .chroma = options.chroma};
	}

	hevc::Frame_Layout alpha_layout_for(hevc::Frame_Layout color) {
		return {.width = color.width, .height = color.height, .depth = 8, .chroma = 420};
	}

	[[nodiscard]] visual::Status convert_va_frames(Device_Converter &gpu, Video_Options config,
			const hevc::Va_Surface &color, const hevc::Va_Surface *alpha, const Imported *target, const Buffer *buffer) {
		const auto layout = layout_for(config);
		const auto alpha_layout = alpha_layout_for(layout);
		VISUAL_TRY(gpu.va_color.prepare(gpu.context, color, layout, false));
		if (alpha) { VISUAL_TRY(gpu.va_alpha.prepare(gpu.context, *alpha, alpha_layout, true)); }
		if (vaSyncSurface(color.display, color.surface) != VA_STATUS_SUCCESS ||
			(alpha && vaSyncSurface(alpha->display, alpha->surface) != VA_STATUS_SUCCESS)) {
			return std::unexpected("VA-API decode did not complete successfully");
		}
		auto &commands = gpu.converter->commands;
		VISUAL_TRY(commands.begin_commands());
		gpu.va_color.read(commands, layout, false);
		config.mode = DECODE;
		if (target) { config.mode |= RGB_OUTPUT; }
		if (alpha) {
			gpu.va_alpha.read(commands, alpha_layout, true);
			config.mode |= WITH_ALPHA;
		}
		return gpu.converter->convert(*gpu.va_color.samples, config, default_visual_color(), {
			._commands = &commands,
			._planar_alpha = alpha ? gpu.va_alpha.samples.get() : nullptr,
			._planar_output = target,
			._buffer_output = buffer,
		});
	}

}

extern "C" void *ssh_video_create_stream() { return new Video_Stream; }
extern "C" void *ssh_video_capture_stream() { return &capture_stream; }
extern "C" void ssh_video_drop_stream(void *stream) { delete static_cast<Video_Stream *>(stream); }

extern "C" void *ssh_video_create_converter(const char *device) {
	struct stat node{};
	if (stat(device, &node) != 0 || !S_ISCHR(node.st_mode)) { return nullptr; }
	auto created = make_converter(node.st_rdev);
	return created ? created->release() : nullptr;
}

extern "C" void ssh_video_take_converter(void *pointer) {
	auto next = std::unique_ptr<Device_Converter>(static_cast<Device_Converter *>(pointer));
	if (next) { cached_converters.try_emplace(next->device, std::move(next)); }
}

extern "C" void ssh_video_drop_converter(void *pointer) {
	delete static_cast<Device_Converter *>(pointer);
}

extern "C" std::uint32_t ssh_video_encode_formats(const char *device, const hevc::Frame_Format *formats,
												 std::uint32_t count, Video_Rate_Mode rate) {
	const auto snapshot = hevc::encoder::query_encode_plan_restrictions(device);
	if (count > 16) { return 0; }
	std::uint32_t supported = 0;
	if (!snapshot) { return 0; }
	for (std::uint32_t i = 0; i < count; ++i) {
		for (const auto &mode : snapshot->modes_) {
			if (!can_encode(mode.public_, formats[i], rate)) { continue; }
			supported |= 1U << i;
			if (mode.public_.max_reference_l0_ != 0) { supported |= 1U << (i + 16); }
		}
	}
	return supported;
}

extern "C" std::uint32_t ssh_video_decode_formats(const char *device, const hevc::Frame_Format *formats,
												  std::uint32_t count) {
	if (count > 16) { return 0; }
	const auto requested = std::span(formats, count);
	const auto va = hevc::decode_formats(device, requested);
	return (va << 16) | va;
}

extern "C" int ssh_video_prepare_encoder(const char *device, const Video_Options *options, bool scc,
										 const Video_Rate *rate, char *error, void *stream) {
	return ffi_status(prepare_encoder(device, layout_for(*options), scc, (options->mode & ALPHA_PICTURE) != 0,
									  (options->mode & PREDICTIVE_PLAN) != 0, rate, stream_state(stream)), error);
}

extern "C" std::uint32_t ssh_video_capture_modifiers(const char *device, std::uint32_t format,
													std::uint64_t *modifiers, std::uint32_t capacity) {
	const auto selected = device_converter(device);
	const auto vk_format = visual_capture_format(format);
	if (!selected || vk_format == VK_FORMAT_UNDEFINED) { return 0; }
	const auto physical = (*selected)->context._physical;
	VkDrmFormatModifierPropertiesListEXT list{
		.sType = VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT, .pNext = nullptr,
		.drmFormatModifierCount = 0, .pDrmFormatModifierProperties = nullptr,
	};
	VkFormatProperties2 props{.sType = VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_2, .pNext = &list, .formatProperties = {}};
	vkGetPhysicalDeviceFormatProperties2(physical, vk_format, &props);
	std::vector<VkDrmFormatModifierPropertiesEXT> properties(list.drmFormatModifierCount);
	list.pDrmFormatModifierProperties = properties.data();
	vkGetPhysicalDeviceFormatProperties2(physical, vk_format, &props);
	std::uint32_t count = 0;
	for (const auto &property : properties) {
		const auto modifier = property.drmFormatModifier;
		if (count == capacity) { break; }
		if (property.drmFormatModifierPlaneCount == 1 &&
			visual_can_import_transfer(physical, vk_format, modifier)) {
			modifiers[count++] = modifier;
		}
	}
	return count;
}

extern "C" int ssh_video_convert_snapshot(void *pointer, const Video_Options *options, const Visual_Color_Profile *profile,
										void *stream, char *error) {
	const auto convert = [&]() -> visual::Status {
		auto &snapshot = *static_cast<Cached_Snapshot_Native *>(pointer);
		if (!snapshot._raw._captured || snapshot._width != options->width || snapshot._height != options->height ||
			snapshot._format != options->format) {
			return std::unexpected("snapshot_unavailable: video source changed");
		}
		auto &encoder = stream_state(stream).encoders[false].front();
		if (!encoder || !encoder->va_context) { return std::unexpected("capture requires a prepared GPU encoder"); }
		auto input = encoder->va_context->prepare_input();
		if (!input) { return std::unexpected(input.error().message_); }
		auto &context = *snapshot._raw._c;
		const auto converter = snapshot_converter(context);
		if (!converter) { return std::unexpected(converter.error()); }
		auto &gpu = **converter;
		auto config = *options;
		VISUAL_TRY(gpu.input.prepare(context, *input, layout_for(config), false, VK_IMAGE_USAGE_TRANSFER_DST_BIT));
		VISUAL_TRY(gpu.converter.convert(snapshot._raw._pixels, config, *profile,
			{._planar_output = &gpu.input.current()}));
		encoder->prepared_source = true;
		return {};
	};
	return ffi_status(convert(), error);
}

extern "C" int ssh_video_copy_snapshot(void *pointer, const Video_Options *options,
		const Visual_Plane *destination, std::uint32_t destination_width, char *error) {
	const auto copy = [&]() -> visual::Status {
		auto &snapshot = *static_cast<Cached_Snapshot_Native *>(pointer);
		if (!snapshot._raw._captured || snapshot._width != options->width || snapshot._height != options->height ||
			snapshot._format != options->format) {
			return std::unexpected("snapshot_unavailable: video source changed");
		}
		auto &commands = snapshot._raw;
		auto &context = *commands._c;
		const auto converter = snapshot_converter(context);
		if (!converter) { return std::unexpected(converter.error()); }
		const auto target = import_output(context, (*converter)->staging, *options, *destination, destination_width);
		if (!target) { return std::unexpected(target.error()); }
		VISUAL_TRY(commands.begin_commands());
		commands.barrier(VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT,
			VK_ACCESS_TRANSFER_WRITE_BIT, VK_ACCESS_TRANSFER_READ_BIT);
		const auto region = packed_region(options->width, options->height);
		upload_image(commands, commands._pixels, **target, std::span(&region, 1));
		return commands.submit_wait(false);
	};
	return ffi_status(copy(), error);
}

extern "C" int ssh_video_convert_buffer(const Video_Options *options, const Video_Shm_Source *shm,
		const Visual_Plane *plane, std::uint32_t plane_width, const Visual_Color_Profile *color, bool implicit_sync,
		const Video_Encode_Target *target, char *error) {
	const auto convert = [&]() -> visual::Result<int> {
		if ((!shm && !plane) || (shm && plane)) { return std::unexpected("expected exactly one GPU source"); }
		const auto selected = device_converter(target->_device);
		if (!selected) { return std::unexpected(selected.error()); }
		auto &gpu = **selected;
		auto &stream = stream_state(target->_stream);
		auto config = *options;
		auto &color_encoder = stream.encoders[false].front();
		auto &alpha_encoder = stream.encoders[true].front();
		if (!color_encoder || !color_encoder->va_context || (target->_with_alpha && (!alpha_encoder || !alpha_encoder->va_context))) {
			return std::unexpected("GPU-only capture requires prepared VA-API encoders");
		}
		const auto profile = color ? *color : default_visual_color(2);
		if ((config.format != DRM_FORMAT_XBGR16161616F && config.format != DRM_FORMAT_ABGR16161616F) &&
			(profile._transfer == 9 || profile._transfer == 14) &&
			same_color(profile, default_visual_color(profile._alpha_mode)) && config.tone_map == 0 &&
			config.exposure == 1.0f && config.background == 0 && (config.mode & WITH_ALPHA) == 0) {
			config.mode |= IDENTITY_SDR;
		}
		if (plane && (config.mode & IDENTITY_SDR) != 0 && !target->_with_alpha &&
			!((config.width | config.height) & 1U) && (config.format == DRM_FORMAT_XRGB8888 ||
				config.format == DRM_FORMAT_XBGR8888 || config.format == DRM_FORMAT_XRGB2101010 ||
				config.format == DRM_FORMAT_XBGR2101010)) {
			auto output = color_encoder->va_context->prepare_input();
			if (!output) { return std::unexpected(output.error().message_); }
			auto &processor = color_encoder->processor;
			if (!color_encoder->tried_processor) {
				color_encoder->tried_processor = true;
				processor = std::make_unique<Va_Processor>(*output);
				if (!processor->init(config.coded_width, config.coded_height)) { processor.reset(); }
			}
			if (processor) {
				const auto input = processor->import(*plane, plane_width, config.height, config.format);
				if (input != VA_INVALID_SURFACE) {
					if (implicit_sync) {
						pollfd fence{.fd = plane->_fd, .events = POLLIN, .revents = 0};
						int result;
						do { result = poll(&fence, 1, 2000); } while (result < 0 && errno == EINTR);
						if (result != 1 || !(fence.revents & POLLIN)) { return std::unexpected("DMA-BUF producer did not complete"); }
					}
					if (processor->convert(input, output->surface, config.width, config.height, true)) {
						color_encoder->prepared_source = true;
						if (std::getenv("WAYLAND_MCP_TRACE_TIMING")) { std::fprintf(stderr, "SSH encode VA VPP %ux%u\n", config.width, config.height); }
						return 0;
					}
					// Clear the VPP context before the GPU compute path overwrites this slot.
					processor.reset();
					if (vaSyncSurface2(output->display, output->surface, 2'000'000'000ULL) != VA_STATUS_SUCCESS) {
						return std::unexpected("VA video processing did not release the encoder surface");
					}
				}
			}
		}
		const auto write = [&](const Buffer &input, Program *submission, bool producer) -> visual::Result<int> {
			const auto prepare = [&](Encoder &encoder, Video_Options output, Program *commands, bool wait,
				const Buffer *history = nullptr) -> visual::Status {
				auto surface = encoder.va_context->prepare_input();
				if (!surface) { return std::unexpected(surface.error().message_); }
				VISUAL_TRY(gpu.va_input.prepare(gpu.context, *surface, layout_for(output), false, VK_IMAGE_USAGE_TRANSFER_DST_BIT));
				VISUAL_TRY(gpu.converter->convert(input, output, profile, {
					._commands = commands, ._wait_for_producer = wait, ._planar_output = &gpu.va_input.current(), ._alpha_history = history,
				}));
				encoder.prepared_source = true;
				return {};
			};
			if (!target->_with_alpha) {
				VISUAL_TRY(prepare(*color_encoder, config, submission, producer));
				return 0;
			}
			auto mask = config;
			const bool collect_alpha = config.depth == 8 && config.chroma == 420;
			const auto luma_size = VkDeviceSize{config.coded_width} * config.coded_height;
			const auto history_size = collect_alpha ? luma_size * 3 / 2 : luma_size;
			if (stream.alpha_device != gpu.device) {
				stream.alpha_history.reset();
				stream.alpha_device = gpu.device;
			}
			const bool compare = target->_compare_alpha && stream.alpha_history && stream.alpha_history->_size >= history_size;
			VISUAL_TRY(reserve_buffer(stream.alpha_history, &gpu.context, history_size,
				VK_BUFFER_USAGE_STORAGE_BUFFER_BIT | VK_BUFFER_USAGE_TRANSFER_SRC_BIT));
			mask.mode = ALPHA_ONLY | (compare ? COMPARE_ALPHA : 0);
			mask.depth = 8;
			mask.chroma = 420;
			if (collect_alpha) {
				config.mode |= COLLECT_ALPHA | (compare ? COMPARE_ALPHA : 0);
				VISUAL_TRY(prepare(*color_encoder, config, submission, producer, stream.alpha_history.get()));
			} else {
				VISUAL_TRY(prepare(*color_encoder, config, submission, producer));
				VISUAL_TRY(prepare(*alpha_encoder, mask, nullptr, false, stream.alpha_history.get()));
			}
			const auto alpha = *static_cast<const std::uint32_t *>(gpu.converter->alpha_status._mapped);
			const auto action = (alpha & 1u) == 0u ? 0 : compare && (alpha & 2u) == 0u ? 2 : 1;
			if (collect_alpha && action == 1) {
				auto surface = alpha_encoder->va_context->prepare_input();
				if (!surface) { return std::unexpected(surface.error().message_); }
				const auto layout = layout_for(mask);
				VISUAL_TRY(gpu.va_input.prepare(gpu.context, *surface, layout, false, VK_IMAGE_USAGE_TRANSFER_DST_BIT));
				auto &commands = gpu.converter->commands;
				VISUAL_TRY(commands.begin_commands());
				commands.barrier(VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT,
					VK_ACCESS_SHADER_WRITE_BIT, VK_ACCESS_TRANSFER_READ_BIT);
				upload_image(commands, *stream.alpha_history, gpu.va_input.current(), video_regions(layout));
				VISUAL_TRY(commands.submit_wait(false));
				alpha_encoder->prepared_source = true;
			}
			if (action != 1) { alpha_encoder->prepared_source = false; }
			return action;
		};
		if (shm) {
			config.source_stride = shm->_stride;
			auto input = gpu.import_shm(*shm, config.width, config.height, false, config.source_offset);
			if (!input) { return std::unexpected(input.error()); }
			return write((*input)->buffer, nullptr, false);
		}
		struct stat allocation{};
		if (fstat(plane->_fd, &allocation) != 0) {
			return std::unexpected("could not identify the video DMA-BUF");
		}
		auto snapshot = gpu.snapshot(config);
		if (!snapshot) { return std::unexpected(snapshot.error()); }
		auto &commands = (*snapshot)->_raw;
		const auto image = commands.import_buffer(
			allocation.st_ino, plane_width, config.height, config.format, plane, 1, true);
		if (!image) { return std::unexpected(image.error()); }
		const auto recorded = commands.record_capture(*image, 0, implicit_sync);
		if (!recorded) { return std::unexpected(recorded.error()); }
		return write(commands._pixels, &commands, *recorded);
	};
	return ffi_result(convert(), error);
}

extern "C" int ssh_video_copy_dma_buf(const char *device, const Video_Options *options, const Visual_Plane *source,
		const Visual_Plane *destination, std::uint32_t destination_width, bool implicit_sync, char *error) {
	const auto copy = [&]() -> visual::Status {
		const auto selected = device_converter(device);
		if (!selected) { return std::unexpected(selected.error()); }
		auto &gpu = **selected;
		const auto target = import_output(gpu.context, gpu.display_images, *options, *destination, destination_width);
		if (!target) { return std::unexpected(target.error()); }
		struct stat allocation{};
		if (fstat(source->_fd, &allocation) != 0) { return std::unexpected("could not identify the source DMA-BUF"); }
		const auto snapshot = gpu.snapshot(*options);
		if (!snapshot) { return std::unexpected(snapshot.error()); }
		auto &commands = (*snapshot)->_raw;
		const auto input = commands.import_buffer(allocation.st_ino, options->width, options->height,
			options->format, source, 1, true);
		if (!input) { return std::unexpected(input.error()); }
		if (implicit_sync) { VISUAL_TRY(commands.import_producer_fence(**input)); }
		VISUAL_TRY(commands.begin_commands());
		const auto acquire = [&](VkImage image, VkAccessFlags access, VkImageLayout layout) {
			return VkImageMemoryBarrier{
				.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, .pNext = nullptr,
				.srcAccessMask = 0, .dstAccessMask = access,
				.oldLayout = VK_IMAGE_LAYOUT_GENERAL, .newLayout = layout,
				.srcQueueFamilyIndex = VK_QUEUE_FAMILY_FOREIGN_EXT, .dstQueueFamilyIndex = gpu.context._family,
				.image = image, .subresourceRange = {
					.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT, .baseMipLevel = 0, .levelCount = 1, .baseArrayLayer = 0, .layerCount = 1,
				},
			};
		};
		std::array barriers{
			acquire((*input)->_image, VK_ACCESS_TRANSFER_READ_BIT, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL),
			acquire((*target)->_image, VK_ACCESS_TRANSFER_WRITE_BIT, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL),
		};
		vkCmdPipelineBarrier(commands._cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT,
			0, 0, nullptr, 0, nullptr, barriers.size(), barriers.data());
		constexpr VkImageSubresourceLayers subresource{
			.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT, .mipLevel = 0, .baseArrayLayer = 0, .layerCount = 1,
		};
		const VkImageCopy region{
			.srcSubresource = subresource, .srcOffset = {}, .dstSubresource = subresource, .dstOffset = {},
			.extent = {.width = options->width, .height = options->height, .depth = 1},
		};
		vkCmdCopyImage(commands._cmd, (*input)->_image, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
			(*target)->_image, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &region);
		for (auto &barrier : barriers) {
			std::swap(barrier.srcAccessMask, barrier.dstAccessMask);
			std::swap(barrier.oldLayout, barrier.newLayout);
			std::swap(barrier.srcQueueFamilyIndex, barrier.dstQueueFamilyIndex);
		}
		vkCmdPipelineBarrier(commands._cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT,
			0, 0, nullptr, 0, nullptr, barriers.size(), barriers.data());
		return commands.submit_wait(implicit_sync);
	};
	return ffi_status(copy(), error);
}

extern "C" void *ssh_video_encode(const char *device, const Video_Options *options, bool scc, std::uint8_t qp,
								 const Video_Rate *rate, std::uint32_t stream_poc, char *error, void *stream) {
	const auto layout = layout_for(*options);
	const bool alpha = (options->mode & ALPHA_PICTURE) != 0;
	auto &state = stream_state(stream);
	auto &cached_encoder = state.encoders[alpha].front();
	const bool predictive = stream_poc != INDEPENDENT;
	if (predictive && (stream_poc >= GROUP_LENGTH || !rate)) {
		error_copy(error, "invalid predictive H.265 encoding parameters");
		return nullptr;
	}
	const auto status = prepare_encoder(device, layout, scc, alpha, predictive, rate, state);
	if (!status) {
		error_copy(error, status.error());
		return nullptr;
	}
	const bool predicted = predictive && stream_poc != 0;
	const auto poc = predictive ? stream_poc : 0;
	if (predicted && cached_encoder->previous_poc != poc - 1) {
		error_copy(error, "missing previous H.265 encoder reference");
		return nullptr;
	}
	const hevc::Frame_Id predecessor{.value_ = predicted ? poc - 1 : 0};
	const hevc::H265_Encode_Picture picture{
		.frame_ = {.value_ = poc}, .type_ = predicted ? hevc::H265_Slice_Type::P : hevc::H265_Slice_Type::I,
		.picture_order_count_ = static_cast<std::int32_t>(poc), .idr_ = !predicted, .reference_ = true,
		.references_l0_ = predicted ? std::vector{predecessor} : std::vector<hevc::Frame_Id>{},
		.references_l1_ = {},
		.cqp_ = rate ? std::nullopt : std::optional<std::uint8_t>{std::max(qp, cached_encoder->minimum_qp)},
		.read_reconstruction_ = false,
	};
	if (!std::exchange(cached_encoder->prepared_source, false)) {
		error_copy(error, "missing corresponding prepared VA picture");
		return nullptr;
	}
	auto encoded = cached_encoder->va_context->encode_prepared(picture);
	if (!encoded) {
		error_copy(error, encoded.error().message_);
		cached_encoder.reset();
		return nullptr;
	}
	if (predicted) {
		auto released = cached_encoder->context->release_reference(predecessor);
		if (!released) {
			error_copy(error, released.error().message_);
			cached_encoder.reset();
			return nullptr;
		}
	}
	cached_encoder->previous_poc = predictive ? std::optional{poc} : std::nullopt;
	return new Video_Packet(std::move(encoded->annex_b_));
}

extern "C" const std::uint8_t *ssh_video_packet_data(const void *pointer, std::uint32_t *size) {
	const auto &packet = *static_cast<const Video_Packet *>(pointer);
	*size = static_cast<std::uint32_t>(packet.size());
	return packet.data();
}

extern "C" void ssh_video_packet_destroy(void *pointer) { delete static_cast<Video_Packet *>(pointer); }

extern "C" int ssh_video_decode(const char *device, const Video_Options *options, void *stream,
		const Video_Bitstream *picture, const Video_Display_Target *output, char *error) {
	const auto decode = [&]() -> visual::Status {
		const auto layout = layout_for(*options);
		const auto alpha_layout = alpha_layout_for(layout);
		const auto destination = output->_dma;
		const auto display_shm = output->_shm;
		if ((!destination && !display_shm) || (destination && display_shm)) {
			return std::unexpected("expected exactly one GPU presentation destination");
		}
		auto &state = stream_state(stream);
		const auto sequence = [](std::uint32_t poc) {
			return poc == INDEPENDENT ? std::nullopt : std::optional<std::uint16_t>{static_cast<std::uint16_t>(poc)};
		};
		if ((picture->_poc != INDEPENDENT && picture->_poc >= GROUP_LENGTH) ||
			(picture->_alpha_poc != INDEPENDENT && picture->_alpha_poc >= GROUP_LENGTH)) {
			return std::unexpected("invalid predictive H.265 decoding parameters");
		}
		auto color = hevc::decode_picture(device, layout, picture->_scc, std::span(picture->_color, picture->_color_size),
			false, true, sequence(picture->_poc), state.decoders.get(), true);
		if (!color) { return std::unexpected(color.error().message_); }
		const auto *va_color = std::get_if<hevc::Va_Surface>(&*color);
		if (!va_color) { return std::unexpected("decode produced no GPU surface"); }
		std::optional<hevc::Va_Surface> mask;
		if (picture->_reuse_alpha) {
			if (picture->_alpha_size != 0 || !state.last_alpha) { return std::unexpected("no decoded alpha surface to reuse"); }
			mask = state.last_alpha;
		} else if (picture->_alpha_size != 0) {
			auto decoded = hevc::decode_picture(device, alpha_layout, false, std::span(picture->_alpha, picture->_alpha_size),
				true, true, sequence(picture->_alpha_poc), state.decoders.get(), true);
			if (!decoded) { return std::unexpected("decode HEVC alpha: " + decoded.error().message_); }
			const auto *surface = std::get_if<hevc::Va_Surface>(&*decoded);
			if (!surface) { return std::unexpected("decode produced no GPU alpha surface"); }
			mask = *surface;
		}
		if (destination && !mask && !((options->width | options->height) & 1U)) {
			if (state.presentation_owner != va_color->owner) {
				state.presentation_owner = va_color->owner;
				state.presentation = std::make_unique<Va_Processor>(*va_color);
				if (!state.presentation->init(layout.width, layout.height)) { state.presentation.reset(); }
			}
			if (state.presentation) {
				const auto target = state.presentation->import(*destination, options->width, options->height, options->format);
				if (target != VA_INVALID_SURFACE) {
					if (vaSyncSurface2(va_color->display, va_color->surface, 2'000'000'000ULL) != VA_STATUS_SUCCESS) {
						return std::unexpected("VA-API decode did not complete successfully");
					}
					if (state.presentation->convert(va_color->surface, target, options->width, options->height, false)) {
						if (std::getenv("WAYLAND_MCP_TRACE_TIMING")) { std::fprintf(stderr, "SSH display VA VPP %ux%u\n", options->width, options->height); }
						state.last_alpha.reset();
						return {};
					}
					const auto status = vaSyncSurface2(va_color->display, target, 2'000'000'000ULL);
					state.presentation.reset();
					if (status != VA_STATUS_SUCCESS) { return std::unexpected("VA video processing did not release the display surface"); }
				}
			}
		}
		const auto selected = device_converter(device);
		if (!selected) { return std::unexpected(selected.error()); }
		auto &gpu = **selected;
		auto config = *options;
		Imported *target = nullptr;
		if (destination) {
			const auto imported = import_output(gpu.context, gpu.display_images, config, *destination, config.width);
			if (!imported) { return std::unexpected(imported.error()); }
			target = *imported;
		}
		const Buffer *host_output = nullptr;
		if (display_shm) {
			auto image = gpu.import_shm(*display_shm, config.width, config.height, true, config.output_offset);
			if (!image) { return std::unexpected(image.error()); }
			config.output_offset /= sizeof(std::uint32_t);
			host_output = &(*image)->buffer;
		}
		VISUAL_TRY(convert_va_frames(gpu, config, *va_color, mask ? &*mask : nullptr, target, host_output));
		state.last_alpha = std::move(mask);
		return {};
	};
	return ffi_status(decode(), error);
}

extern "C" void *ssh_video_decode_preview(const char *device, const Video_Options *options, bool scc,
	const std::uint8_t *data, std::uint32_t size, std::uint32_t width, std::uint32_t height, char *error) {
	const auto preview = [&]() -> visual::Result<std::vector<std::uint8_t>> {
		const auto layout = layout_for(*options);
		auto color = hevc::decode_picture(device, layout, scc, std::span(data, size), false, true,
			std::nullopt, capture_stream.decoders.get(), true);
		if (!color) { return std::unexpected(color.error().message_); }
		const auto *va_color = std::get_if<hevc::Va_Surface>(&*color);
		if (!va_color) { return std::unexpected("capture preview requires a GPU decoded surface"); }
		const auto selected = device_converter(device);
		if (!selected) { return std::unexpected(selected.error()); }
		auto &gpu = **selected;
		auto config = *options;
		config.source_stride = config.width;
		config.source_offset = config.height;
		config.width = width;
		config.height = height;
		config.format = 0; // pack RGB in increasing byte order for the PNG encoder
		VISUAL_TRY(reserve_buffer(gpu.converter->preview_pixels, &gpu.context,
			VkDeviceSize{width} * height * 4, VK_BUFFER_USAGE_STORAGE_BUFFER_BIT));
		VISUAL_TRY(convert_va_frames(gpu, config, *va_color, nullptr, nullptr, gpu.converter->preview_pixels.get()));
		return gpu.converter->deflate_preview(width, height);
	};
	auto result = preview();
	if (!result) {
		error_copy(error, result.error());
		return nullptr;
	}
	return new Video_Packet(std::move(*result));
}
