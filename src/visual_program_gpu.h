#pragma once
// Shared runtime definitions for the native implementation and GPU probes.
#include "shader_compiler.h"
#include "visual_gpu_internal.h"
#include "visual_normalize_shader.h"
#include <set>
#include <unordered_map>

[[nodiscard]] inline visual::Status check_program_contract(std::span<const uint32_t>     words,
														   const Visual_Program_Config  &cfg,
														   const VkPhysicalDeviceLimits &limits) {
	std::unordered_map<uint32_t, std::vector<uint32_t>> types;
	std::unordered_map<uint32_t, uint32_t>              bindings, sets, variables;
	std::unordered_map<uint64_t, uint32_t>              member_offsets;
	std::set<uint32_t>                                  readonly;
	std::set<uint64_t>                                  readonly_members;
	std::array<uint32_t, 3>                             local{
			{
					0,
					0,
					0,
			},
	};
	for (size_t i = 5; i < words.size();) {
		const uint32_t n = words[i] >> 16u, op = words[i] & 65535u;
		if (!n || i + n > words.size()) {
			return std::unexpected("invalid compiled SPIR-V instruction");
		}
		const auto w = words.subspan(i, n);
		if (op == 17 && n >= 2 && w[1] != 1 && w[1] != 50 && w[1] != 49) {
			return std::unexpected("program requires capabilities outside the 32-bit compute contract");
		}
		if (op == 16 && n >= 6 && w[2] == 17) {
			local[0] = w[3];
			local[1] = w[4];
			local[2] = w[5];
		}
		if (op >= 19 && op <= 39 && n >= 2) {
			types[w[1]] = std::vector<uint32_t>(w.begin(), w.end());
		}
		if (op == 59 && n >= 4) {
			variables[w[2]] = w[1];
		}
		if (op == 71 && n >= 3) {
			if (w[2] == 33 && n == 4) {
				bindings[w[1]] = w[3];
			}
			if (w[2] == 34 && n == 4) {
				sets[w[1]] = w[3];
			}
			if (w[2] == 24) {
				readonly.insert(w[1]);
			}
		}
		if (op == 72 && n == 5 && w[3] == 35) {
			member_offsets[(static_cast<uint64_t>(w[1]) << 32u) | w[2]] = w[4];
		}
		if (op == 72 && n >= 4 && w[3] == 24) {
			readonly_members.insert((static_cast<uint64_t>(w[1]) << 32u) | w[2]);
		}
		i += n;
	}
	auto const type_for = [&](uint32_t id) -> visual::Result<std::span<const uint32_t>> {
		auto const found = types.find(id);
		if (found == types.end() || found->second.empty()) {
			return std::unexpected("missing SPIR-V type");
		}
		return std::span<const uint32_t>(found->second);
	};
	uint64_t invocations = 1;
	for (int i = 0; i < 3; i++) {
		if (!local[i] || local[i] > limits.maxComputeWorkGroupSize[i]) {
			return std::unexpected("fixed local workgroup dimensions required within device limits");
		}
		invocations *= local[i];
	}
	if (invocations > limits.maxComputeWorkGroupInvocations) {
		return std::unexpected("local workgroup exceeds device limits");
	}
	for (auto [variable, binding]: bindings) {
		if (!sets.contains(variable) || sets[variable] != 0 || binding > 6) {
			return std::unexpected("program resources must use set 0, bindings 0..6");
		}
		if (binding == 1 && !cfg._previous_frame) {
			return std::unexpected("previous-frame image used without previousFrame:true");
		}
		auto const variable_type = variables.find(variable);
		if (variable_type == variables.end()) {
			return std::unexpected("missing SPIR-V resource variable");
		}
		auto pointer_result = type_for(variable_type->second);
		if (!pointer_result) {
			return std::unexpected(std::move(pointer_result.error()));
		}
		auto const pointer = *pointer_result;
		if ((pointer[0] & 65535u) != 32 || pointer.size() != 4) {
			return std::unexpected("unsupported resource pointer");
		}
		auto type_result = type_for(pointer[3]);
		if (!type_result) {
			return std::unexpected(std::move(type_result.error()));
		}
		auto const     type      = *type_result;
		const uint32_t kind      = type[0] & 65535u;
		bool           read_only = readonly.contains(variable);
		if (binding < 2) {
			if (kind != 25 || type.size() < 9 || type[3] != 1 || type[5] != 0 || type[6] != 0 || type[7] != 2 ||
				type[8] != 2) {
				return std::unexpected("frame bindings require readonly rgba16f image2D");
			}
		} else {
			if (kind != 30 || pointer[2] != 12) {
				return std::unexpected("buffer bindings require std430 storage buffers");
			}
			bool members_read_only = type.size() > 2;
			for (uint32_t m = 0; m + 2 < type.size(); m++) {
				members_read_only =
						members_read_only && readonly_members.contains((static_cast<uint64_t>(pointer[3]) << 32u) | m);
			}
			read_only = read_only || members_read_only;
		}
		if ((binding < 3 || binding == 6) && !read_only) {
			return std::unexpected(
					"current/previous frame, previous state and "
					"parameters must be readonly");
		}
	}
	for (auto [variable, pointer_id]: variables) {
		auto pointer_result = type_for(pointer_id);
		if (!pointer_result) {
			return std::unexpected(std::move(pointer_result.error()));
		}
		auto const pointer = *pointer_result;
		if (pointer.size() != 4) {
			return std::unexpected("unsupported variable pointer");
		}
		if (pointer[2] == 9) {
			auto block_result = type_for(pointer[3]);
			if (!block_result) {
				return std::unexpected(std::move(block_result.error()));
			}
			auto const block = *block_result;
			if ((block[0] & 65535u) != 30 || block.size() > 6) {
				return std::unexpected("push constants limited to four u32 words");
			}
			for (uint32_t m = 0; m + 2 < block.size(); m++) {
				auto type_result = type_for(block[m + 2]);
				if (!type_result) {
					return std::unexpected(std::move(type_result.error()));
				}
				auto const type   = *type_result;
				auto const offset = member_offsets.find((static_cast<uint64_t>(pointer[3]) << 32u) | m);
				if ((type[0] & 65535u) != 21 || type.size() != 4 || type[2] != 32 || type[3] != 0 ||
					offset == member_offsets.end() || offset->second != m * 4) {
					return std::unexpected(
							"push constants require width,height,sequence,historyValid as "
							"consecutive u32 words");
				}
			}
		}
		if ((pointer[2] == 0 || pointer[2] == 2 || pointer[2] == 12) && !bindings.contains(variable)) {
			return std::unexpected("unbound descriptor variable");
		}
	}
	for (auto [variable, set]: sets) {
		if (!bindings.contains(variable) || set != 0) {
			return std::unexpected("unbound or unsupported descriptor resource");
		}
	}
	return {};
}

