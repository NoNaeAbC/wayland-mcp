#include "h265/vulkan_video.hpp"
#include "h265/vulkan_parameter_sets.hpp"
#include "vulkan_instance.h"

#include <algorithm>
#include <array>
#include <cstring>
#include <memory>
#include <mutex>
#include <string>
#include <string_view>
#include <utility>
#include <vector>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <vulkan/vulkan_core.h>

namespace video_editor::h265::vulkan_video {
	namespace {
		using Status = Result<void>;
		using enum Media_Error_Code;
		constexpr VkDeviceSize MAX_UNIT = VkDeviceSize{16} * 1024 * 1024;
		constexpr VkVideoEncodeFeedbackFlagsKHR FEEDBACK =
			VK_VIDEO_ENCODE_FEEDBACK_BITSTREAM_BUFFER_OFFSET_BIT_KHR |
			VK_VIDEO_ENCODE_FEEDBACK_BITSTREAM_BYTES_WRITTEN_BIT_KHR;

		std::unexpected<Media_Error> fail(std::string message) {
			return std::unexpected(Media_Error{
				.code_ = VULKAN_INTEROP_FAILED, .byte_offset_ = 0, .bit_offset_ = 0, .message_ = std::move(message),
			});
		}

		Status checked(VkResult result, std::string_view operation) {
			if (result == VK_ERROR_INVALID_VIDEO_STD_PARAMETERS_KHR) {
				return fail(std::string(operation) + " failed with VK_ERROR_INVALID_VIDEO_STD_PARAMETERS_KHR");
			}
			if (result != VK_SUCCESS) {
				return fail(std::string(operation) + " failed with Vulkan status " + std::to_string(result));
			}
			return {};
		}

#define VIDEO_TRY(expression) do { if (auto status = (expression); !status) { return std::unexpected(std::move(status.error())); } } while (false)
#define VIDEO_VK(expression) VIDEO_TRY(checked((expression), #expression))

		template<class T> T init_struct(VkStructureType type) {
			T value{};
			value.sType = type;
			return value;
		}

		struct Profile {
			VkVideoEncodeH265ProfileInfoKHR _encode =
				init_struct<VkVideoEncodeH265ProfileInfoKHR>(VK_STRUCTURE_TYPE_VIDEO_ENCODE_H265_PROFILE_INFO_KHR);
			VkVideoDecodeH265ProfileInfoKHR _decode =
				init_struct<VkVideoDecodeH265ProfileInfoKHR>(VK_STRUCTURE_TYPE_VIDEO_DECODE_H265_PROFILE_INFO_KHR);
			VkVideoProfileInfoKHR _info = init_struct<VkVideoProfileInfoKHR>(VK_STRUCTURE_TYPE_VIDEO_PROFILE_INFO_KHR);
			VkVideoProfileListInfoKHR _list = init_struct<VkVideoProfileListInfoKHR>(VK_STRUCTURE_TYPE_VIDEO_PROFILE_LIST_INFO_KHR);
			VkFormat _format{};
			StdVideoH265ProfileIdc _idc = STD_VIDEO_H265_PROFILE_IDC_INVALID;

			Profile(Frame_Layout layout, bool encode) {
				const bool wide = layout.chroma == 444;
				const bool ten = layout.depth == 10;
				if (wide) {
					_format = ten ? VK_FORMAT_G10X6_B10X6R10X6_2PLANE_444_UNORM_3PACK16 : VK_FORMAT_G8_B8R8_2PLANE_444_UNORM;
					_idc = STD_VIDEO_H265_PROFILE_IDC_FORMAT_RANGE_EXTENSIONS;
				} else {
					_format = ten ? VK_FORMAT_G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16 : VK_FORMAT_G8_B8R8_2PLANE_420_UNORM;
					_idc = ten ? STD_VIDEO_H265_PROFILE_IDC_MAIN_10 : STD_VIDEO_H265_PROFILE_IDC_MAIN;
				}
				_encode.stdProfileIdc = _idc;
				_decode.stdProfileIdc = _idc;
				_info.pNext = encode ? static_cast<void *>(&_encode) : static_cast<void *>(&_decode);
				_info.videoCodecOperation = encode ? VK_VIDEO_CODEC_OPERATION_ENCODE_H265_BIT_KHR :
					VK_VIDEO_CODEC_OPERATION_DECODE_H265_BIT_KHR;
				_info.chromaSubsampling = wide ? VK_VIDEO_CHROMA_SUBSAMPLING_444_BIT_KHR : VK_VIDEO_CHROMA_SUBSAMPLING_420_BIT_KHR;
				_info.lumaBitDepth = ten ? VK_VIDEO_COMPONENT_BIT_DEPTH_10_BIT_KHR : VK_VIDEO_COMPONENT_BIT_DEPTH_8_BIT_KHR;
				_info.chromaBitDepth = _info.lumaBitDepth;
				_list.profileCount = 1;
				_list.pProfiles = &_info;
			}
			Profile(const Profile &) = delete;
			Profile &operator=(const Profile &) = delete;
		};

		struct Physical_Device {
			std::shared_ptr<visual::Vulkan_Instance> _instance;
			VkPhysicalDevice _physical{};
			std::uint32_t _family{};
			PFN_vkGetPhysicalDeviceVideoCapabilitiesKHR _video_caps{};
			PFN_vkGetPhysicalDeviceVideoFormatPropertiesKHR _video_formats{};
			bool _encode{};
			bool _yuv444{};

			Status open(const char *path, bool encode) {
				struct stat node{};
				if (stat(path, &node) != 0 || !S_ISCHR(node.st_mode)) {
					return fail("Vulkan Video requires a DRM render node");
				}
				_encode = encode;
				auto instance = visual::vulkan_instance(visual::Vulkan_Version::Video);
				if (!instance) { return checked(instance.error(), "vkCreateInstance"); }
				_instance = std::move(*instance);
				_video_caps = reinterpret_cast<PFN_vkGetPhysicalDeviceVideoCapabilitiesKHR>(
					vkGetInstanceProcAddr(_instance->_handle, "vkGetPhysicalDeviceVideoCapabilitiesKHR"));
				_video_formats = reinterpret_cast<PFN_vkGetPhysicalDeviceVideoFormatPropertiesKHR>(
					vkGetInstanceProcAddr(_instance->_handle, "vkGetPhysicalDeviceVideoFormatPropertiesKHR"));
				if (!_video_caps || !_video_formats) { return fail("Vulkan loader has no Video capability queries"); }

				std::uint32_t count = 0;
				VIDEO_VK(vkEnumeratePhysicalDevices(_instance->_handle, &count, nullptr));
				std::vector<VkPhysicalDevice> devices(count);
				VIDEO_VK(vkEnumeratePhysicalDevices(_instance->_handle, &count, devices.data()));
				for (auto physical : devices) {
					auto drm = init_struct<VkPhysicalDeviceDrmPropertiesEXT>(VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRM_PROPERTIES_EXT);
					auto properties = init_struct<VkPhysicalDeviceProperties2>(VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2);
					properties.pNext = &drm;
					vkGetPhysicalDeviceProperties2(physical, &properties);
					if (properties.properties.apiVersion >= VK_API_VERSION_1_3 && drm.hasRender &&
						std::cmp_equal(drm.renderMajor, major(node.st_rdev)) && std::cmp_equal(drm.renderMinor, minor(node.st_rdev))) {
						_physical = physical;
						break;
					}
				}
				if (!_physical) { return fail("Vulkan Video found no Vulkan 1.3 device for the DRM render node"); }

				VIDEO_VK(vkEnumerateDeviceExtensionProperties(_physical, nullptr, &count, nullptr));
				std::vector<VkExtensionProperties> extensions(count);
				VIDEO_VK(vkEnumerateDeviceExtensionProperties(_physical, nullptr, &count, extensions.data()));
				const auto has_extension = [&](std::string_view name) {
					return std::ranges::any_of(extensions, [&](const auto &entry) { return name == entry.extensionName; });
				};
				for (const auto *extension : required_extensions()) {
					if (!has_extension(extension)) { return fail("DRM device lacks " + std::string(extension)); }
				}
				auto features = init_struct<VkPhysicalDeviceFeatures2>(VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2);
				auto sync = init_struct<VkPhysicalDeviceSynchronization2Features>(VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_SYNCHRONIZATION_2_FEATURES);
				auto yuv444 = init_struct<VkPhysicalDeviceYcbcr2Plane444FormatsFeaturesEXT>(
					VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_YCBCR_2_PLANE_444_FORMATS_FEATURES_EXT);
				features.pNext = &sync;
				if (has_extension(VK_EXT_YCBCR_2PLANE_444_FORMATS_EXTENSION_NAME)) { sync.pNext = &yuv444; }
				vkGetPhysicalDeviceFeatures2(_physical, &features);
				if (!sync.synchronization2) { return fail("DRM device lacks Vulkan synchronization2"); }
				_yuv444 = yuv444.ycbcr2plane444Formats;

				vkGetPhysicalDeviceQueueFamilyProperties2(_physical, &count, nullptr);
				std::vector queue_props(count, init_struct<VkQueueFamilyProperties2>(VK_STRUCTURE_TYPE_QUEUE_FAMILY_PROPERTIES_2));
				std::vector video_props(count, init_struct<VkQueueFamilyVideoPropertiesKHR>(VK_STRUCTURE_TYPE_QUEUE_FAMILY_VIDEO_PROPERTIES_KHR));
				std::vector status_props(count, init_struct<VkQueueFamilyQueryResultStatusPropertiesKHR>(
					VK_STRUCTURE_TYPE_QUEUE_FAMILY_QUERY_RESULT_STATUS_PROPERTIES_KHR));
				for (std::uint32_t index = 0; index < count; ++index) {
					queue_props[index].pNext = &video_props[index];
					video_props[index].pNext = &status_props[index];
				}
				vkGetPhysicalDeviceQueueFamilyProperties2(_physical, &count, queue_props.data());
				const auto operation = encode ? VK_VIDEO_CODEC_OPERATION_ENCODE_H265_BIT_KHR : VK_VIDEO_CODEC_OPERATION_DECODE_H265_BIT_KHR;
				for (std::uint32_t index = 0; index < count; ++index) {
					if ((video_props[index].videoCodecOperations & operation) && status_props[index].queryResultStatusSupport &&
						(queue_props[index].queueFamilyProperties.queueFlags &
						 (VK_QUEUE_GRAPHICS_BIT | VK_QUEUE_COMPUTE_BIT | VK_QUEUE_TRANSFER_BIT))) {
						_family = index;
						return {};
					}
				}
				return fail("no Vulkan H.265 queue supports transfers and video result status");
			}

