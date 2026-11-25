#pragma once

#include <array>
#include <cstdint>
#include <expected>
#include <memory>
#include <mutex>
#include <optional>
#include "visual_result.h"
#include <vulkan/vulkan_core.h>

namespace visual {

	struct Vulkan_Instance : Non_Copyable {
		VkInstance _handle{};
		~Vulkan_Instance() {
			if (_handle) { vkDestroyInstance(_handle, nullptr); }
		}
	};

	enum class Vulkan_Version : std::uint8_t { Compute, Video };

	inline std::mutex vulkan_instances_mutex;
	inline std::optional<std::array<std::shared_ptr<Vulkan_Instance>, 2>> vulkan_instances;

	[[nodiscard]] inline std::expected<std::shared_ptr<Vulkan_Instance>, VkResult> vulkan_instance(Vulkan_Version version) {
		std::lock_guard lock(vulkan_instances_mutex);
		auto *slot = vulkan_instances ? &(*vulkan_instances)[static_cast<std::size_t>(version)] : nullptr;
		if (slot && *slot) { return *slot; }

		const VkApplicationInfo application{
			.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
			.pNext = nullptr,
			.pApplicationName = "wayland-mcp",
			.applicationVersion = 0,
			.pEngineName = nullptr,
			.engineVersion = 0,
			.apiVersion = version == Vulkan_Version::Video ? VK_API_VERSION_1_3 : VK_API_VERSION_1_1,
		};
		const VkInstanceCreateInfo create{
			.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
			.pNext = nullptr,
			.flags = 0,
			.pApplicationInfo = &application,
			.enabledLayerCount = 0,
			.ppEnabledLayerNames = nullptr,
			.enabledExtensionCount = 0,
			.ppEnabledExtensionNames = nullptr,
		};
		VkInstance handle{};
		const auto status = vkCreateInstance(&create, nullptr, &handle);
		if (status != VK_SUCCESS) { return std::unexpected(status); }
		auto owner = std::make_shared<Vulkan_Instance>();
		owner->_handle = handle;
		if (slot) { *slot = owner; }
		return owner;
	}

}