struct Canonical_Frame : visual::Non_Copyable {
	Context       *_c{};
	VkImage        _image{};
	VkDeviceMemory _memory{};
	VkImageView    _view{};
	~Canonical_Frame() {
		if (_c) {
			vkDestroyImageView(_c->_device, _view, nullptr);
			vkDestroyImage(_c->_device, _image, nullptr);
			vkFreeMemory(_c->_device, _memory, nullptr);
		}
	}
	[[nodiscard]] visual::Status init(Context *ctx, uint32_t width, uint32_t height) {
		_c                        = ctx;
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
				.usage                 = VkFlags{VK_IMAGE_USAGE_STORAGE_BIT} | VkFlags{VK_IMAGE_USAGE_TRANSFER_DST_BIT},
				.sharingMode           = {},
				.queueFamilyIndexCount = {},
				.pQueueFamilyIndices   = {},
				.initialLayout         = {},
		};
		VISUAL_TRY(vk_status(vkCreateImage(_c->_device, &i, nullptr, &_image), "create canonical GPU image"));
		VkMemoryRequirements r;
		vkGetImageMemoryRequirements(_c->_device, _image, &r);
		VkMemoryAllocateInfo a = {
				.sType           = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
				.pNext           = {},
				.allocationSize  = r.size,
				.memoryTypeIndex = {},
		};
		auto memory_type_result = _c->memory_type(r.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT);
		if (!memory_type_result) {
			return std::unexpected(std::move(memory_type_result.error()));
		}
		auto const memory_type = *memory_type_result;
		a.memoryTypeIndex      = memory_type;
		VISUAL_TRY(
				vk_status(vkAllocateMemory(_c->_device, &a, nullptr, &_memory), "allocate unmapped canonical image"));
		VISUAL_TRY(vk_status(vkBindImageMemory(_c->_device, _image, _memory, 0), "bind canonical image"));
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
		VISUAL_TRY(vk_status(vkCreateImageView(_c->_device, &v, nullptr, &_view), "canonical image view"));
		return {};
	}
};
struct Runtime_Program : Program {
	Visual_Program_Config                _cfg{};
	std::array<Canonical_Frame, 2>       _frames{};
	std::array<Buffer, 2>                _states{};
	Buffer                               _scratch, _result, _parameters;
	VkDescriptorSetLayout                _agent_layout{}, _normalize_layout{};
	VkPipelineLayout                     _agent_pipeline_layout{}, _normalize_pipeline_layout{};
	VkDescriptorPool                     _descriptor_pool{};
	VkDescriptorSet                      _agent_set{}, _normalize_set{};
	VkPipeline                           _normalize_pipeline{};
	std::vector<VkPipeline>              _pipelines;
	std::vector<std::array<uint32_t, 3>> _dispatch;
	uint32_t                             _sequence = 0;
	explicit Runtime_Program(Context *c) : Program(c) {}
	~Runtime_Program() {
		for (auto p: _pipelines) {
			vkDestroyPipeline(_c->_device, p, nullptr);
		}
		vkDestroyPipeline(_c->_device, _normalize_pipeline, nullptr);
		vkDestroyDescriptorPool(_c->_device, _descriptor_pool, nullptr);
		vkDestroyPipelineLayout(_c->_device, _agent_pipeline_layout, nullptr);
		vkDestroyPipelineLayout(_c->_device, _normalize_pipeline_layout, nullptr);
		vkDestroyDescriptorSetLayout(_c->_device, _agent_layout, nullptr);
		vkDestroyDescriptorSetLayout(_c->_device, _normalize_layout, nullptr);
	}
	[[nodiscard]] visual::Result<VkPipeline> compile_pipeline(VkPipelineLayout          layout,
															  std::span<const uint32_t> words) {
		VkShaderModuleCreateInfo const s = {
				.sType    = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
				.pNext    = {},
				.flags    = {},
				.codeSize = words.size() * 4,
				.pCode    = words.data(),
		};
		VkShaderModule shader{};
		VISUAL_TRY(
				vk_status(vkCreateShaderModule(_c->_device, &s, nullptr, &shader), "create submitted shader module"));
		VkComputePipelineCreateInfo const p = {
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
				.layout             = layout,
				.basePipelineHandle = {},
				.basePipelineIndex  = {},
		};
		VkPipeline pipeline{};
		auto const r = vkCreateComputePipelines(_c->_device, VK_NULL_HANDLE, 1, &p, nullptr, &pipeline);
		vkDestroyShaderModule(_c->_device, shader, nullptr);
		if (r != VK_SUCCESS) {
			vkDestroyPipeline(_c->_device, pipeline, nullptr);
			return std::unexpected(std::string("create submitted compute pipeline: VkResult ") + std::to_string(r));
		}
		return pipeline;
	}
	[[nodiscard]] visual::Status init(const Visual_Program_Config &config, std::span<const Visual_Shader_Pass> passes,
									  std::span<const uint8_t> data) {
		if (passes.size() != config._pass_count || data.size() != config._parameter_bytes) {
			return std::unexpected("visual program input sizes do not match configuration");
		}
		_cfg = config;
		if (!_cfg._width || !_cfg._height || static_cast<uint64_t>(_cfg._width) * _cfg._height > 8'388'608 ||
			!_cfg._result_bytes || _cfg._result_bytes > 256 || _cfg._result_bytes % 4 || !_cfg._state_bytes ||
			_cfg._state_bytes > 65536 || _cfg._state_bytes % 4 || !_cfg._scratch_bytes ||
			_cfg._scratch_bytes > 1048576 || _cfg._scratch_bytes % 4 || _cfg._parameter_bytes > 4096 ||
			_cfg._parameter_bytes % 4 || !_cfg._pass_count || _cfg._pass_count > 8 || _cfg._alpha_mode > 2) {
			return std::unexpected("invalid visual program resource budget");
		}
		if (_cfg._feedback) {
			_cfg._state_bytes = _cfg._result_bytes;
		}
		_profile = default_visual_color(_cfg._alpha_mode);
		VISUAL_TRY(init_raw(_cfg._width, _cfg._height, 8));
		for (uint32_t i = 0; i < (_cfg._previous_frame ? 2u : 1u); i++) {
			VISUAL_TRY(_frames[i].init(_c, _cfg._width, _cfg._height));
		}
		const auto usage = VkFlags{VK_BUFFER_USAGE_STORAGE_BUFFER_BIT} | VkFlags{VK_BUFFER_USAGE_TRANSFER_DST_BIT} |
						   VkFlags{VK_BUFFER_USAGE_TRANSFER_SRC_BIT};
		for (auto &s: _states) {
			VISUAL_TRY(s.init(_c, _cfg._state_bytes, usage));
		}
		VISUAL_TRY(_scratch.init(_c, _cfg._scratch_bytes, usage));
		VISUAL_TRY(_result.init(_c, _cfg._result_bytes, usage, true));
		VISUAL_TRY(_parameters.init(_c, std::max(4u, _cfg._parameter_bytes), usage, true));
		std::memset(_parameters._mapped, 0, _parameters._size);
		if (_cfg._parameter_bytes) {
			std::memcpy(_parameters._mapped, data.data(), _cfg._parameter_bytes);
		}
		std::array<VkDescriptorSetLayoutBinding, 7> ab{};
		for (uint32_t i = 0; i < 7; i++) {
			ab[i] = {
					.binding            = i,
					.descriptorType     = i < 2 ? VK_DESCRIPTOR_TYPE_STORAGE_IMAGE : VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
					.descriptorCount    = 1,
					.stageFlags         = VK_SHADER_STAGE_COMPUTE_BIT,
					.pImmutableSamplers = nullptr,
			};
		}
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
		VkDescriptorSetLayoutCreateInfo const l = {
				.sType        = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
				.pNext        = {},
				.flags        = {},
				.bindingCount = 7,
				.pBindings    = ab.data(),
		};
		VISUAL_TRY(vk_status(vkCreateDescriptorSetLayout(_c->_device, &l, nullptr, &_agent_layout),
							 "agent descriptor layout"));
		VkDescriptorSetLayoutCreateInfo const normalize_layout_info = {
				.sType        = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
				.pNext        = {},
				.flags        = {},
				.bindingCount = 3,
				.pBindings    = nb.data(),
		};
		VISUAL_TRY(
				vk_status(vkCreateDescriptorSetLayout(_c->_device, &normalize_layout_info, nullptr, &_normalize_layout),
						  "normalization descriptor layout"));
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
				.pSetLayouts            = &_agent_layout,
				.pushConstantRangeCount = 1,
				.pPushConstantRanges    = &push,
		};
		VISUAL_TRY(vk_status(vkCreatePipelineLayout(_c->_device, &pl, nullptr, &_agent_pipeline_layout),
							 "agent pipeline layout"));
		VkPipelineLayoutCreateInfo const normalize_pipeline_layout_info = {
				.sType                  = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
				.pNext                  = {},
				.flags                  = {},
				.setLayoutCount         = 1,
				.pSetLayouts            = &_normalize_layout,
				.pushConstantRangeCount = 1,
				.pPushConstantRanges    = &push,
		};
		VISUAL_TRY(vk_status(vkCreatePipelineLayout(_c->_device, &normalize_pipeline_layout_info, nullptr,
													&_normalize_pipeline_layout),
							 "normalization pipeline layout"));
		auto normalizer_words_result = compile_visual_glsl(visual_normalize_source);
		if (!normalizer_words_result) {
			return std::unexpected(std::move(normalizer_words_result.error()));
		}
		auto normalizer_words           = std::move(*normalizer_words_result);
		auto normalizer_pipeline_result = compile_pipeline(_normalize_pipeline_layout, normalizer_words);
		if (!normalizer_pipeline_result) {
			return std::unexpected(std::move(normalizer_pipeline_result.error()));
		}
		auto const normalizer_pipeline = *normalizer_pipeline_result;
		_normalize_pipeline            = normalizer_pipeline;
		VkPhysicalDeviceProperties properties;
		vkGetPhysicalDeviceProperties(_c->_physical, &properties);
		for (uint32_t i = 0; i < _cfg._pass_count; i++) {
			auto words_result = compile_visual_glsl(std::string_view(passes[i]._source, passes[i]._source_size));
			if (!words_result) {
				return std::unexpected(std::move(words_result.error()));
			}
			auto words = std::move(*words_result);
			VISUAL_TRY(check_program_contract(words, _cfg, properties.limits));
			uint64_t groups = 1;
			for (int k = 0; k < 3; k++) {
				if (!passes[i]._groups[k] || passes[i]._groups[k] > properties.limits.maxComputeWorkGroupCount[k]) {
					return std::unexpected("dispatch exceeds device dimensions");
				}
				groups *= passes[i]._groups[k];
			}
			if (groups > 8'388'608) {
				return std::unexpected("dispatch workgroup budget exceeded");
			}
			auto pipeline_result = compile_pipeline(_agent_pipeline_layout, words);
			if (!pipeline_result) {
				return std::unexpected(std::move(pipeline_result.error()));
			}
			auto const pipeline = *pipeline_result;
			_pipelines.push_back(pipeline);
			_dispatch.push_back({
					passes[i]._groups[0],
					passes[i]._groups[1],
					passes[i]._groups[2],
			});
		}
		std::array<VkDescriptorPoolSize, 2> sizes{
				{
						{
								.type            = VK_DESCRIPTOR_TYPE_STORAGE_IMAGE,
								.descriptorCount = 3,
						},
						{
								.type            = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
								.descriptorCount = 7,
						},
				},
		};
		VkDescriptorPoolCreateInfo const d = {
				.sType         = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
				.pNext         = {},
				.flags         = {},
				.maxSets       = 2,
				.poolSizeCount = 2,
				.pPoolSizes    = sizes.data(),
		};
		VISUAL_TRY(vk_status(vkCreateDescriptorPool(_c->_device, &d, nullptr, &_descriptor_pool),
							 "program descriptor pool"));
		std::array<VkDescriptorSetLayout, 2> layouts{
				{
						_agent_layout,
						_normalize_layout,
				},
		};
		VkDescriptorSetAllocateInfo const a = {
				.sType              = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
				.pNext              = {},
				.descriptorPool     = _descriptor_pool,
				.descriptorSetCount = 2,
				.pSetLayouts        = layouts.data(),
		};
		std::array<VkDescriptorSet, 2> sets{};
		VISUAL_TRY(vk_status(vkAllocateDescriptorSets(_c->_device, &a, sets.data()), "program descriptor sets"));
		_agent_set     = sets[0];
		_normalize_set = sets[1];
		return {};
	}
	[[nodiscard]] visual::Result<int> analyze_result(uint8_t *output) {
		if (!_captured) {
			return std::unexpected("program analysis requires owned source data");
		}
		uint32_t const                       current = _sequence % 2, previous = 1 - current,
											 image_current  = _cfg._previous_frame ? current : 0,
											 image_previous = _cfg._previous_frame ? previous : 0;
		std::array<VkDescriptorImageInfo, 2> images{
				{
						{
								.sampler     = VK_NULL_HANDLE,
								.imageView   = _frames[image_current]._view,
								.imageLayout = VK_IMAGE_LAYOUT_GENERAL,
						},
						{
								.sampler     = VK_NULL_HANDLE,
								.imageView   = _frames[image_previous]._view,
								.imageLayout = VK_IMAGE_LAYOUT_GENERAL,
						},
				},
		};
		std::array<Buffer *, 5> buffers{
				{
						&_states[previous],
						&_states[current],
						&_scratch,
						&_result,
						&_parameters,
				},
		};
		std::array<VkDescriptorBufferInfo, 7> infos{};
		std::array<VkWriteDescriptorSet, 10>  writes{};
		for (uint32_t i = 0; i < 5; i++) {
			infos[i] = {
					.buffer = buffers[i]->_buffer,
					.offset = 0,
					.range  = buffers[i]->_size,
			};
		}
		infos[5] = {
				.buffer = _pixels._buffer,
				.offset = 0,
				.range  = _pixels._size,
		};
		infos[6] = {
				.buffer = _source_color._buffer,
				.offset = 0,
				.range  = _source_color._size,
		};
		for (uint32_t i = 0; i < 10; i++) {
			auto &w = writes[i];
			w       = {
					.sType           = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
					.pNext           = {},
					.dstSet          = i < 7 ? _agent_set : _normalize_set,
					.dstBinding      = i < 7 ? i : i - 7,
					.dstArrayElement = {},
					.descriptorCount = 1,
					.descriptorType =
							(i < 2 || i == 8) ? VK_DESCRIPTOR_TYPE_STORAGE_IMAGE : VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
					.pImageInfo       = i < 2    ? &images[i]
										: i == 8 ? &images[0]
												 : nullptr,
					.pBufferInfo      = (i < 2 || i == 8) ? nullptr
														  : &infos[i == 7   ? 5
																   : i == 9 ? 6
																			: i - 2],
					.pTexelBufferView = {},
			};
		}
		vkUpdateDescriptorSets(_c->_device, 10, writes.data(), 0, nullptr);
		VISUAL_TRY(begin_commands());
		barrier(VkFlags{VK_PIPELINE_STAGE_TRANSFER_BIT} | VkFlags{VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT} |
						VkFlags{VK_PIPELINE_STAGE_HOST_BIT},
				VkFlags{VK_PIPELINE_STAGE_TRANSFER_BIT} | VkFlags{VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT},
				VkFlags{VK_ACCESS_TRANSFER_WRITE_BIT} | VkFlags{VK_ACCESS_SHADER_READ_BIT} |
						VkFlags{VK_ACCESS_SHADER_WRITE_BIT} | VkFlags{VK_ACCESS_HOST_WRITE_BIT},
				VkFlags{VK_ACCESS_TRANSFER_READ_BIT} | VkFlags{VK_ACCESS_TRANSFER_WRITE_BIT} |
						VkFlags{VK_ACCESS_SHADER_READ_BIT} | VkFlags{VK_ACCESS_SHADER_WRITE_BIT});
		if (!_initialized) {
			for (uint32_t i = 0; i < (_cfg._previous_frame ? 2u : 1u); i++) {
				VkImageMemoryBarrier const b = {
						.sType               = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
						.pNext               = {},
						.srcAccessMask       = {},
						.dstAccessMask       = VK_ACCESS_TRANSFER_WRITE_BIT,
						.oldLayout           = VK_IMAGE_LAYOUT_UNDEFINED,
						.newLayout           = VK_IMAGE_LAYOUT_GENERAL,
						.srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
						.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
						.image               = _frames[i]._image,
						.subresourceRange =
								{
										.aspectMask     = VK_IMAGE_ASPECT_COLOR_BIT,
										.baseMipLevel   = 0,
										.levelCount     = 1,
										.baseArrayLayer = 0,
										.layerCount     = 1,
								},
				};
				vkCmdPipelineBarrier(_cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0,
									 nullptr, 0, nullptr, 1, &b);
				VkClearColorValue const zero{
						.float32 =
								{
										0,
										0,
										0,
										0,
								},
				};
				vkCmdClearColorImage(_cmd, b.image, VK_IMAGE_LAYOUT_GENERAL, &zero, 1, &b.subresourceRange);
			}
			for (auto const &s: _states) {
				vkCmdFillBuffer(_cmd, s._buffer, 0, s._size, 0);
			}
		} else {
			VkBufferCopy const copy{
					.srcOffset = 0,
					.dstOffset = 0,
					.size      = _cfg._state_bytes,
			};
			vkCmdCopyBuffer(_cmd, _states[previous]._buffer, _states[current]._buffer, 1, &copy);
		}
		vkCmdFillBuffer(_cmd, _scratch._buffer, 0, _scratch._size, 0);
		vkCmdFillBuffer(_cmd, _result._buffer, 0, _result._size, 0);
		barrier(VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_ACCESS_TRANSFER_WRITE_BIT,
				VkFlags{VK_ACCESS_SHADER_READ_BIT} | VkFlags{VK_ACCESS_SHADER_WRITE_BIT});
		std::array<uint32_t, 4> normalization{
				{
						_cfg._width,
						_cfg._height,
						_source_format,
						0,
				},
		};
		vkCmdBindPipeline(_cmd, VK_PIPELINE_BIND_POINT_COMPUTE, _normalize_pipeline);
		vkCmdBindDescriptorSets(_cmd, VK_PIPELINE_BIND_POINT_COMPUTE, _normalize_pipeline_layout, 0, 1, &_normalize_set,
								0, nullptr);
		vkCmdPushConstants(_cmd, _normalize_pipeline_layout, VK_SHADER_STAGE_COMPUTE_BIT, 0, 16, normalization.data());
		vkCmdDispatch(_cmd, (_cfg._width + 15) / 16, (_cfg._height + 15) / 16, 1);
		barrier(VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_ACCESS_SHADER_WRITE_BIT,
				VkFlags{VK_ACCESS_SHADER_READ_BIT} | VkFlags{VK_ACCESS_SHADER_WRITE_BIT});
		std::array<uint32_t, 4> metadata{
				{
						_cfg._width,
						_cfg._height,
						_sequence + 1,
						_initialized ? 1u : 0u,
				},
		};
		vkCmdBindDescriptorSets(_cmd, VK_PIPELINE_BIND_POINT_COMPUTE, _agent_pipeline_layout, 0, 1, &_agent_set, 0,
								nullptr);
		vkCmdPushConstants(_cmd, _agent_pipeline_layout, VK_SHADER_STAGE_COMPUTE_BIT, 0, 16, metadata.data());
		for (size_t i = 0; i < _pipelines.size(); i++) {
			vkCmdBindPipeline(_cmd, VK_PIPELINE_BIND_POINT_COMPUTE, _pipelines[i]);
			vkCmdDispatch(_cmd, _dispatch[i][0], _dispatch[i][1], _dispatch[i][2]);
			barrier(VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
					VkFlags{VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT} | VkFlags{VK_PIPELINE_STAGE_TRANSFER_BIT} |
							VkFlags{VK_PIPELINE_STAGE_HOST_BIT},
					VK_ACCESS_SHADER_WRITE_BIT,
					VkFlags{VK_ACCESS_SHADER_READ_BIT} | VkFlags{VK_ACCESS_SHADER_WRITE_BIT} |
							VkFlags{VK_ACCESS_TRANSFER_READ_BIT} | VkFlags{VK_ACCESS_HOST_READ_BIT});
		}
		if (_cfg._feedback) {
			barrier(VkFlags{VK_PIPELINE_STAGE_TRANSFER_BIT} | VkFlags{VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT},
					VK_PIPELINE_STAGE_TRANSFER_BIT,
					VkFlags{VK_ACCESS_TRANSFER_WRITE_BIT} | VkFlags{VK_ACCESS_SHADER_WRITE_BIT},
					VkFlags{VK_ACCESS_TRANSFER_READ_BIT} | VkFlags{VK_ACCESS_TRANSFER_WRITE_BIT});
			VkBufferCopy const copy{
					.srcOffset = 0,
					.dstOffset = 0,
					.size      = _result._size,
			};
			vkCmdCopyBuffer(_cmd, _result._buffer, _states[current]._buffer, 1, &copy);
		}
		// Include GPU-cleared fields even when a pass does not overwrite them.
		barrier(VkFlags{VK_PIPELINE_STAGE_TRANSFER_BIT} | VkFlags{VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT},
				VK_PIPELINE_STAGE_HOST_BIT, VkFlags{VK_ACCESS_TRANSFER_WRITE_BIT} | VkFlags{VK_ACCESS_SHADER_WRITE_BIT},
				VK_ACCESS_HOST_READ_BIT);
		VISUAL_TRY(submit_wait(false));
		_initialized = true;
		_captured    = false;
		_sequence++;
		std::memcpy(output, _result._mapped, _result._size);
		return static_cast<int>(_result._size);
	}
};