			[[nodiscard]] std::array<const char *, 3> required_extensions() const {
				return {
					VK_KHR_VIDEO_QUEUE_EXTENSION_NAME,
					_encode ? VK_KHR_VIDEO_ENCODE_QUEUE_EXTENSION_NAME : VK_KHR_VIDEO_DECODE_QUEUE_EXTENSION_NAME,
					_encode ? VK_KHR_VIDEO_ENCODE_H265_EXTENSION_NAME : VK_KHR_VIDEO_DECODE_H265_EXTENSION_NAME,
				};
			}

			[[nodiscard]] bool supports(const Profile &profile, VkImageUsageFlags usage) const {
				auto info = init_struct<VkPhysicalDeviceVideoFormatInfoKHR>(VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VIDEO_FORMAT_INFO_KHR);
				info.pNext = &profile._list;
				info.imageUsage = usage;
				std::uint32_t count = 0;
				if (_video_formats(_physical, &info, &count, nullptr) != VK_SUCCESS || count == 0) { return false; }
				std::vector formats(count, init_struct<VkVideoFormatPropertiesKHR>(VK_STRUCTURE_TYPE_VIDEO_FORMAT_PROPERTIES_KHR));
				if (_video_formats(_physical, &info, &count, formats.data()) != VK_SUCCESS) { return false; }
				return std::ranges::any_of(formats, [&](const auto &format) {
					return format.format == profile._format && format.imageTiling == VK_IMAGE_TILING_OPTIMAL &&
						format.imageType == VK_IMAGE_TYPE_2D && (format.imageUsageFlags & usage) == usage;
				});
			}
		};

		constexpr VkImageUsageFlags ENCODE_SOURCE = VK_IMAGE_USAGE_VIDEO_ENCODE_SRC_BIT_KHR | VK_IMAGE_USAGE_TRANSFER_DST_BIT;
		constexpr VkImageUsageFlags ENCODE_DPB = VK_IMAGE_USAGE_VIDEO_ENCODE_DPB_BIT_KHR | VK_IMAGE_USAGE_TRANSFER_SRC_BIT;
		constexpr VkImageUsageFlags DECODE_OUTPUT = VK_IMAGE_USAGE_VIDEO_DECODE_DST_BIT_KHR | VK_IMAGE_USAGE_TRANSFER_SRC_BIT;
		constexpr VkImageUsageFlags DECODE_DPB = DECODE_OUTPUT | VK_IMAGE_USAGE_VIDEO_DECODE_DPB_BIT_KHR;

		struct Capabilities {
			VkVideoEncodeH265CapabilitiesKHR _h265 =
				init_struct<VkVideoEncodeH265CapabilitiesKHR>(VK_STRUCTURE_TYPE_VIDEO_ENCODE_H265_CAPABILITIES_KHR);
			VkVideoEncodeCapabilitiesKHR _encode =
				init_struct<VkVideoEncodeCapabilitiesKHR>(VK_STRUCTURE_TYPE_VIDEO_ENCODE_CAPABILITIES_KHR);
			VkVideoDecodeCapabilitiesKHR _decode =
				init_struct<VkVideoDecodeCapabilitiesKHR>(VK_STRUCTURE_TYPE_VIDEO_DECODE_CAPABILITIES_KHR);
			VkVideoDecodeH265CapabilitiesKHR _decode_h265 =
				init_struct<VkVideoDecodeH265CapabilitiesKHR>(VK_STRUCTURE_TYPE_VIDEO_DECODE_H265_CAPABILITIES_KHR);
			VkVideoCapabilitiesKHR _common = init_struct<VkVideoCapabilitiesKHR>(VK_STRUCTURE_TYPE_VIDEO_CAPABILITIES_KHR);

			Status query(const Physical_Device &device, const Profile &profile, bool vbr = false) {
				_common.pNext = device._encode ? static_cast<void *>(&_encode) : static_cast<void *>(&_decode);
				_encode.pNext = &_h265;
				_decode.pNext = &_decode_h265;
				VIDEO_VK(device._video_caps(device._physical, &profile._info, &_common));
				if (_common.maxDpbSlots == 0 || _common.maxActiveReferencePictures == 0) {
					return fail("Vulkan Video has no reconstruction slot");
				}
				if (profile._info.chromaSubsampling == VK_VIDEO_CHROMA_SUBSAMPLING_444_BIT_KHR && !device._yuv444) {
					return fail("Vulkan device lacks two-plane 4:4:4 images");
				}
				if (device._encode) {
					const auto rate_mode = vbr ? VK_VIDEO_ENCODE_RATE_CONTROL_MODE_VBR_BIT_KHR :
						VK_VIDEO_ENCODE_RATE_CONTROL_MODE_DISABLED_BIT_KHR;
					if ((_encode.supportedEncodeFeedbackFlags & FEEDBACK) != FEEDBACK ||
						!(_encode.rateControlModes & rate_mode) || (vbr && _encode.maxRateControlLayers == 0) ||
						!(_h265.ctbSizes & (VK_VIDEO_ENCODE_H265_CTB_SIZE_32_BIT_KHR | VK_VIDEO_ENCODE_H265_CTB_SIZE_64_BIT_KHR)) ||
						!(_h265.transformBlockSizes & VK_VIDEO_ENCODE_H265_TRANSFORM_BLOCK_SIZE_4_BIT_KHR) ||
						!(_h265.transformBlockSizes & VK_VIDEO_ENCODE_H265_TRANSFORM_BLOCK_SIZE_32_BIT_KHR) ||
						_h265.maxLevelIdc < STD_VIDEO_H265_LEVEL_IDC_5_1 ||
						_h265.requiresGopRemainingFrames || !device.supports(profile, ENCODE_SOURCE) ||
						!device.supports(profile, ENCODE_DPB)) {
						return fail("Vulkan H.265 encoder lacks the requested rate mode or reconstruction readback");
					}
				} else if (!(_decode.flags & VK_VIDEO_DECODE_CAPABILITY_DPB_AND_OUTPUT_COINCIDE_BIT_KHR) ||
						   !device.supports(profile, DECODE_DPB)) {
					return fail("Vulkan H.265 decoder lacks a readable coincident output and reference image");
				}
				return {};
			}
		};

		std::uint32_t probe(const char *path, std::span<const Frame_Format> formats, bool encode, bool vbr = false) {
			Physical_Device device{};
			if (formats.size() > 32 || !device.open(path, encode)) { return 0; }
			std::uint32_t supported = 0;
			for (std::size_t index = 0; index < formats.size(); ++index) {
				const auto [chroma, depth, scc] = formats[index];
				if (scc || (chroma != 420 && chroma != 444) || (depth != 8 && depth != 10)) { continue; }
				const Profile profile({.width = 0, .height = 0, .depth = depth, .chroma = chroma}, encode);
				Capabilities capabilities{};
				if (capabilities.query(device, profile, vbr)) { supported |= 1U << index; }
			}
			return supported;
		}

		VkDeviceSize aligned(VkDeviceSize size, VkDeviceSize alignment) {
			return (size + alignment - 1) / alignment * alignment;
		}

		struct Codec_Queue : Physical_Device {
			dev_t _node{};
			VkDevice _device{};
			VkQueue _queue{};
			std::mutex _submit;
			~Codec_Queue() {
				if (_device) { vkDestroyDevice(_device, nullptr); }
			}
			Status init() {
				if (_device) { return {}; }
				const float priority = 1;
				auto queues = init_struct<VkDeviceQueueCreateInfo>(VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO);
				queues.queueFamilyIndex = _family;
				queues.queueCount = 1;
				queues.pQueuePriorities = &priority;
				const auto required = required_extensions();
				std::vector extensions(required.begin(), required.end());
				auto sync = init_struct<VkPhysicalDeviceSynchronization2Features>(VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_SYNCHRONIZATION_2_FEATURES);
				sync.synchronization2 = VK_TRUE;
				auto yuv444 = init_struct<VkPhysicalDeviceYcbcr2Plane444FormatsFeaturesEXT>(
					VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_YCBCR_2_PLANE_444_FORMATS_FEATURES_EXT);
				if (_yuv444) {
					extensions.push_back(VK_EXT_YCBCR_2PLANE_444_FORMATS_EXTENSION_NAME);
					yuv444.ycbcr2plane444Formats = VK_TRUE;
					sync.pNext = &yuv444;
				}
				auto create = init_struct<VkDeviceCreateInfo>(VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO);
				create.pNext = &sync;
				create.queueCreateInfoCount = 1;
				create.pQueueCreateInfos = &queues;
				create.enabledExtensionCount = static_cast<std::uint32_t>(extensions.size());
				create.ppEnabledExtensionNames = extensions.data();
				VkDevice device{};
				VIDEO_VK(vkCreateDevice(_physical, &create, nullptr, &device));
				_device = device;
				vkGetDeviceQueue(_device, _family, 0, &_queue);
				return {};
			}
		};

		Result<std::shared_ptr<Codec_Queue>> codec_queue(const char *path, bool encode) {
			struct stat node{};
			if (stat(path, &node) != 0 || !S_ISCHR(node.st_mode)) {
				return fail("Vulkan Video requires a DRM render node");
			}
			thread_local std::array<std::weak_ptr<Codec_Queue>, 2> cached;
			// Video sessions own their parameters, images, and commands. Compatible
			// sessions keep the device and queue alive when the window resizes.
			if (auto existing = cached[encode].lock(); existing && existing->_node == node.st_rdev) {
				return existing;
			}
			auto next = std::make_shared<Codec_Queue>();
			VIDEO_TRY(next->open(path, encode));
			next->_node = node.st_rdev;
			cached[encode] = next;
			return next;
		}

		struct Video_Device {
			std::shared_ptr<Codec_Queue> _hardware;

			Status open(const char *path, bool encode) {
				auto queue = codec_queue(path, encode);
				if (!queue) { return std::unexpected(std::move(queue.error())); }
				_hardware = std::move(*queue);
				return {};
			}
			[[nodiscard]] const Physical_Device &physical() const { return *_hardware; }
			[[nodiscard]] VkDevice handle() const { return _hardware ? _hardware->_device : VK_NULL_HANDLE; }

			VkCommandPool _pool{};
			VkCommandBuffer _commands{};
			VkFence _fence{};
			VkVideoSessionKHR _session{};
			std::vector<VkDeviceMemory> _session_memory{};
			VkVideoSessionParametersKHR _parameters{};
			VkQueryPool _query{};
			bool _entered{};
			PFN_vkDestroyVideoSessionKHR _destroy_session{};
			PFN_vkCreateVideoSessionParametersKHR _create_parameters{};
			PFN_vkDestroyVideoSessionParametersKHR _destroy_parameters{};
			PFN_vkCmdBeginVideoCodingKHR _begin_video{};
			PFN_vkCmdControlVideoCodingKHR _control_video{};
			PFN_vkCmdEndVideoCodingKHR _end_video{};

			template<class T> T procedure(const char *name) const {
				return reinterpret_cast<T>(vkGetDeviceProcAddr(handle(), name));
			}

			~Video_Device() {
				if (!handle()) { return; }
				if (_query) { vkDestroyQueryPool(handle(), _query, nullptr); }
				if (_fence) { vkDestroyFence(handle(), _fence, nullptr); }
				if (_pool) { vkDestroyCommandPool(handle(), _pool, nullptr); }
				if (_parameters) { _destroy_parameters(handle(), _parameters, nullptr); }
				if (_session) { _destroy_session(handle(), _session, nullptr); }
				for (auto memory : _session_memory) { vkFreeMemory(handle(), memory, nullptr); }
			}

			Status init(const Profile &profile, const Capabilities &caps, Frame_Layout layout) {
				const auto &common = caps._common;
				if (layout.width < common.minCodedExtent.width || layout.height < common.minCodedExtent.height ||
					layout.width > common.maxCodedExtent.width || layout.height > common.maxCodedExtent.height) {
					return fail("coded frame extent is outside the Vulkan H.265 profile limits");
				}
				VIDEO_TRY(_hardware->init());

				const auto create_session = procedure<PFN_vkCreateVideoSessionKHR>("vkCreateVideoSessionKHR");
				const auto requirements = procedure<PFN_vkGetVideoSessionMemoryRequirementsKHR>("vkGetVideoSessionMemoryRequirementsKHR");
				const auto bind = procedure<PFN_vkBindVideoSessionMemoryKHR>("vkBindVideoSessionMemoryKHR");
				_destroy_session = procedure<PFN_vkDestroyVideoSessionKHR>("vkDestroyVideoSessionKHR");
				_create_parameters = procedure<PFN_vkCreateVideoSessionParametersKHR>("vkCreateVideoSessionParametersKHR");
				_destroy_parameters = procedure<PFN_vkDestroyVideoSessionParametersKHR>("vkDestroyVideoSessionParametersKHR");
				_begin_video = procedure<PFN_vkCmdBeginVideoCodingKHR>("vkCmdBeginVideoCodingKHR");
				_control_video = procedure<PFN_vkCmdControlVideoCodingKHR>("vkCmdControlVideoCodingKHR");
				_end_video = procedure<PFN_vkCmdEndVideoCodingKHR>("vkCmdEndVideoCodingKHR");
				if (!create_session || !requirements || !bind || !_destroy_session || !_create_parameters ||
					!_destroy_parameters || !_begin_video || !_control_video || !_end_video) {
					return fail("video device is missing Vulkan Video session functions");
				}
				const VkExtensionProperties header = _hardware->_encode ?
					VkExtensionProperties{.extensionName = VK_STD_VULKAN_VIDEO_CODEC_H265_ENCODE_EXTENSION_NAME,
						.specVersion = VK_STD_VULKAN_VIDEO_CODEC_H265_ENCODE_SPEC_VERSION} :
					VkExtensionProperties{.extensionName = VK_STD_VULKAN_VIDEO_CODEC_H265_DECODE_EXTENSION_NAME,
						.specVersion = VK_STD_VULKAN_VIDEO_CODEC_H265_DECODE_SPEC_VERSION};
				auto session = init_struct<VkVideoSessionCreateInfoKHR>(VK_STRUCTURE_TYPE_VIDEO_SESSION_CREATE_INFO_KHR);
				session.queueFamilyIndex = _hardware->_family;
				session.pVideoProfile = &profile._info;
				session.pictureFormat = profile._format;
				session.maxCodedExtent = {.width = layout.width, .height = layout.height};
				session.referencePictureFormat = profile._format;
				session.maxDpbSlots = 1;
				session.maxActiveReferencePictures = 1;
				session.pStdHeaderVersion = &header;
				VIDEO_VK(create_session(handle(), &session, nullptr, &_session));
				std::uint32_t count = 0;
				VIDEO_VK(requirements(handle(), _session, &count, nullptr));
				std::vector memories(count, init_struct<VkVideoSessionMemoryRequirementsKHR>(VK_STRUCTURE_TYPE_VIDEO_SESSION_MEMORY_REQUIREMENTS_KHR));
				if (count) {
					VIDEO_VK(requirements(handle(), _session, &count, memories.data()));
					std::vector bindings(count, init_struct<VkBindVideoSessionMemoryInfoKHR>(VK_STRUCTURE_TYPE_BIND_VIDEO_SESSION_MEMORY_INFO_KHR));
					_session_memory.resize(count);
					for (std::uint32_t index = 0; index < count; ++index) {
						const auto &memory = memories[index];
						VIDEO_TRY(allocate(memory.memoryRequirements, 0, _session_memory[index]));
						bindings[index].memoryBindIndex = memory.memoryBindIndex;
						bindings[index].memory = _session_memory[index];
						bindings[index].memorySize = memory.memoryRequirements.size;
					}
					VIDEO_VK(bind(handle(), _session, count, bindings.data()));
				}

				auto pool = init_struct<VkCommandPoolCreateInfo>(VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO);
				pool.flags = VK_COMMAND_POOL_CREATE_TRANSIENT_BIT;
				pool.queueFamilyIndex = _hardware->_family;
				VIDEO_VK(vkCreateCommandPool(handle(), &pool, nullptr, &_pool));
				auto command = init_struct<VkCommandBufferAllocateInfo>(VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO);
				command.commandPool = _pool;
				command.commandBufferCount = 1;
				VIDEO_VK(vkAllocateCommandBuffers(handle(), &command, &_commands));
				const auto fence = init_struct<VkFenceCreateInfo>(VK_STRUCTURE_TYPE_FENCE_CREATE_INFO);
				VIDEO_VK(vkCreateFence(handle(), &fence, nullptr, &_fence));

				auto feedback = init_struct<VkQueryPoolVideoEncodeFeedbackCreateInfoKHR>(
					VK_STRUCTURE_TYPE_QUERY_POOL_VIDEO_ENCODE_FEEDBACK_CREATE_INFO_KHR);
				feedback.pNext = &profile._info;
				feedback.encodeFeedbackFlags = FEEDBACK;
				auto query = init_struct<VkQueryPoolCreateInfo>(VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO);
				query.pNext = _hardware->_encode ? static_cast<const void *>(&feedback) : static_cast<const void *>(&profile._info);
				query.queryType = _hardware->_encode ? VK_QUERY_TYPE_VIDEO_ENCODE_FEEDBACK_KHR : VK_QUERY_TYPE_RESULT_STATUS_ONLY_KHR;
				query.queryCount = 1;
				return checked(vkCreateQueryPool(handle(), &query, nullptr, &_query), "create Vulkan Video status query");
			}

			Status allocate(VkMemoryRequirements requirements, VkMemoryPropertyFlags flags, VkDeviceMemory &memory,
							VkMemoryPropertyFlags preferred = 0) const {
				VkPhysicalDeviceMemoryProperties available{};
				vkGetPhysicalDeviceMemoryProperties(_hardware->_physical, &available);
				auto selected = available.memoryTypeCount;
				for (std::uint32_t i = 0; i < available.memoryTypeCount; ++i) {
					const auto properties = available.memoryTypes[i].propertyFlags;
					if ((requirements.memoryTypeBits & (1U << i)) && (properties & flags) == flags) {
						if ((properties & preferred) == preferred) {
							selected = i;
							break;
						}
						if (selected == available.memoryTypeCount) { selected = i; }
					}
				}
				if (selected == available.memoryTypeCount) { return fail("Vulkan Video has no compatible memory type"); }
				auto allocation = init_struct<VkMemoryAllocateInfo>(VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO);
				allocation.allocationSize = requirements.size;
				allocation.memoryTypeIndex = selected;
				return checked(vkAllocateMemory(handle(), &allocation, nullptr, &memory), "allocate Vulkan Video memory");
			}

			Status parameters(const void *info) {
				auto create = init_struct<VkVideoSessionParametersCreateInfoKHR>(VK_STRUCTURE_TYPE_VIDEO_SESSION_PARAMETERS_CREATE_INFO_KHR);
				create.pNext = info;
				create.videoSession = _session;
				VkVideoSessionParametersKHR replacement{};
				VIDEO_VK(_create_parameters(handle(), &create, nullptr, &replacement));
				if (_parameters) { _destroy_parameters(handle(), _parameters, nullptr); }
				_parameters = replacement;
				return {};
			}

			Status begin() const {
				VIDEO_VK(vkResetFences(handle(), 1, &_fence));
				VIDEO_VK(vkResetCommandPool(handle(), _pool, 0));
				auto begin = init_struct<VkCommandBufferBeginInfo>(VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO);
				begin.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
				VIDEO_VK(vkBeginCommandBuffer(_commands, &begin));
				vkCmdResetQueryPool(_commands, _query, 0, 1);
				return {};
			}

			Status submit() const {
				VIDEO_VK(vkEndCommandBuffer(_commands));
				auto submit = init_struct<VkSubmitInfo>(VK_STRUCTURE_TYPE_SUBMIT_INFO);
				submit.commandBufferCount = 1;
				submit.pCommandBuffers = &_commands;
				{
					std::lock_guard lock(_hardware->_submit);
					VIDEO_VK(vkQueueSubmit(_hardware->_queue, 1, &submit, _fence));
				}
				const auto result = vkWaitForFences(handle(), 1, &_fence, VK_TRUE, 2'000'000'000);
				if (result != VK_SUCCESS) {
					std::lock_guard lock(_hardware->_submit);
					(void) vkQueueWaitIdle(_hardware->_queue);
				}
				return checked(result, "complete Vulkan Video submission");
			}

			void begin_video(VkVideoReferenceSlotInfoKHR slot, const void *rate = nullptr, bool preserve_rate = false) {
				slot.slotIndex = -1;
				auto coding = init_struct<VkVideoBeginCodingInfoKHR>(VK_STRUCTURE_TYPE_VIDEO_BEGIN_CODING_INFO_KHR);
				coding.videoSession = _session;
				coding.videoSessionParameters = _parameters;
				coding.pNext = _entered ? rate : nullptr;
				coding.referenceSlotCount = 1;
				coding.pReferenceSlots = &slot;
				_begin_video(_commands, &coding);
				if (!_entered || !preserve_rate) {
					auto control = init_struct<VkVideoCodingControlInfoKHR>(VK_STRUCTURE_TYPE_VIDEO_CODING_CONTROL_INFO_KHR);
					control.flags = VK_VIDEO_CODING_CONTROL_RESET_BIT_KHR;
					if (rate) {
						control.pNext = rate;
						control.flags |= VK_VIDEO_CODING_CONTROL_ENCODE_RATE_CONTROL_BIT_KHR;
					}
					_control_video(_commands, &control);
				}
				vkCmdBeginQuery(_commands, _query, 0, 0);
				_entered = true;
			}

			void end_video() const {
				vkCmdEndQuery(_commands, _query, 0);
				const auto end = init_struct<VkVideoEndCodingInfoKHR>(VK_STRUCTURE_TYPE_VIDEO_END_CODING_INFO_KHR);
				_end_video(_commands, &end);
			}

			void host_readback() const {
				auto barrier = init_struct<VkMemoryBarrier2>(VK_STRUCTURE_TYPE_MEMORY_BARRIER_2);
				barrier.srcStageMask = VK_PIPELINE_STAGE_2_COPY_BIT;
				barrier.srcAccessMask = VK_ACCESS_2_TRANSFER_WRITE_BIT;
				if (_hardware->_encode) {
					barrier.srcStageMask |= VK_PIPELINE_STAGE_2_VIDEO_ENCODE_BIT_KHR;
					barrier.srcAccessMask |= VK_ACCESS_2_VIDEO_ENCODE_WRITE_BIT_KHR;
				}
				barrier.dstStageMask = VK_PIPELINE_STAGE_2_HOST_BIT;
				barrier.dstAccessMask = VK_ACCESS_2_HOST_READ_BIT;
				auto dependency = init_struct<VkDependencyInfo>(VK_STRUCTURE_TYPE_DEPENDENCY_INFO);
				dependency.memoryBarrierCount = 1;
				dependency.pMemoryBarriers = &barrier;
				vkCmdPipelineBarrier2(_commands, &dependency);
			}
		};

		struct Buffer {
			const Video_Device &_owner;
			VkBuffer _buffer{};
			VkDeviceMemory _memory{};
			void *_mapped{};
			VkDeviceSize _size{};
			~Buffer() {
				if (_mapped) { vkUnmapMemory(_owner.handle(), _memory); }
				if (_buffer) { vkDestroyBuffer(_owner.handle(), _buffer, nullptr); }
				if (_memory) { vkFreeMemory(_owner.handle(), _memory, nullptr); }
			}
			Status init(VkDeviceSize size, VkBufferUsageFlags usage, const void *next = nullptr, bool readback = false) {
				_size = size;
				auto info = init_struct<VkBufferCreateInfo>(VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO);
				info.pNext = next;
				info.size = size;
				info.usage = usage;
				VIDEO_VK(vkCreateBuffer(_owner.handle(), &info, nullptr, &_buffer));
				VkMemoryRequirements requirements{};
				vkGetBufferMemoryRequirements(_owner.handle(), _buffer, &requirements);
				const VkMemoryPropertyFlags preferred = readback ? VK_MEMORY_PROPERTY_HOST_CACHED_BIT : 0;
				VIDEO_TRY(_owner.allocate(requirements, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT, _memory, preferred));
				VIDEO_VK(vkBindBufferMemory(_owner.handle(), _buffer, _memory, 0));
				return checked(vkMapMemory(_owner.handle(), _memory, 0, size, 0, &_mapped), "map Vulkan Video buffer");
			}
		};

		struct Image {
			const Video_Device &_owner;
			VkImage _image{};
			VkDeviceMemory _memory{};
			VkImageView _view{};
			VkImageLayout _layout = VK_IMAGE_LAYOUT_UNDEFINED;
			VkPipelineStageFlags2 _stage = VK_PIPELINE_STAGE_2_NONE;
			VkAccessFlags2 _access = VK_ACCESS_2_NONE;
			~Image() {
				if (_view) { vkDestroyImageView(_owner.handle(), _view, nullptr); }
				if (_image) { vkDestroyImage(_owner.handle(), _image, nullptr); }
				if (_memory) { vkFreeMemory(_owner.handle(), _memory, nullptr); }
			}

			Status init(const Profile &profile, Frame_Layout layout, VkImageUsageFlags usage) {
				auto create = init_struct<VkImageCreateInfo>(VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO);
				create.pNext = &profile._list;
				create.imageType = VK_IMAGE_TYPE_2D;
				create.format = profile._format;
				create.extent = {.width = layout.width, .height = layout.height, .depth = 1};
				create.mipLevels = 1;
				create.arrayLayers = 1;
				create.samples = VK_SAMPLE_COUNT_1_BIT;
				create.tiling = VK_IMAGE_TILING_OPTIMAL;
				create.usage = usage;
				VIDEO_VK(vkCreateImage(_owner.handle(), &create, nullptr, &_image));
				VkMemoryRequirements requirements{};
				vkGetImageMemoryRequirements(_owner.handle(), _image, &requirements);
				VIDEO_TRY(_owner.allocate(requirements, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, _memory));
				VIDEO_VK(vkBindImageMemory(_owner.handle(), _image, _memory, 0));
				auto view = init_struct<VkImageViewCreateInfo>(VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO);
				view.image = _image;
				view.viewType = VK_IMAGE_VIEW_TYPE_2D;
				view.format = profile._format;
				view.subresourceRange = {.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT, .baseMipLevel = 0, .levelCount = 1,
					.baseArrayLayer = 0, .layerCount = 1};
				return checked(vkCreateImageView(_owner.handle(), &view, nullptr, &_view), "create Vulkan Video image view");
			}

			void transition(VkImageLayout layout, VkPipelineStageFlags2 stage, VkAccessFlags2 access) {
				auto barrier = init_struct<VkImageMemoryBarrier2>(VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER_2);
				barrier.srcStageMask = _stage;
				barrier.srcAccessMask = _access;
				barrier.dstStageMask = stage;
				barrier.dstAccessMask = access;
				barrier.oldLayout = _layout;
				barrier.newLayout = layout;
				barrier.srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
				barrier.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
				barrier.image = _image;
				barrier.subresourceRange = {.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT, .baseMipLevel = 0, .levelCount = 1,
					.baseArrayLayer = 0, .layerCount = 1};
				auto dependency = init_struct<VkDependencyInfo>(VK_STRUCTURE_TYPE_DEPENDENCY_INFO);
				dependency.imageMemoryBarrierCount = 1;
				dependency.pImageMemoryBarriers = &barrier;
				vkCmdPipelineBarrier2(_owner._commands, &dependency);
				_layout = layout;
				_stage = stage;
				_access = access;
			}

			[[nodiscard]] VkVideoPictureResourceInfoKHR picture(Frame_Layout layout) const {
				auto resource = init_struct<VkVideoPictureResourceInfoKHR>(VK_STRUCTURE_TYPE_VIDEO_PICTURE_RESOURCE_INFO_KHR);
				resource.codedExtent = {.width = layout.width, .height = layout.height};
				resource.imageViewBinding = _view;
				return resource;
			}
		};

		struct Planes {
			Frame_Layout _layout;
			std::uint32_t _factor;
			std::uint32_t _component_bytes;
			std::size_t _luma_bytes;
			std::size_t _chroma_width;
			std::size_t _chroma_height;

			explicit Planes(Frame_Layout layout) : _layout(layout), _factor(layout.chroma == 444 ? 1 : 2),
				_component_bytes(layout.depth == 8 ? 1 : 2),
				_luma_bytes(static_cast<std::size_t>(layout.width) * layout.height * _component_bytes),
				_chroma_width(layout.width / _factor), _chroma_height(layout.height / _factor) {}

			[[nodiscard]] std::size_t size() const { return _luma_bytes + _chroma_width * _chroma_height * 2 * _component_bytes; }

			[[nodiscard]] std::array<VkBufferImageCopy, 2> regions() const {
				return {{
					{.bufferOffset = 0, .bufferRowLength = 0, .bufferImageHeight = 0,
					 .imageSubresource = {.aspectMask = VK_IMAGE_ASPECT_PLANE_0_BIT, .mipLevel = 0, .baseArrayLayer = 0, .layerCount = 1},
					 .imageOffset = {}, .imageExtent = {.width = _layout.width, .height = _layout.height, .depth = 1}},
					{.bufferOffset = _luma_bytes, .bufferRowLength = 0, .bufferImageHeight = 0,
					 .imageSubresource = {.aspectMask = VK_IMAGE_ASPECT_PLANE_1_BIT, .mipLevel = 0, .baseArrayLayer = 0, .layerCount = 1},
					 .imageOffset = {}, .imageExtent = {.width = static_cast<std::uint32_t>(_chroma_width),
						.height = static_cast<std::uint32_t>(_chroma_height), .depth = 1}},
				}};
			}

			template<class T>
			void pack(T *data, std::span<const std::uint32_t> words) const {
				constexpr unsigned shift = sizeof(T) == 1 ? 0 : 6;
				std::ranges::transform(words, data, [](std::uint32_t word) {
					return static_cast<T>(((word >> 10) & 1023u) << shift);
				});
				auto *chroma = data + words.size();
				for (std::size_t y = 0; y < _layout.height; y += _factor) {
					const auto row = words.subspan(y * _layout.width, _layout.width);
					for (std::size_t x = 0; x < _layout.width; x += _factor) {
						const auto word = row[x];
						*chroma++ = static_cast<T>((word & 1023u) << shift);
						*chroma++ = static_cast<T>(((word >> 20) & 1023u) << shift);
					}
				}
			}

			template<class T>
			void unpack(const T *data, std::span<std::uint32_t> words) const {
				constexpr unsigned shift = sizeof(T) == 1 ? 0 : 6;
				const auto *chroma = data + words.size();
				for (std::size_t y = 0; y < _layout.height; ++y) {
					const auto *luma = data + y * _layout.width;
					const auto *uv = chroma + (y / _factor) * _chroma_width * 2;
					auto row = words.subspan(y * _layout.width, _layout.width);
					for (std::size_t x = 0; x < _layout.width; x += _factor) {
						const auto cb = static_cast<std::uint32_t>(*uv++) >> shift;
						const auto cr = static_cast<std::uint32_t>(*uv++) >> shift;
						const auto u_and_v = cb | (cr << 20);
						for (unsigned i = 0; i < _factor; ++i) {
							row[x + i] = u_and_v | ((static_cast<std::uint32_t>(luma[x + i]) >> shift) << 10);
						}
					}
				}
			}

			void write(void *data, std::span<const std::uint32_t> words) const {
				if (_component_bytes == 1) {
					pack(static_cast<std::uint8_t *>(data), words);
				} else {
					pack(static_cast<std::uint16_t *>(data), words);
				}
			}

			std::vector<std::uint32_t> read(const void *data) const {
				std::vector<std::uint32_t> output(static_cast<std::size_t>(_layout.width) * _layout.height);
				if (_component_bytes == 1) {
					unpack(static_cast<const std::uint8_t *>(data), output);
				} else {
					unpack(static_cast<const std::uint16_t *>(data), output);
				}
				return output;
			}
		};

		class Encoder final : public H265_Encode_Context {
			Frame_Layout _layout;
			Profile _profile;
			Planes _planes;
			Encode_Rate _rate;
			Capabilities _caps{};
			Video_Device _device{};
			Image _source{._owner = _device}, _reconstruction{._owner = _device};
			Buffer _upload{._owner = _device}, _readback{._owner = _device}, _bitstream{._owner = _device};
			PFN_vkCmdEncodeVideoKHR _encode{};
			std::vector<std::uint8_t> _headers{};

			Status parameters() {
				StdVideoH265ProfileTierLevel tier{};
				tier.flags.general_progressive_source_flag = 1;
				tier.flags.general_frame_only_constraint_flag = 1;
				tier.general_profile_idc = _profile._idc;
				tier.general_level_idc = std::uint64_t{_layout.width} * _layout.height * _rate._frame_rate > 534'773'760 ?
					STD_VIDEO_H265_LEVEL_IDC_5_2 : STD_VIDEO_H265_LEVEL_IDC_5_1;
				if (tier.general_level_idc > _caps._h265.maxLevelIdc) {
					return fail("requested frame rate exceeds the Vulkan H.265 profile level");
				}
				tier.flags.general_tier_flag = _rate._target_bitrate > 20'000'000;
				const StdVideoH265DecPicBufMgr dpb{};
				StdVideoH265VideoParameterSet vps{};
				vps.flags.vps_temporal_id_nesting_flag = 1;
				vps.pProfileTierLevel = &tier;
				vps.pDecPicBufMgr = &dpb;
				StdVideoH265SequenceParameterSetVui vui{};
				vui.flags.video_signal_type_present_flag = 1;
				vui.flags.video_full_range_flag = 1;
				vui.flags.colour_description_present_flag = 1;
				vui.flags.chroma_loc_info_present_flag = _layout.chroma == 420;
				vui.video_format = 5;
				vui.colour_primaries = static_cast<std::uint8_t>(Color_Primaries::BT709);
				vui.transfer_characteristics = static_cast<std::uint8_t>(Transfer_Function::SRGB);
				vui.matrix_coeffs = static_cast<std::uint8_t>(Matrix_Coefficients::BT709);
				vui.chroma_sample_loc_type_top_field = 1;
				vui.chroma_sample_loc_type_bottom_field = 1;
				vui.flags.vui_timing_info_present_flag = 1;
				vui.vui_num_units_in_tick = 1;
				vui.vui_time_scale = _rate._frame_rate;
				StdVideoH265SequenceParameterSet sps{};
				sps.flags.sps_temporal_id_nesting_flag = 1;
				sps.flags.vui_parameters_present_flag = 1;
				sps.flags.sample_adaptive_offset_enabled_flag =
					(_caps._h265.stdSyntaxFlags & VK_VIDEO_ENCODE_H265_STD_SAMPLE_ADAPTIVE_OFFSET_ENABLED_FLAG_SET_BIT_KHR) != 0;
				sps.chroma_format_idc = _layout.chroma == 444 ? STD_VIDEO_H265_CHROMA_FORMAT_IDC_444 : STD_VIDEO_H265_CHROMA_FORMAT_IDC_420;
				sps.pic_width_in_luma_samples = _layout.width;
				sps.pic_height_in_luma_samples = _layout.height;
				sps.bit_depth_luma_minus8 = static_cast<std::uint8_t>(_layout.depth - 8);
				sps.bit_depth_chroma_minus8 = sps.bit_depth_luma_minus8;
				sps.log2_max_pic_order_cnt_lsb_minus4 = 4;
				sps.log2_diff_max_min_luma_coding_block_size =
					(_caps._h265.ctbSizes & VK_VIDEO_ENCODE_H265_CTB_SIZE_32_BIT_KHR) ? 2 : 3;
				sps.log2_diff_max_min_luma_transform_block_size = 3;
				sps.max_transform_hierarchy_depth_inter = 2;
				sps.max_transform_hierarchy_depth_intra = 2;
				sps.pProfileTierLevel = &tier;
				sps.pDecPicBufMgr = &dpb;
				sps.pSequenceParameterSetVui = &vui;
				StdVideoH265PictureParameterSet pps{};
				pps.flags.pps_loop_filter_across_slices_enabled_flag = 1;
				pps.flags.cu_qp_delta_enabled_flag = _rate._target_bitrate != 0;
				auto add = init_struct<VkVideoEncodeH265SessionParametersAddInfoKHR>(VK_STRUCTURE_TYPE_VIDEO_ENCODE_H265_SESSION_PARAMETERS_ADD_INFO_KHR);
				add.stdVPSCount = 1;
				add.pStdVPSs = &vps;
				add.stdSPSCount = 1;
				add.pStdSPSs = &sps;
				add.stdPPSCount = 1;
				add.pStdPPSs = &pps;
				auto params = init_struct<VkVideoEncodeH265SessionParametersCreateInfoKHR>(
					VK_STRUCTURE_TYPE_VIDEO_ENCODE_H265_SESSION_PARAMETERS_CREATE_INFO_KHR);
				params.maxStdVPSCount = 1;
				params.maxStdSPSCount = 1;
				params.maxStdPPSCount = 1;
				params.pParametersAddInfo = &add;
				VIDEO_TRY(_device.parameters(&params));

				const auto get = _device.procedure<PFN_vkGetEncodedVideoSessionParametersKHR>("vkGetEncodedVideoSessionParametersKHR");
				if (!get) { return fail("Vulkan Video lacks encoded H.265 parameters"); }
				auto write = init_struct<VkVideoEncodeH265SessionParametersGetInfoKHR>(
					VK_STRUCTURE_TYPE_VIDEO_ENCODE_H265_SESSION_PARAMETERS_GET_INFO_KHR);
				write.writeStdVPS = VK_TRUE;
				write.writeStdSPS = VK_TRUE;
				write.writeStdPPS = VK_TRUE;
				auto request = init_struct<VkVideoEncodeSessionParametersGetInfoKHR>(
					VK_STRUCTURE_TYPE_VIDEO_ENCODE_SESSION_PARAMETERS_GET_INFO_KHR);
				request.pNext = &write;
				request.videoSessionParameters = _device._parameters;
				std::size_t size = 0;
				VIDEO_VK(get(_device.handle(), &request, nullptr, &size, nullptr));
				if (size == 0 || size > std::size_t{64} * 1024) { return fail("invalid Vulkan H.265 parameter-set length"); }
				_headers.resize(size);
				VIDEO_VK(get(_device.handle(), &request, nullptr, &size, _headers.data()));
				_headers.resize(size);
				return {};
			}

		public:
			explicit Encoder(Frame_Layout layout, Encode_Rate rate)
				: _layout(layout), _profile(layout, true), _planes(layout), _rate(rate) {}
			Status init(const char *path) {
				VIDEO_TRY(_device.open(path, true));
				VIDEO_TRY(_caps.query(_device.physical(), _profile, _rate._target_bitrate != 0));
				const auto max_target = std::min<std::uint64_t>(_caps._encode.maxBitrate, 160'000'000) / 2;
				if (_rate._target_bitrate > max_target) {
					if (!_rate._automatic_bitrate || max_target == 0) {
						return fail("requested peak bitrate exceeds Vulkan H.265 limits");
					}
					_rate._target_bitrate = static_cast<std::uint32_t>(max_target);
				}
				VIDEO_TRY(_device.init(_profile, _caps, _layout));
				_encode = _device.procedure<PFN_vkCmdEncodeVideoKHR>("vkCmdEncodeVideoKHR");
				if (!_encode) { return fail("Vulkan Video encode command unavailable"); }
				VIDEO_TRY(parameters());
				VIDEO_TRY(_source.init(_profile, _layout, ENCODE_SOURCE));
				VIDEO_TRY(_reconstruction.init(_profile, _layout, ENCODE_DPB));
				VIDEO_TRY(_upload.init(_planes.size(), VK_BUFFER_USAGE_TRANSFER_SRC_BIT));
				const auto size = aligned(MAX_UNIT, _caps._common.minBitstreamBufferSizeAlignment);
				return _bitstream.init(size, VK_BUFFER_USAGE_VIDEO_ENCODE_DST_BIT_KHR, &_profile._list, true);
			}

			Result<H265_Encoded_Picture> encode(std::span<const std::uint32_t> words, const H265_Encode_Picture &picture) override {
				if (!picture.idr_ || picture.type_ != H265_Slice_Type::I || !picture.references_l0_.empty() ||
					!picture.references_l1_.empty() || words.size() != static_cast<std::size_t>(_layout.width) * _layout.height) {
					return fail("Vulkan Video encoder requires a complete independent IDR");
				}
				const bool vbr = _rate._target_bitrate != 0;
				const std::int32_t qp = picture.cqp_.value_or(0);
				if (vbr == picture.cqp_.has_value() ||
					(!vbr && (qp < _caps._h265.minQp || qp > _caps._h265.maxQp))) {
					return fail("picture QP disagrees with the Vulkan H.265 rate-control mode");
				}
				if (picture.read_reconstruction_ && !_readback._buffer) {
					VIDEO_TRY(_readback.init(_planes.size(), VK_BUFFER_USAGE_TRANSFER_DST_BIT, nullptr, true));
				}
				_planes.write(_upload._mapped, words);
				VIDEO_TRY(_device.begin());
				_source.transition(VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, VK_PIPELINE_STAGE_2_COPY_BIT, VK_ACCESS_2_TRANSFER_WRITE_BIT);
				const auto regions = _planes.regions();
				vkCmdCopyBufferToImage(_device._commands, _upload._buffer, _source._image, _source._layout, regions.size(), regions.data());
				_source.transition(VK_IMAGE_LAYOUT_VIDEO_ENCODE_SRC_KHR, VK_PIPELINE_STAGE_2_VIDEO_ENCODE_BIT_KHR, VK_ACCESS_2_VIDEO_ENCODE_READ_BIT_KHR);
				_reconstruction.transition(VK_IMAGE_LAYOUT_VIDEO_ENCODE_DPB_KHR, VK_PIPELINE_STAGE_2_VIDEO_ENCODE_BIT_KHR, VK_ACCESS_2_VIDEO_ENCODE_WRITE_BIT_KHR);

				const auto reconstructed = _reconstruction.picture(_layout);
				StdVideoEncodeH265ReferenceInfo reference{};
				reference.pic_type = STD_VIDEO_H265_PICTURE_TYPE_IDR;
				auto dpb = init_struct<VkVideoEncodeH265DpbSlotInfoKHR>(VK_STRUCTURE_TYPE_VIDEO_ENCODE_H265_DPB_SLOT_INFO_KHR);
				dpb.pStdReferenceInfo = &reference;
				auto slot = init_struct<VkVideoReferenceSlotInfoKHR>(VK_STRUCTURE_TYPE_VIDEO_REFERENCE_SLOT_INFO_KHR);
				slot.pNext = &dpb;
				slot.pPictureResource = &reconstructed;

				auto rate = init_struct<VkVideoEncodeRateControlInfoKHR>(VK_STRUCTURE_TYPE_VIDEO_ENCODE_RATE_CONTROL_INFO_KHR);
				rate.rateControlMode = VK_VIDEO_ENCODE_RATE_CONTROL_MODE_DISABLED_BIT_KHR;
				auto layer = init_struct<VkVideoEncodeRateControlLayerInfoKHR>(VK_STRUCTURE_TYPE_VIDEO_ENCODE_RATE_CONTROL_LAYER_INFO_KHR);
				auto hevc_layer = init_struct<VkVideoEncodeH265RateControlLayerInfoKHR>(
					VK_STRUCTURE_TYPE_VIDEO_ENCODE_H265_RATE_CONTROL_LAYER_INFO_KHR);
				auto hevc_rate = init_struct<VkVideoEncodeH265RateControlInfoKHR>(VK_STRUCTURE_TYPE_VIDEO_ENCODE_H265_RATE_CONTROL_INFO_KHR);
				if (vbr) {
					hevc_rate.flags = VK_VIDEO_ENCODE_H265_RATE_CONTROL_REGULAR_GOP_BIT_KHR;
					hevc_rate.gopFrameCount = 1;
					hevc_rate.idrPeriod = 1;
					hevc_rate.subLayerCount = 1;
					// NVIDIA requires the codec-specific layer even when QP
					// bounds and frame sizes are left to the rate controller.
					layer.pNext = &hevc_layer;
					layer.averageBitrate = _rate._target_bitrate;
					layer.maxBitrate = layer.averageBitrate * 2;
					layer.frameRateNumerator = _rate._frame_rate;
					layer.frameRateDenominator = 1;
					rate.pNext = &hevc_rate;
					rate.rateControlMode = VK_VIDEO_ENCODE_RATE_CONTROL_MODE_VBR_BIT_KHR;
					rate.layerCount = 1;
					rate.pLayers = &layer;
					rate.virtualBufferSizeInMs = 250;
					rate.initialVirtualBufferSizeInMs = 125;
				}
				_device.begin_video(slot, &rate, vbr);
				StdVideoEncodeH265SliceSegmentHeader slice{};
				slice.flags.first_slice_segment_in_pic_flag = 1;
				slice.flags.slice_loop_filter_across_slices_enabled_flag = 1;
				slice.flags.slice_sao_luma_flag =
					(_caps._h265.stdSyntaxFlags & VK_VIDEO_ENCODE_H265_STD_SAMPLE_ADAPTIVE_OFFSET_ENABLED_FLAG_SET_BIT_KHR) != 0;
				slice.flags.slice_sao_chroma_flag = slice.flags.slice_sao_luma_flag;
				slice.slice_type = STD_VIDEO_H265_SLICE_TYPE_I;
				auto segment = init_struct<VkVideoEncodeH265NaluSliceSegmentInfoKHR>(
					VK_STRUCTURE_TYPE_VIDEO_ENCODE_H265_NALU_SLICE_SEGMENT_INFO_KHR);
				segment.constantQp = qp;
				segment.pStdSliceSegmentHeader = &slice;
				StdVideoEncodeH265PictureInfo standard{};
				standard.flags.is_reference = 1;
				standard.flags.IrapPicFlag = 1;
				standard.flags.pic_output_flag = 1;
				standard.pic_type = STD_VIDEO_H265_PICTURE_TYPE_IDR;
				auto frame = init_struct<VkVideoEncodeH265PictureInfoKHR>(VK_STRUCTURE_TYPE_VIDEO_ENCODE_H265_PICTURE_INFO_KHR);
				frame.naluSliceSegmentEntryCount = 1;
				frame.pNaluSliceSegmentEntries = &segment;
				frame.pStdPictureInfo = &standard;
				auto encode = init_struct<VkVideoEncodeInfoKHR>(VK_STRUCTURE_TYPE_VIDEO_ENCODE_INFO_KHR);
				encode.pNext = &frame;
				encode.dstBuffer = _bitstream._buffer;
				encode.dstBufferRange = _bitstream._size;
				encode.srcPictureResource = _source.picture(_layout);
				encode.pSetupReferenceSlot = &slot;
				_encode(_device._commands, &encode);
				_device.end_video();
				if (picture.read_reconstruction_) {
					_reconstruction.transition(VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, VK_PIPELINE_STAGE_2_COPY_BIT, VK_ACCESS_2_TRANSFER_READ_BIT);
					vkCmdCopyImageToBuffer(_device._commands, _reconstruction._image, _reconstruction._layout, _readback._buffer,
						regions.size(), regions.data());
				}
				_device.host_readback();
				VIDEO_TRY(_device.submit());

				std::array<std::uint32_t, 3> query{};
				VIDEO_VK(vkGetQueryPoolResults(_device.handle(), _device._query, 0, 1, sizeof(query), query.data(), sizeof(query),
					VK_QUERY_RESULT_WITH_STATUS_BIT_KHR));
				const auto [offset, size, status] = query;
				if (status != VK_QUERY_RESULT_STATUS_COMPLETE_KHR || size == 0 || offset > _bitstream._size ||
					size > _bitstream._size - offset || size > MAX_UNIT - _headers.size()) {
					return fail("Vulkan H.265 encoder reported an invalid access unit: status " +
						std::to_string(static_cast<std::int32_t>(status)) + ", offset " + std::to_string(offset) +
						", bytes " + std::to_string(size));
				}
				auto data = _headers;
				const auto *begin = static_cast<const std::uint8_t *>(_bitstream._mapped) + offset;
				data.insert(data.end(), begin, begin + size);
				return H265_Encoded_Picture{
					.frame_ = picture.frame_, .annex_b_ = std::move(data),
					.reconstruction_ = picture.read_reconstruction_ ? _planes.read(_readback._mapped) : std::vector<std::uint32_t>{},
				};
			}

			Result<void> release_reference(Frame_Id) override { return {}; }
		};

		class Decoder {
			Profile _profile;
			Planes _planes;
			Capabilities _caps{};
			Video_Device _device{};
			Image _output{._owner = _device};
			Buffer _bitstream{._owner = _device}, _readback{._owner = _device};
			PFN_vkCmdDecodeVideoKHR _decode{};
			std::vector<std::uint32_t> _offsets{};

			Status parameters(const decoder::Parameter_Bundle &bundle) {
				auto add = init_struct<VkVideoDecodeH265SessionParametersAddInfoKHR>(
					VK_STRUCTURE_TYPE_VIDEO_DECODE_H265_SESSION_PARAMETERS_ADD_INFO_KHR);
				add.stdVPSCount = 1;
				add.pStdVPSs = &bundle.vps_;
				add.stdSPSCount = 1;
				add.pStdSPSs = &bundle.sps_;
				add.stdPPSCount = 1;
				add.pStdPPSs = &bundle.pps_;
				auto create = init_struct<VkVideoDecodeH265SessionParametersCreateInfoKHR>(
					VK_STRUCTURE_TYPE_VIDEO_DECODE_H265_SESSION_PARAMETERS_CREATE_INFO_KHR);
				create.maxStdVPSCount = 1;
				create.maxStdSPSCount = 1;
				create.maxStdPPSCount = 1;
				create.pParametersAddInfo = &add;
				return _device.parameters(&create);
			}

			Result<VkDeviceSize> prepare_slices(const decoder::Parsed_Decode_Frame &frame,
											   std::span<const std::uint8_t> bitstream) {
				_offsets.clear();
				_offsets.reserve(frame.slices_.size());
				const std::array<std::uint8_t, 3> prefix{0, 0, 1};
				auto *mapped = static_cast<std::uint8_t *>(_bitstream._mapped);
				std::size_t written = 0;
				for (const auto &slice : frame.slices_) {
					const auto offset = slice.logical_offset_;
					const auto length = slice.nalu_.bytes.size();
					if (offset > bitstream.size() || length > bitstream.size() - offset ||
						prefix.size() + length > _bitstream._size - written) {
						return fail("parsed H.265 slice is outside the Vulkan Video source range");
					}
					_offsets.push_back(static_cast<std::uint32_t>(written));
					std::memcpy(mapped + written, prefix.data(), prefix.size());
					written += prefix.size();
					std::memcpy(mapped + written, bitstream.data() + offset, length);
					written += length;
				}
				const auto size = aligned(written, _caps._common.minBitstreamBufferSizeAlignment);
				if (written == 0 || size > _bitstream._size) { return fail("invalid Vulkan H.265 input length"); }
				std::memset(mapped + written, 0, size - written);
				return size;
			}

		public:
			const std::string _path;
			const Frame_Layout _layout;
			const StdVideoH265ProfileIdc _profile_idc;

			Decoder(const char *path, Frame_Layout layout, StdVideoH265ProfileIdc profile) :
				_profile(layout, false), _planes(layout), _path(path), _layout(layout), _profile_idc(profile) {
				_profile._decode.stdProfileIdc = profile;
			}

			Status init() {
				VIDEO_TRY(_device.open(_path.c_str(), false));
				VIDEO_TRY(_caps.query(_device.physical(), _profile));
				VIDEO_TRY(_device.init(_profile, _caps, _layout));
				_decode = _device.procedure<PFN_vkCmdDecodeVideoKHR>("vkCmdDecodeVideoKHR");
				if (!_decode) { return fail("Vulkan Video decode command unavailable"); }
				VIDEO_TRY(_output.init(_profile, _layout, DECODE_DPB));
				VIDEO_TRY(_readback.init(_planes.size(), VK_BUFFER_USAGE_TRANSFER_DST_BIT, nullptr, true));
				const auto size = aligned(MAX_UNIT, _caps._common.minBitstreamBufferSizeAlignment);
				return _bitstream.init(size, VK_BUFFER_USAGE_VIDEO_DECODE_SRC_BIT_KHR, &_profile._list);
			}

			Result<std::vector<std::uint32_t>> decode(const decoder::Parsed_Decode_Frame &frame,
													 std::span<const std::uint8_t> bitstream) {
				const decoder::Parameter_Bundle bundle(*frame.parameters_);
				VIDEO_TRY(parameters(bundle));
				const auto size = prepare_slices(frame, bitstream);
				if (!size) { return std::unexpected(size.error()); }
				VIDEO_TRY(_device.begin());
				_output.transition(VK_IMAGE_LAYOUT_VIDEO_DECODE_DPB_KHR, VK_PIPELINE_STAGE_2_VIDEO_DECODE_BIT_KHR,
					VK_ACCESS_2_VIDEO_DECODE_WRITE_BIT_KHR);
				const auto resource = _output.picture(_layout);
				const StdVideoDecodeH265ReferenceInfo reference{};
				auto dpb = init_struct<VkVideoDecodeH265DpbSlotInfoKHR>(VK_STRUCTURE_TYPE_VIDEO_DECODE_H265_DPB_SLOT_INFO_KHR);
				dpb.pStdReferenceInfo = &reference;
				auto slot = init_struct<VkVideoReferenceSlotInfoKHR>(VK_STRUCTURE_TYPE_VIDEO_REFERENCE_SLOT_INFO_KHR);
				slot.pNext = &dpb;
				slot.pPictureResource = &resource;
				_device.begin_video(slot);

				StdVideoDecodeH265PictureInfo standard{};
				standard.flags.IrapPicFlag = 1;
				standard.flags.IdrPicFlag = 1;
				standard.flags.IsReference = 1;
				standard.sps_video_parameter_set_id = bundle.sps_.sps_video_parameter_set_id;
				standard.pps_seq_parameter_set_id = bundle.pps_.pps_seq_parameter_set_id;
				standard.pps_pic_parameter_set_id = bundle.pps_.pps_pic_parameter_set_id;
				std::ranges::fill(standard.RefPicSetStCurrBefore, STD_VIDEO_H265_NO_REFERENCE_PICTURE);
				std::ranges::fill(standard.RefPicSetStCurrAfter, STD_VIDEO_H265_NO_REFERENCE_PICTURE);
				std::ranges::fill(standard.RefPicSetLtCurr, STD_VIDEO_H265_NO_REFERENCE_PICTURE);
				auto picture = init_struct<VkVideoDecodeH265PictureInfoKHR>(VK_STRUCTURE_TYPE_VIDEO_DECODE_H265_PICTURE_INFO_KHR);
				picture.pStdPictureInfo = &standard;
				picture.sliceSegmentCount = static_cast<std::uint32_t>(_offsets.size());
				picture.pSliceSegmentOffsets = _offsets.data();
				auto submission = init_struct<VkVideoDecodeInfoKHR>(VK_STRUCTURE_TYPE_VIDEO_DECODE_INFO_KHR);
				submission.pNext = &picture;
				submission.srcBuffer = _bitstream._buffer;
				submission.srcBufferRange = *size;
				submission.dstPictureResource = resource;
				submission.pSetupReferenceSlot = &slot;
				_decode(_device._commands, &submission);
				_device.end_video();
				_output.transition(VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, VK_PIPELINE_STAGE_2_COPY_BIT, VK_ACCESS_2_TRANSFER_READ_BIT);
				const auto regions = _planes.regions();
				vkCmdCopyImageToBuffer(_device._commands, _output._image, _output._layout, _readback._buffer,
					regions.size(), regions.data());
				_device.host_readback();
				VIDEO_TRY(_device.submit());
				std::int32_t decode_status = 0;
				VIDEO_VK(vkGetQueryPoolResults(_device.handle(), _device._query, 0, 1, sizeof(decode_status), &decode_status, sizeof(decode_status),
					VK_QUERY_RESULT_WITH_STATUS_BIT_KHR));
				if (decode_status != VK_QUERY_RESULT_STATUS_COMPLETE_KHR) {
					return fail("Vulkan H.265 decode failed with video status " + std::to_string(decode_status));
				}
				return _planes.read(_readback._mapped);
			}
		};
	}

	std::uint32_t encode_formats(const char *device, std::span<const Frame_Format> formats, bool vbr) {
		return probe(device, formats, true, vbr);
	}
	std::uint32_t decode_formats(const char *device, std::span<const Frame_Format> formats) { return probe(device, formats, false); }
	Result<std::unique_ptr<H265_Encode_Context>> create_encoder(const char *path, Frame_Layout layout, Encode_Rate rate) {
		if ((layout.chroma != 420 && layout.chroma != 444) || (layout.depth != 8 && layout.depth != 10)) {
			return fail("Vulkan H.265 encoder supports 8/10-bit 4:2:0 and 4:4:4");
		}
		auto encoder = std::make_unique<Encoder>(layout, rate);
		VIDEO_TRY(encoder->init(path));
		return encoder;
	}

	Result<std::vector<std::uint32_t>> decode_idr(const char *path, Frame_Layout layout,
												 const decoder::Parsed_Decode_Frame &picture, std::span<const std::uint8_t> bitstream,
												 bool alpha) {
		if ((layout.chroma != 420 && layout.chroma != 444) || (layout.depth != 8 && layout.depth != 10) || !picture.parameters_) {
			return fail("Vulkan H.265 decoder requires parsed 8/10-bit 4:2:0 or 4:4:4 IDR parameters");
		}
		const auto profile = static_cast<StdVideoH265ProfileIdc>(picture.parameters_->sps_.profile_tier_level.profile_idc);
		if (profile != STD_VIDEO_H265_PROFILE_IDC_MAIN && profile != STD_VIDEO_H265_PROFILE_IDC_MAIN_10 &&
			profile != STD_VIDEO_H265_PROFILE_IDC_FORMAT_RANGE_EXTENSIONS) {
			return fail("H.265 profile unavailable for Vulkan Video decode");
		}
		using Decoder_Cache = std::array<std::unique_ptr<Decoder>, 2>;
		thread_local std::array<Decoder_Cache, 2> decoders;
		auto &cache = decoders[alpha];
		auto &cached = cache.front();
		const auto matches = [&](const std::unique_ptr<Decoder> &entry) {
			return entry && entry->_path == path && entry->_layout == layout && entry->_profile_idc == profile;
		};
		if (!matches(cached) && matches(cache.back())) {
			std::swap(cached, cache.back());
		}
		if (!matches(cached)) {
			auto decoder = std::make_unique<Decoder>(path, layout, profile);
			VIDEO_TRY(decoder->init());
			if (cached) {
				cache.back() = std::move(cached);
			}
			cached = std::move(decoder);
		}
		auto decoded = cached->decode(picture, bitstream);
		if (!decoded) { cached.reset(); }
		return decoded;
	}
#undef VIDEO_VK
#undef VIDEO_TRY
}
