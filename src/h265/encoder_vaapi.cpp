#include "h265/encoder.hpp"

#include "h265/frame_vaapi.hpp"
#include "video_editor/h265/api.hpp"

#include <fcntl.h>
#include <unistd.h>
#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_enc_hevc.h>

#include <algorithm>
#include <array>
#include <bit>
#include <cstdint>
#include <cstring>
#include <limits>
#include <memory>
#include <optional>
#include <ranges>
#include <span>
#include <string>
#include <unordered_map>
#include <utility>
#include <vector>

namespace video_editor::h265::encoder {
	namespace {
		[[nodiscard]] Media_Error error(Media_Error_Code code, std::string message) {
			return Media_Error{.code_ = code, .byte_offset_ = 0, .bit_offset_ = 0, .message_ = std::move(message)};
		}

		[[nodiscard]] bool status_ok(VAStatus status) { return status == VA_STATUS_SUCCESS; }


		struct Profile_Description {
			VAProfile              profile_;
			std::uint8_t           bit_depth_;
			encode::Chroma_Layout  chroma_;
			bool                   screen_content_;
			std::uint32_t          rt_format_;
			const char            *name_;
		};

		constexpr std::array kProfiles{
				Profile_Description{VAProfileHEVCMain, 8, encode::Chroma_Layout::YUV420, false,
								VA_RT_FORMAT_YUV420, "HEVC Main"},
				Profile_Description{VAProfileHEVCMain10, 10, encode::Chroma_Layout::YUV420, false,
								VA_RT_FORMAT_YUV420_10, "HEVC Main 10"},
				Profile_Description{VAProfileHEVCMain422_10, 10, encode::Chroma_Layout::YUV422, false,
								VA_RT_FORMAT_YUV422_10, "HEVC Main 4:2:2 10"},
				Profile_Description{VAProfileHEVCMain444, 8, encode::Chroma_Layout::YUV444, false,
								VA_RT_FORMAT_YUV444, "HEVC Main 4:4:4"},
				Profile_Description{VAProfileHEVCMain444_10, 10, encode::Chroma_Layout::YUV444, false,
								VA_RT_FORMAT_YUV444_10, "HEVC Main 4:4:4 10"},
				Profile_Description{VAProfileHEVCSccMain, 8, encode::Chroma_Layout::YUV420, true,
								VA_RT_FORMAT_YUV420, "HEVC SCC Main"},
				Profile_Description{VAProfileHEVCSccMain10, 10, encode::Chroma_Layout::YUV420, true,
								VA_RT_FORMAT_YUV420_10, "HEVC SCC Main 10"},
				Profile_Description{VAProfileHEVCSccMain444, 8, encode::Chroma_Layout::YUV444, true,
								VA_RT_FORMAT_YUV444, "HEVC SCC Main 4:4:4"},
				Profile_Description{VAProfileHEVCSccMain444_10, 10, encode::Chroma_Layout::YUV444, true,
								VA_RT_FORMAT_YUV444_10, "HEVC SCC Main 4:4:4 10"},
		};

		[[nodiscard]] constexpr unsigned chroma_idc(encode::Chroma_Layout chroma) {
			switch (chroma) {
				case encode::Chroma_Layout::YUV420: return 1;
				case encode::Chroma_Layout::YUV422: return 2;
				case encode::Chroma_Layout::YUV444: return 3;
			}
			return 0;
		}

		[[nodiscard]] bool contains(std::span<const VAProfile> profiles, VAProfile profile) {
			return std::ranges::find(profiles, profile) != profiles.end();
		}

		[[nodiscard]] bool contains(std::span<const VAEntrypoint> entrypoints, VAEntrypoint entrypoint) {
			return std::ranges::find(entrypoints, entrypoint) != entrypoints.end();
		}

		[[nodiscard]] std::vector<encode::Rate_Control_Mode> rate_controls(std::uint32_t mask) {
			std::vector<encode::Rate_Control_Mode> result;
			const auto add = [&](std::uint32_t bit, encode::Rate_Control_Mode mode) {
				if ((mask & bit) != 0U) { result.push_back(mode); }
			};
			add(VA_RC_CQP, encode::Rate_Control_Mode::CQP);
			add(VA_RC_ICQ, encode::Rate_Control_Mode::ICQ);
			add(VA_RC_VBR, encode::Rate_Control_Mode::VBR);
			add(VA_RC_CBR, encode::Rate_Control_Mode::CBR);
			add(VA_RC_QVBR, encode::Rate_Control_Mode::QVBR);
			add(VA_RC_VCM, encode::Rate_Control_Mode::VCM);
			add(VA_RC_TCBRC, encode::Rate_Control_Mode::TCBRC);
			return result;
		}

		[[nodiscard]] std::uint32_t va_rate_control(encode::Rate_Control_Mode mode) {
			switch (mode) {
				case encode::Rate_Control_Mode::CQP: return VA_RC_CQP;
				case encode::Rate_Control_Mode::ICQ: return VA_RC_ICQ;
				case encode::Rate_Control_Mode::VBR: return VA_RC_VBR;
				case encode::Rate_Control_Mode::CBR: return VA_RC_CBR;
				case encode::Rate_Control_Mode::QVBR: return VA_RC_QVBR;
				case encode::Rate_Control_Mode::VCM: return VA_RC_VCM;
				case encode::Rate_Control_Mode::TCBRC: return VA_RC_TCBRC;
			}
			return 0;
		}

		[[nodiscard]] bool source_pixel_format_supported(VADisplay display, VAProfile profile, VAEntrypoint entrypoint,
													std::uint32_t rt_format, std::uint32_t fourcc) {
			std::array<VAConfigAttrib, 2> attributes{{
					{.type = VAConfigAttribRTFormat, .value = rt_format},
					{.type = VAConfigAttribRateControl, .value = VA_RC_CQP},
			}};
			VAConfigID config = VA_INVALID_ID;
			if (!status_ok(vaCreateConfig(display, profile, entrypoint, attributes.data(), attributes.size(), &config))) {
				return false;
			}
			std::uint32_t count = 0;
			VAStatus status = vaQuerySurfaceAttributes(display, config, nullptr, &count);
			std::vector<VASurfaceAttrib> surface_attributes(count);
			if (status_ok(status) && count != 0U) {
				status = vaQuerySurfaceAttributes(display, config, surface_attributes.data(), &count);
			}
			(void) vaDestroyConfig(display, config);
			if (!status_ok(status)) { return false; }
			return std::ranges::any_of(surface_attributes, [&](const VASurfaceAttrib &attribute) {
				return attribute.type == VASurfaceAttribPixelFormat &&
					   attribute.value.type == VAGenericValueTypeInteger &&
					   static_cast<std::uint32_t>(attribute.value.value.i) == fourcc;
			});
		}

		[[nodiscard]] std::uint64_t mix_identity(std::uint64_t state, std::uint64_t value) {
			state ^= value;
			state *= 1099511628211ULL;
			return state;
		}

		[[nodiscard]] std::vector<encode::Codec_Level_Restriction> hevc_levels() {
			using Level = encode::Codec_Level_Restriction;
			return {
					Level{30U, 36'864U, 552'960U, 128'000U, 0U},
					Level{60U, 122'880U, 3'686'400U, 1'500'000U, 0U},
					Level{63U, 245'760U, 7'372'800U, 3'000'000U, 0U},
					Level{90U, 552'960U, 16'588'800U, 6'000'000U, 0U},
					Level{93U, 983'040U, 33'177'600U, 10'000'000U, 0U},
					Level{120U, 2'228'224U, 66'846'720U, 12'000'000U, 30'000'000U},
					Level{123U, 2'228'224U, 133'693'440U, 20'000'000U, 50'000'000U},
					Level{150U, 8'912'896U, 267'386'880U, 25'000'000U, 100'000'000U},
					Level{153U, 8'912'896U, 534'773'760U, 40'000'000U, 160'000'000U},
					Level{156U, 8'912'896U, 1'069'547'520U, 60'000'000U, 240'000'000U},
					Level{180U, 35'651'584U, 1'069'547'520U, 60'000'000U, 240'000'000U},
					Level{183U, 35'651'584U, 2'139'095'040U, 120'000'000U, 480'000'000U},
					Level{186U, 35'651'584U, 4'278'190'080U, 240'000'000U, 800'000'000U},
			};
		}

		[[nodiscard]] Result<Capability_Snapshot> query(VADisplay display) {
			const std::int32_t maximum_profiles = vaMaxNumProfiles(display);
			std::vector<VAProfile> profiles(static_cast<std::size_t>(std::max(maximum_profiles, 0)));
			std::int32_t profile_count = 0;
			VAStatus status = vaQueryConfigProfiles(display, profiles.data(), &profile_count);
			if (!status_ok(status)) {
				return std::unexpected(error(Media_Error_Code::UNSUPPORTED_DRIVER_FEATURE,
										 std::string{"failed to query H.265 encode profiles: "} + vaErrorStr(status)));
			}
			profiles.resize(static_cast<std::size_t>(profile_count));

			Capability_Snapshot snapshot;
			std::uint64_t identity = 1469598103934665603ULL;
			std::uint32_t next_id = 1;
			for (const auto &description: kProfiles) {
				if (!contains(profiles, description.profile_)) { continue; }
				const std::int32_t maximum_entrypoints = vaMaxNumEntrypoints(display);
				std::vector<VAEntrypoint> entrypoints(static_cast<std::size_t>(std::max(maximum_entrypoints, 0)));
				std::int32_t entrypoint_count = 0;
				status = vaQueryConfigEntrypoints(display, description.profile_, entrypoints.data(), &entrypoint_count);
				if (!status_ok(status)) { continue; }
				entrypoints.resize(static_cast<std::size_t>(entrypoint_count));
				auto entrypoint = VAEntrypointEncSlice;
				if (!contains(entrypoints, entrypoint)) {
					entrypoint = VAEntrypointEncSliceLP;
					if (!contains(entrypoints, entrypoint)) { continue; }
				}

				std::array<VAConfigAttrib, 13> attributes{{
						{.type = VAConfigAttribRTFormat, .value = 0},
						{.type = VAConfigAttribRateControl, .value = 0},
						{.type = VAConfigAttribMaxPictureWidth, .value = 0},
						{.type = VAConfigAttribMaxPictureHeight, .value = 0},
						{.type = VAConfigAttribEncMaxRefFrames, .value = 0},
						{.type = VAConfigAttribEncMaxSlices, .value = 0},
						{.type = VAConfigAttribEncSliceStructure, .value = 0},
						{.type = VAConfigAttribEncTileSupport, .value = 0},
						{.type = VAConfigAttribEncROI, .value = 0},
						{.type = VAConfigAttribEncIntraRefresh, .value = 0},
						{.type = VAConfigAttribEncHEVCBlockSizes, .value = 0},
						{.type = VAConfigAttribEncHEVCFeatures, .value = 0},
						{.type = VAConfigAttribEncQualityRange, .value = 0},
				}};
				status = vaGetConfigAttributes(display, description.profile_, entrypoint, attributes.data(), attributes.size());
				if (!status_ok(status) || attributes[0].value == VA_ATTRIB_NOT_SUPPORTED ||
					(attributes[0].value & description.rt_format_) == 0U) {
					continue;
				}

				Mode_Binding binding;
				if (attributes.back().value != VA_ATTRIB_NOT_SUPPORTED) {
					binding.max_quality_level_ = attributes.back().value;
				}
				auto &mode = binding.public_;
				mode.id_.value_      = next_id++;
				mode.codec_          = encode::Codec::H265;
				mode.bit_depth_      = description.bit_depth_;
				mode.chroma_         = description.chroma_;
				mode.screen_content_ = description.screen_content_;
				mode.device_mode_name_ = description.name_;
				mode.dimensions_.maximum_width_ = attributes[2].value == VA_ATTRIB_NOT_SUPPORTED ? 0U : attributes[2].value;
				mode.dimensions_.maximum_height_ = attributes[3].value == VA_ATTRIB_NOT_SUPPORTED ? 0U : attributes[3].value;
				mode.dimensions_.width_alignment_ = 8U;
				mode.dimensions_.height_alignment_ = 8U;
				mode.rate_control_.modes_ = attributes[1].value == VA_ATTRIB_NOT_SUPPORTED
											 ? std::vector<encode::Rate_Control_Mode>{}
											 : rate_controls(attributes[1].value);
				mode.levels_ = hevc_levels();
				if (attributes[4].value != VA_ATTRIB_NOT_SUPPORTED) {
					mode.max_reference_l0_ = attributes[4].value & 0xffffU;
					mode.max_reference_l1_ = attributes[4].value >> 16U;
				}
				mode.max_slices_ = attributes[5].value == VA_ATTRIB_NOT_SUPPORTED ? 1U : attributes[5].value;
				// Legacy HEVC encoders can require 32x32 CTUs and omit the block-size query.
				const VAConfigAttribValEncHEVCBlockSizes blocks{.value = attributes[10].value};
				binding.legacy_block_sizes_ = blocks.value == VA_ATTRIB_NOT_SUPPORTED;
				mode.coding_tree_block_size_ = binding.legacy_block_sizes_ ? 32U :
						8U << blocks.bits.log2_max_coding_tree_block_size_minus3;
				const VAConfigAttribValEncHEVCFeatures features{.value = attributes[11].value};
				binding.va_sao_supported_ = features.value != VA_ATTRIB_NOT_SUPPORTED &&
						features.bits.sao != VA_FEATURE_NOT_SUPPORTED;
				if (attributes[6].value != VA_ATTRIB_NOT_SUPPORTED) {
					mode.slices_equal_ctu_rows_ =
							(attributes[6].value & VA_ENC_SLICE_STRUCTURE_EQUAL_ROWS) != 0U;
					mode.device_slice_max_size_ =
							(attributes[6].value & VA_ENC_SLICE_STRUCTURE_MAX_SLICE_SIZE) != 0U;
				}
				mode.device_tiles_ = attributes[7].value != VA_ATTRIB_NOT_SUPPORTED && attributes[7].value != 0U;
				if (attributes[8].value != VA_ATTRIB_NOT_SUPPORTED) {
					VAConfigAttribValEncROI roi{};
					roi.value = attributes[8].value;
					mode.device_roi_qp_delta_ = roi.bits.num_roi_regions != 0U;
				}
				if (attributes[9].value != VA_ATTRIB_NOT_SUPPORTED) {
					mode.device_intra_refresh_rows_ =
							(attributes[9].value & VA_ENC_INTRA_REFRESH_ROLLING_ROW) != 0U;
					mode.device_intra_refresh_columns_ =
							(attributes[9].value & VA_ENC_INTRA_REFRESH_ROLLING_COLUMN) != 0U;
				}

				const Frame_Layout layout{
					.width = 0, .height = 0, .depth = description.bit_depth_,
					.chroma = chroma_idc(description.chroma_) == 1U ? 420U :
							  chroma_idc(description.chroma_) == 2U ? 422U : 444U,
				};
				if (source_pixel_format_supported(display, description.profile_, entrypoint,
												  description.rt_format_, layout.fourcc())) {
					mode.implemented_source_formats_.push_back(VK_FORMAT_R32_UINT);
				}
				binding.va_profile_    = description.profile_;
				binding.va_entrypoint_ = entrypoint;
				binding.va_rt_format_  = description.rt_format_;
				identity = mix_identity(identity, static_cast<std::uint64_t>(description.profile_));
				identity = mix_identity(identity, static_cast<std::uint64_t>(entrypoint));
				for (const auto &attribute: attributes) { identity = mix_identity(identity, attribute.value); }
				identity = mix_identity(identity, mode.implemented_source_formats_.size());
				snapshot.public_.modes_.push_back(mode);
				snapshot.modes_.push_back(std::move(binding));
			}
			snapshot.public_.identity_ = identity;
			return snapshot;
		}

		[[nodiscard]] std::uint32_t align_up(std::uint32_t value, std::uint32_t alignment) {
			return (value + alignment - 1U) / alignment * alignment;
		}

		[[nodiscard]] std::uint32_t coded_buffer_size(std::uint32_t width, std::uint32_t height,
											std::uint8_t bit_depth) {
			const std::uint64_t bytes_per_sample = bit_depth > 8U ? 2U : 1U;
			const std::uint64_t raw = static_cast<std::uint64_t>(width) * height * 3U * bytes_per_sample / 2U;
			return static_cast<std::uint32_t>(std::min<std::uint64_t>(
					std::max<std::uint64_t>(raw * 2U, 2U * 1024U * 1024U), std::numeric_limits<std::uint32_t>::max()));
		}

		[[nodiscard]] std::vector<std::uint8_t> misc_parameter(VAEncMiscParameterType type, std::size_t payload_size) {
			std::vector<std::uint8_t> bytes(sizeof(VAEncMiscParameterBuffer) + payload_size);
			auto *header = reinterpret_cast<VAEncMiscParameterBuffer *>(bytes.data());
			header->type = type;
			return bytes;
		}

		class Hevc_Bit_Writer {
		public:
			void bit(bool value) {
				if (bit_offset_ == 0U) { bytes_.push_back(0U); }
				if (value) { bytes_.back() |= static_cast<std::uint8_t>(1U << (7U - bit_offset_)); }
				bit_offset_ = (bit_offset_ + 1U) & 7U;
			}

			void bits(std::uint64_t value, std::uint32_t count) {
				for (std::uint32_t index = count; index != 0U; --index) {
					bit(((value >> (index - 1U)) & 1U) != 0U);
				}
			}

			void ue(std::uint32_t value) {
				const std::uint64_t code = static_cast<std::uint64_t>(value) + 1U;
				std::uint32_t width = 0U;
				for (auto remaining = code; remaining != 0U; remaining >>= 1U) { ++width; }
				for (std::uint32_t zero = 1U; zero < width; ++zero) { bit(false); }
				bits(code, width);
			}

			void se(std::int32_t value) {
				const auto code = value <= 0 ? static_cast<std::uint32_t>(-2LL * value)
										 : static_cast<std::uint32_t>(2LL * value - 1LL);
				ue(code);
			}

			void trailing_bits() {
				bit(true);
				while (bit_offset_ != 0U) { bit(false); }
			}

			[[nodiscard]] std::vector<std::uint8_t> take() && { return std::move(bytes_); }

		private:
			std::vector<std::uint8_t> bytes_;
			std::uint8_t bit_offset_ = 0U;
		};

		void hevc_nal_prefix(Hevc_Bit_Writer &writer, std::uint8_t nal_unit_type) {
			writer.bits(0x00000001U, 32U);
			writer.bit(false); // forbidden_zero_bit
			writer.bits(nal_unit_type, 6U);
			writer.bits(0U, 6U); // nuh_layer_id
			writer.bits(1U, 3U); // nuh_temporal_id_plus1
		}

		[[nodiscard]] std::vector<std::uint8_t> add_emulation_prevention(std::vector<std::uint8_t> raw) {
			if (raw.size() <= 6U) { return raw; }
			std::vector<std::uint8_t> escaped;
			escaped.reserve(raw.size() + raw.size() / 32U);
			escaped.insert(escaped.end(), raw.begin(), raw.begin() + 6);
			std::uint32_t consecutive_zeroes = 0U;
			for (auto byte = raw.begin() + 6; byte != raw.end(); ++byte) {
				if (consecutive_zeroes >= 2U && *byte <= 3U) {
					escaped.push_back(3U);
					consecutive_zeroes = 0U;
				}
				escaped.push_back(*byte);
				consecutive_zeroes = *byte == 0U ? consecutive_zeroes + 1U : 0U;
			}
			return escaped;
		}

		void profile_tier_level(Hevc_Bit_Writer &writer, std::uint8_t profile_idc, std::uint8_t level_idc,
							bool high_tier) {
			writer.bits(0U, 2U); // general_profile_space
			writer.bit(high_tier);
			writer.bits(profile_idc, 5U);
			for (std::uint32_t profile = 0; profile < 32U; ++profile) { writer.bit(profile == profile_idc); }
			writer.bit(true);  // progressive_source
			writer.bit(false); // interlaced_source
			writer.bit(false); // non_packed_constraint
			writer.bit(true);  // frame_only_constraint
			writer.bits(0U, 32U);
			writer.bits(0U, 12U);
			writer.bits(level_idc, 8U);
		}

		[[nodiscard]] std::uint32_t max_dpb_minus1(const H265_Encode_Plan &plan, const Mode_Binding &binding) {
			if (plan._decoded_picture_buffer_size != 0U) { return plan._decoded_picture_buffer_size - 1U; }
			return std::min<std::uint32_t>(
					std::max<std::uint32_t>(binding.public_.max_reference_l0_ + binding.public_.max_reference_l1_ + 1U, 1U),
					16U) - 1U;
		}

		[[nodiscard]] std::vector<std::uint8_t> packed_vps(const H265_Encode_Plan &plan,
														 const Mode_Binding &binding) {
			Hevc_Bit_Writer writer;
			hevc_nal_prefix(writer, 32U);
			writer.bits(0U, 4U); // vps_video_parameter_set_id
			writer.bit(true);
			writer.bit(true);
			writer.bits(0U, 6U); // vps_max_layers_minus1
			writer.bits(0U, 3U); // vps_max_sub_layers_minus1
			writer.bit(true);
			writer.bits(0xffffU, 16U);
			const auto profile = binding.public_.screen_content_ ? 9U :
					(chroma_idc(binding.public_.chroma_) != 1U ? 4U : binding.public_.bit_depth_ > 8U ? 2U : 1U);
			profile_tier_level(writer, profile, plan.level_idc_, plan.high_tier_);
			writer.bit(false); // sub_layer_ordering_info_present
			writer.ue(max_dpb_minus1(plan, binding));
			writer.ue(plan.ip_period_ > 1U ? plan.ip_period_ - 1U : 0U);
			writer.ue(0U);
			writer.bits(0U, 6U); // vps_max_layer_id
			writer.ue(0U);       // vps_num_layer_sets_minus1
			writer.bit(false);   // timing_info_present
			writer.bit(false);   // extension_flag
			writer.trailing_bits();
			return add_emulation_prevention(std::move(writer).take());
		}

		[[nodiscard]] std::vector<std::uint8_t> packed_sps(const H265_Encode_Plan &plan,
														 const Mode_Binding &binding) {
			Hevc_Bit_Writer writer;
			hevc_nal_prefix(writer, 33U);
			writer.bits(0U, 4U); // vps id
			writer.bits(0U, 3U); // max sub-layers minus one
			writer.bit(true);
			const auto profile = binding.public_.screen_content_ ? 9U :
					(chroma_idc(binding.public_.chroma_) != 1U ? 4U : binding.public_.bit_depth_ > 8U ? 2U : 1U);
			profile_tier_level(writer, profile, plan.level_idc_, plan.high_tier_);
			writer.ue(0U); // sps id
			writer.ue(binding.public_.chroma_ == encode::Chroma_Layout::YUV420 ? 1U :
					  (binding.public_.chroma_ == encode::Chroma_Layout::YUV422 ? 2U : 3U));
			if (binding.public_.chroma_ == encode::Chroma_Layout::YUV444) { writer.bit(false); }
			writer.ue(plan.stream_.width_);
			writer.ue(plan.stream_.height_);
			writer.bit(false); // conformance_window_flag; public plans require aligned visible dimensions
			writer.ue(binding.public_.bit_depth_ - 8U);
			writer.ue(binding.public_.bit_depth_ - 8U);
			writer.ue(plan.picture_order_count_lsb_bits_ - 4U);
			writer.bit(false);
			writer.ue(max_dpb_minus1(plan, binding));
			writer.ue(plan.ip_period_ > 1U ? plan.ip_period_ - 1U : 0U);
			writer.ue(0U);
			writer.ue(0U); // min coding block = 8
			writer.ue(std::bit_width(binding.public_.coding_tree_block_size_) - 4U);
			writer.ue(0U); // min transform block = 4
			writer.ue(3U); // max transform block = 32
			writer.ue(2U);
			writer.ue(2U);
			writer.bit(false); // scaling lists
			writer.bit(true);  // AMP
			writer.bit(plan.sample_adaptive_offset_);
			writer.bit(false); // PCM
			writer.ue(0U);     // driver writes a local short-term RPS from the explicit picture lists
			writer.bit(false); // long-term refs
			writer.bit(plan.temporal_mvp_);
			writer.bit(plan.strong_intra_smoothing_);
			writer.bit(true);  // VUI present
			writer.bit(false); // aspect_ratio_info_present
			writer.bit(false); // overscan_info_present
			writer.bit(true);  // video_signal_type_present
			writer.bits(5U, 3U); // unspecified source format
			writer.bit(plan.output_color_.range_ == Color_Range::FULL);
			writer.bit(true); // colour_description_present
			writer.bits(static_cast<std::uint8_t>(plan.output_color_.primaries_), 8U);
			writer.bits(static_cast<std::uint8_t>(plan.output_color_.transfer_), 8U);
			writer.bits(static_cast<std::uint8_t>(plan.output_color_.matrix_), 8U);
			const bool signal_location = binding.public_.chroma_ == encode::Chroma_Layout::YUV420;
			writer.bit(signal_location);
			if (signal_location) {
				writer.ue(1U); // center of the 2 x 2 luma block
				writer.ue(1U);
			}
			writer.bit(false); // neutral_chroma_indication
			writer.bit(false); // field_seq
			writer.bit(false); // frame_field_info_present
			writer.bit(false); // default_display_window
			writer.bit(true);  // vui_timing_info_present
			writer.bits(plan.stream_.framerate_den_, 32U);
			writer.bits(plan.stream_.framerate_num_, 32U);
			writer.bit(plan.ip_period_ > 1U); // poc_proportional_to_timing
			if (plan.ip_period_ > 1U) { writer.ue(0U); } // one tick per POC
			writer.bit(false); // hrd_parameters_present
			writer.bit(false); // bitstream_restriction
			writer.bit(binding.public_.screen_content_);
			if (binding.public_.screen_content_) {
				writer.bits(0x10U, 8U); // SCC extension
				writer.bit(plan.intra_block_copy_);
				writer.bit(plan.palette_mode_);
				if (plan.palette_mode_) {
					writer.ue(64U);
					writer.ue(64U);
					writer.bit(false);
				}
				writer.bits(0U, 2U); // motion_vector_resolution_control_idc
				writer.bit(false); // intra_boundary_filtering_disabled_flag
			}
			writer.trailing_bits();
			return add_emulation_prevention(std::move(writer).take());
		}

		[[nodiscard]] std::vector<std::uint8_t> packed_pps(const H265_Encode_Plan &plan, bool scc) {
			Hevc_Bit_Writer writer;
			hevc_nal_prefix(writer, 34U);
			writer.ue(0U); // pps id
			writer.ue(0U); // sps id
			writer.bit(false); // dependent slices
			writer.bit(false); // output flag present
			writer.bits(0U, 3U);
			writer.bit(false); // sign data hiding
			writer.bit(false); // cabac init present
			writer.ue(0U);
			writer.ue(0U);
			writer.se(static_cast<std::int32_t>(plan.rate_control_.initial_qp_) - 26);
			writer.bit(plan.constrained_intra_prediction_);
			writer.bit(plan.transform_skip_);
			const bool adaptive_quantization = plan.stream_.rate_control_ != encode::Rate_Control_Mode::CQP;
			writer.bit(adaptive_quantization); // cu_qp_delta_enabled
			if (adaptive_quantization) { writer.ue(0U); } // diff_cu_qp_delta_depth
			writer.se(plan.chroma_qp_offset_);
			writer.se(plan.chroma_qp_offset_);
			writer.bit(false); // slice chroma qp offsets
			writer.bit(false); // weighted pred
			writer.bit(false); // weighted bipred
			writer.bit(false); // transquant bypass
			writer.bit(false); // tiles
			writer.bit(false); // entropy coding sync
			writer.bit(true);  // loop filter across slices
			writer.bit(false); // deblocking filter control present
			writer.bit(false); // scaling list data
			writer.bit(true);  // lists modification present; picture plans carry explicit list order
			writer.ue(0U);     // log2_parallel_merge_level_minus2
			writer.bit(false); // slice header extension
			writer.bit(scc);
			if (scc) {
				writer.bits(0x10U, 8U);
				writer.bit(plan.intra_block_copy_);
				writer.bit(false); // residual adaptive color transform
				writer.bit(false); // palette predictor initializers
			}
			writer.trailing_bits();
			return add_emulation_prevention(std::move(writer).take());
		}

		struct Packed_Slice_Reference {
			Frame_Id     frame_;
			std::int32_t poc_ = 0;
			bool _used = true;
		};

		[[nodiscard]] std::uint32_t ceil_log2(std::uint32_t value) {
			std::uint32_t result = 0U;
			for (std::uint32_t bound = 1U; bound < value; bound <<= 1U) { ++result; }
			return result;
		}

		[[nodiscard]] std::uint8_t nal_unit_type(const H265_Encode_Picture &picture) {
			if (picture.idr_) { return 19U; }
			if (picture.type_ == H265_Slice_Type::I) { return 21U; }
			return picture.reference_ ? 1U : 0U;
		}

		[[nodiscard]] std::vector<std::uint8_t> packed_slice(
				const H265_Encode_Plan &plan, const H265_Encode_Picture &picture,
				std::span<const Packed_Slice_Reference> references, std::uint32_t slice_segment_address,
				std::uint32_t total_ctus, std::uint8_t picture_qp) {
			Hevc_Bit_Writer writer;
			const auto nal_type = nal_unit_type(picture);
			hevc_nal_prefix(writer, nal_type);
			const bool first_slice = slice_segment_address == 0U;
			writer.bit(first_slice);
			if (nal_type >= 16U && nal_type <= 23U) { writer.bit(false); }
			writer.ue(0U); // slice_pic_parameter_set_id
			if (!first_slice) { writer.bits(slice_segment_address, ceil_log2(total_ctus)); }
			writer.ue(static_cast<std::uint8_t>(picture.type_));

			std::vector<Packed_Slice_Reference> negative;
			std::vector<Packed_Slice_Reference> positive;
			if (!picture.idr_) {
				const auto poc_mask = (1U << plan.picture_order_count_lsb_bits_) - 1U;
				writer.bits(static_cast<std::uint32_t>(picture.picture_order_count_) & poc_mask,
							plan.picture_order_count_lsb_bits_);
				writer.bit(false); // short_term_ref_pic_set_sps_flag; explicit RPS follows
				for (const auto &reference: references) {
					(reference.poc_ < picture.picture_order_count_ ? negative : positive).push_back(reference);
				}
				std::ranges::sort(negative, {}, &Packed_Slice_Reference::poc_);
				std::ranges::reverse(negative);
				std::ranges::sort(positive, {}, &Packed_Slice_Reference::poc_);
				writer.ue(negative.size());
				writer.ue(positive.size());
				std::uint32_t previous_distance = 0U;
				for (const auto &reference: negative) {
					const auto distance = static_cast<std::uint32_t>(picture.picture_order_count_ - reference.poc_);
					writer.ue(distance - previous_distance - 1U);
					writer.bit(reference._used);
					previous_distance = distance;
				}
				previous_distance = 0U;
				for (const auto &reference: positive) {
					const auto distance = static_cast<std::uint32_t>(reference.poc_ - picture.picture_order_count_);
					writer.ue(distance - previous_distance - 1U);
					writer.bit(reference._used);
					previous_distance = distance;
				}
				if (plan.temporal_mvp_) { writer.bit(picture.type_ != H265_Slice_Type::I); }
			}

			if (plan.sample_adaptive_offset_) {
				writer.bit(true);
				writer.bit(true);
			}
			if (picture.type_ != H265_Slice_Type::I) {
				const bool override_counts = picture.references_l0_.size() != 1U ||
						(picture.type_ == H265_Slice_Type::B && picture.references_l1_.size() != 1U);
				writer.bit(override_counts);
				if (override_counts) {
					writer.ue(picture.references_l0_.size() - 1U);
					if (picture.type_ == H265_Slice_Type::B) { writer.ue(picture.references_l1_.size() - 1U); }
				}

				const auto used = std::ranges::count(references, true, &Packed_Slice_Reference::_used);
				const auto list_bits = ceil_log2(used);
				const auto write_list = [&](std::span<const Frame_Id> requested,
											std::span<const Packed_Slice_Reference> first,
											std::span<const Packed_Slice_Reference> second) {
					if (used <= 1) { return; }
					writer.bit(true);
					for (const auto id: requested) {
						std::uint32_t index = 0U;
						bool found = false;
						for (const auto &candidate: first) {
							if (!candidate._used) { continue; }
							if (candidate.frame_ == id) { found = true; break; }
							++index;
						}
						if (!found) {
							for (const auto &candidate: second) {
								if (!candidate._used) { continue; }
								if (candidate.frame_ == id) { found = true; break; }
								++index;
							}
						}
						writer.bits(index, list_bits);
					}
				};
				write_list(picture.references_l0_, negative, positive);
				if (picture.type_ == H265_Slice_Type::B) { write_list(picture.references_l1_, positive, negative); }
				if (picture.type_ == H265_Slice_Type::B) { writer.bit(false); }
				if (plan.temporal_mvp_) {
					const bool temporal_mvp = picture.type_ != H265_Slice_Type::I;
					if (temporal_mvp && picture.type_ == H265_Slice_Type::B) { writer.bit(true); }
				}
				writer.ue(0U); // five_minus_max_num_merge_cand
			}
			writer.se(static_cast<std::int32_t>(picture_qp) - plan.rate_control_.initial_qp_);
			writer.bit(true); // slice_loop_filter_across_slices_enabled_flag
			writer.trailing_bits();
			return add_emulation_prevention(std::move(writer).take());
		}

		class Vaapi_H265_Encode_Context final : public Va_Encode_Context {
		public:
			Vaapi_H265_Encode_Context(H265_Encode_Plan plan, Mode_Binding binding, std::shared_ptr<Va_Display> display,
									 VAConfigID config, VAContextID context, std::vector<VASurfaceID> surfaces)
				: plan_(std::move(plan)), binding_(std::move(binding)), display_owner_(std::move(display)),
				  display_(display_owner_->get()), config_(config), context_(context), surfaces_(std::move(surfaces)),
				  free_surfaces_(surfaces_) {}

			~Vaapi_H265_Encode_Context() override {
				discard_input();
				if (context_ != VA_INVALID_ID) { (void) vaDestroyContext(display_, context_); }
				if (!surfaces_.empty()) {
					(void) vaDestroySurfaces(display_, surfaces_.data(), static_cast<int>(surfaces_.size()));
				}
				if (config_ != VA_INVALID_ID) { (void) vaDestroyConfig(display_, config_); }
			}

			[[nodiscard]] Frame_Layout input_layout() const {
				return Frame_Layout{
					.width = plan_.stream_.width_, .height = plan_.stream_.height_, .depth = binding_.public_.bit_depth_,
					.chroma = chroma_idc(binding_.public_.chroma_) == 1U ? 420U :
							  chroma_idc(binding_.public_.chroma_) == 2U ? 422U : 444U,
				};
			}
			[[nodiscard]] Result<Va_Surface> prepare_input() override {
				auto &slot = input_surfaces_[next_input_];
				if (slot == VA_INVALID_SURFACE) {
					auto source = create_surface(display_, input_layout(), VA_SURFACE_ATTRIB_USAGE_HINT_ENCODER);
					if (!source) { return std::unexpected(source.error()); }
					slot = *source;
				}
				return Va_Surface{.display = display_, .surface = slot, .owner = owner_, .lifetime = display_owner_};
			}
			[[nodiscard]] Result<H265_Encoded_Picture>
			encode(std::span<const std::uint32_t> pixels, const H265_Encode_Picture &picture) override {
				return encode_frame(pixels, picture, false);
			}
			[[nodiscard]] Result<H265_Encoded_Picture>
			encode_prepared(const H265_Encode_Picture &picture) override {
				if (input_surfaces_[next_input_] == VA_INVALID_SURFACE) {
					return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT, "missing prepared VA input surface"));
				}
				return encode_frame({}, picture, true);
			}
			[[nodiscard]] Result<H265_Encoded_Picture>
			encode_frame(std::span<const std::uint32_t> pixels, const H265_Encode_Picture &picture, bool prepared) {
				const auto layout = input_layout();
				if (!prepared && pixels.size() != std::size_t(layout.width) * layout.height) {
					return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT,
											 "H.265 source image does not match the resolved stream plan"));
				}
				if (picture.idr_ && picture.type_ != H265_Slice_Type::I) {
					return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT, "an H.265 IDR picture must be I-coded"));
				}
				if (picture.picture_order_count_ < 0 ||
					static_cast<std::uint32_t>(picture.picture_order_count_) >=
							(1U << plan_.picture_order_count_lsb_bits_)) {
					return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT,
											 "H.265 picture POC must fit the resolved SPS POC-LSB width"));
				}
				if (picture.type_ == H265_Slice_Type::I &&
					(!picture.references_l0_.empty() || !picture.references_l1_.empty() ||
					 !picture._retained_references.empty())) {
					return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT,
											 "an H.265 I picture cannot have reference lists"));
				}
				if (picture.type_ == H265_Slice_Type::P && picture.references_l0_.empty()) {
					return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT,
											 "an H.265 P picture requires an explicit L0 reference"));
				}
				if (picture.type_ == H265_Slice_Type::B &&
					(picture.references_l0_.empty() || picture.references_l1_.empty())) {
					return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT,
											 "an H.265 B picture requires explicit L0 and L1 references"));
				}
				if (picture.references_l0_.size() > binding_.public_.max_reference_l0_ ||
					picture.references_l1_.size() > binding_.public_.max_reference_l1_) {
					return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT,
											 "H.265 picture reference list exceeds the selected mode restriction"));
				}
				for (const auto reference: picture.references_l0_) {
					if (!references_.contains(reference.value_)) {
						return std::unexpected(error(Media_Error_Code::INVALID_REFERENCE_STATE,
												 "H.265 L0 list names a reference which is not resident"));
					}
				}
				for (const auto reference: picture.references_l1_) {
					if (!references_.contains(reference.value_)) {
						return std::unexpected(error(Media_Error_Code::INVALID_REFERENCE_STATE,
												 "H.265 L1 list names a reference which is not resident"));
					}
				}
				const auto has_duplicate = [](const std::vector<Frame_Id> &list) {
					std::vector<std::uint64_t> ids;
					for (const auto id: list) { ids.push_back(id.value_); }
					std::ranges::sort(ids);
					return std::ranges::adjacent_find(ids) != ids.end();
				};
				if (has_duplicate(picture.references_l0_) || has_duplicate(picture.references_l1_)) {
					return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT,
											 "H.265 reference lists cannot contain the same frame more than once"));
				}
				std::vector<std::uint64_t> unique_references;
				for (const auto id: picture.references_l0_) { unique_references.push_back(id.value_); }
				for (const auto id: picture.references_l1_) { unique_references.push_back(id.value_); }
				for (const auto id: picture._retained_references) {
					if (!references_.contains(id.value_) ||
						std::ranges::find(unique_references, id.value_) != unique_references.end()) {
						return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT,
													 "invalid unused H.265 reference"));
					}
					unique_references.push_back(id.value_);
				}
				std::ranges::sort(unique_references);
				const auto unique_end = std::ranges::unique(unique_references).begin();
				unique_references.erase(unique_end, unique_references.end());
				if (unique_references.size() > 15U) {
					return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT,
											 "H.265 picture reference union exceeds the 15-picture DPB syntax limit"));
				}
				for (const auto id: unique_references) {
					if (references_.at(id).poc_ == picture.picture_order_count_) {
						return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT,
												 "H.265 current picture and reference pictures must have distinct POCs"));
					}
				}
				if (plan_.stream_.rate_control_ == encode::Rate_Control_Mode::CQP && !picture.cqp_.has_value()) {
					return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT,
											 "CQP H.265 sessions require an explicit picture QP"));
				}
				if (plan_.stream_.rate_control_ != encode::Rate_Control_Mode::CQP && picture.cqp_.has_value()) {
					return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT,
											 "driver-rate-controlled H.265 sessions cannot accept a picture CQP"));
				}
				const auto qp = picture.cqp_.value_or(plan_.rate_control_.initial_qp_);
				if (qp > 51U) {
					return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT, "H.265 picture QP must be in 0..51"));
				}
				if (picture.idr_) { clear_references(); }
				if (free_surfaces_.empty()) {
					return std::unexpected(error(Media_Error_Code::INVALID_REFERENCE_STATE,
											 "H.265 reconstructed-picture pool is exhausted; release a reference"));
				}

				auto input = prepare_input();
				if (!input) { return std::unexpected(input.error()); }
				if (coded_ == VA_INVALID_ID) {
					const auto status = vaCreateBuffer(display_, context_, VAEncCodedBufferType,
						coded_buffer_size(layout.width, layout.height, layout.depth), 1, nullptr, &coded_);
					if (!status_ok(status)) {
						coded_ = VA_INVALID_ID;
						return std::unexpected(error(Media_Error_Code::VAAPI_SUBMISSION_FAILED,
							std::string{"failed to create H.265 coded buffer: "} + vaErrorStr(status)));
					}
				}
				if (!prepared) {
					auto uploaded = upload_frame(display_, input->surface, layout, pixels);
					if (!uploaded) { return std::unexpected(uploaded.error()); }
				}
				VASurfaceID reconstructed = free_surfaces_.back();
				free_surfaces_.pop_back();
				bool reconstructed_released = false;
				const auto release_reconstructed = [&] {
					if (!reconstructed_released) {
						free_surfaces_.push_back(reconstructed);
						reconstructed_released = true;
					}
				};

				std::vector<VABufferID> buffers;
				const auto destroy_buffers = [&] {
					for (const auto buffer: buffers) { (void) vaDestroyBuffer(display_, buffer); }
				};
				const auto create_buffer = [&](VABufferType type, std::uint32_t size, std::uint32_t count,
										 const void *data) -> Result<VABufferID> {
					VABufferID id = VA_INVALID_ID;
					const VAStatus status = vaCreateBuffer(display_, context_, type, size, count,
														const_cast<void *>(data), &id);
					if (!status_ok(status)) {
						return std::unexpected(error(Media_Error_Code::VAAPI_SUBMISSION_FAILED,
												 std::string{"failed to create H.265 VA buffer: "} + vaErrorStr(status)));
					}
					buffers.push_back(id);
					return id;
				};
				const auto create_packed_header = [&](std::uint32_t type,
													  const std::vector<std::uint8_t> &bytes) -> Result<void> {
					VAEncPackedHeaderParameterBuffer parameters{};
					parameters.type = static_cast<VAEncPackedHeaderType>(type);
					parameters.bit_length = static_cast<std::uint32_t>(bytes.size() * 8U);
					parameters.has_emulation_bytes = 1U;
					auto parameter = create_buffer(VAEncPackedHeaderParameterBufferType, sizeof(parameters), 1,
										   &parameters);
					if (!parameter) { return std::unexpected(parameter.error()); }
					auto data = create_buffer(VAEncPackedHeaderDataBufferType, bytes.size(), 1, bytes.data());
					if (!data) { return std::unexpected(data.error()); }
					return {};
				};

				VAEncSequenceParameterBufferHEVC sequence{};
				sequence.general_profile_idc = binding_.public_.screen_content_ ? 9U :
										   (binding_.public_.bit_depth_ > 8U ? 2U : 1U);
				sequence.general_level_idc = plan_.level_idc_;
				sequence.general_tier_flag = plan_.high_tier_ ? 1U : 0U;
				sequence.intra_period       = plan_.intra_period_;
				sequence.intra_idr_period   = plan_.intra_idr_period_;
				sequence.ip_period          = plan_.ip_period_;
				sequence.bits_per_second    = plan_.rate_control_.target_bitrate_;
				sequence.pic_width_in_luma_samples  = static_cast<std::uint16_t>(plan_.stream_.width_);
				sequence.pic_height_in_luma_samples = static_cast<std::uint16_t>(plan_.stream_.height_);
				sequence.seq_fields.bits.chroma_format_idc = chroma_idc(binding_.public_.chroma_);
				sequence.seq_fields.bits.bit_depth_luma_minus8 = binding_.public_.bit_depth_ - 8U;
				sequence.seq_fields.bits.bit_depth_chroma_minus8 = binding_.public_.bit_depth_ - 8U;
				sequence.seq_fields.bits.strong_intra_smoothing_enabled_flag =
						plan_.strong_intra_smoothing_ ? 1U : 0U;
				sequence.seq_fields.bits.amp_enabled_flag = 1U;
				sequence.seq_fields.bits.sample_adaptive_offset_enabled_flag =
						plan_.sample_adaptive_offset_ ? 1U : 0U;
				sequence.seq_fields.bits.sps_temporal_mvp_enabled_flag = plan_.temporal_mvp_ ? 1U : 0U;
				sequence.seq_fields.bits.low_delay_seq = plan_.ip_period_ == 1U;
				sequence.seq_fields.bits.hierachical_flag = plan_.ip_period_ > 1U;
				sequence.log2_min_luma_coding_block_size_minus3 = 0U;
				const auto ctu_size = binding_.public_.coding_tree_block_size_;
				sequence.log2_diff_max_min_luma_coding_block_size = std::bit_width(ctu_size) - 4U;
				sequence.log2_min_transform_block_size_minus2 = 0U;
				sequence.log2_diff_max_min_transform_block_size = 3U;
				sequence.max_transform_hierarchy_depth_inter = 2U;
				sequence.max_transform_hierarchy_depth_intra = 2U;
				sequence.vui_parameters_present_flag = 1U;
				sequence.vui_fields.bits.vui_timing_info_present_flag = 1U;
				sequence.vui_num_units_in_tick = plan_.stream_.framerate_den_;
				sequence.vui_time_scale = plan_.stream_.framerate_num_;
				sequence.scc_fields.bits.palette_mode_enabled_flag = plan_.palette_mode_ ? 1U : 0U;
				auto sequence_buffer = create_buffer(VAEncSequenceParameterBufferType, sizeof(sequence), 1, &sequence);
				auto parameter_sets = packed_vps(plan_, binding_);
				const auto sps = packed_sps(plan_, binding_);
				parameter_sets.insert(parameter_sets.end(), sps.begin(), sps.end());
				const auto pps = packed_pps(plan_, binding_.public_.screen_content_);
				parameter_sets.insert(parameter_sets.end(), pps.begin(), pps.end());
				auto sequence_headers = create_packed_header(VAEncPackedHeaderSequence, parameter_sets);

				VAEncPictureParameterBufferHEVC params{};
				params.decoded_curr_pic.picture_id = reconstructed;
				params.decoded_curr_pic.pic_order_cnt = picture.picture_order_count_;
				params.decoded_curr_pic.flags = 0U;
				for (auto &reference: params.reference_frames) {
					reference.picture_id = VA_INVALID_SURFACE;
					reference.pic_order_cnt = 0;
					reference.flags = VA_PICTURE_HEVC_INVALID;
				}
				std::vector<std::uint64_t> dpb_ids;
				const auto append_dpb = [&](Frame_Id id, std::uint32_t flags) {
					if (std::ranges::find(dpb_ids, id.value_) != dpb_ids.end()) { return; }
					const auto index = dpb_ids.size();
					if (index >= std::size(params.reference_frames)) { return; }
					const auto &stored = references_.at(id.value_);
					params.reference_frames[index].picture_id = stored.surface_;
					params.reference_frames[index].pic_order_cnt = stored.poc_;
					params.reference_frames[index].flags = flags;
					dpb_ids.push_back(id.value_);
				};
				for (const auto id: picture.references_l0_) { append_dpb(id, VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE); }
				for (const auto id: picture.references_l1_) { append_dpb(id, VA_PICTURE_HEVC_RPS_ST_CURR_AFTER); }
				for (const auto id: picture._retained_references) { append_dpb(id, 0U); }
				params.coded_buf = coded_;
				params.collocated_ref_pic_index = plan_.temporal_mvp_ && picture.type_ != H265_Slice_Type::I ? 0U : 0xffU;
				params.pic_init_qp = plan_.rate_control_.initial_qp_; // same base QP as the packed PPS
				params.pps_cb_qp_offset = plan_.chroma_qp_offset_;
				params.pps_cr_qp_offset = plan_.chroma_qp_offset_;
				params.num_ref_idx_l0_default_active_minus1 =
						picture.references_l0_.empty() ? 0U : picture.references_l0_.size() - 1U;
				params.num_ref_idx_l1_default_active_minus1 =
						picture.references_l1_.empty() ? 0U : picture.references_l1_.size() - 1U;
				params.nal_unit_type = nal_unit_type(picture);
				params.pic_fields.bits.idr_pic_flag = picture.idr_ ? 1U : 0U;
				params.pic_fields.bits.coding_type = picture.type_ == H265_Slice_Type::I ? 1U :
													  (picture.type_ == H265_Slice_Type::P ? 2U : 3U);
				params.pic_fields.bits.reference_pic_flag = picture.reference_ ? 1U : 0U;
				if (plan_.ip_period_ > 1U) { params.hierarchical_level_plus1 = picture._pyramid_level + 1U; }
				params.pic_fields.bits.constrained_intra_pred_flag =
						plan_.constrained_intra_prediction_ ? 1U : 0U;
				params.pic_fields.bits.transform_skip_enabled_flag = plan_.transform_skip_ ? 1U : 0U;
				params.pic_fields.bits.cu_qp_delta_enabled_flag =
						plan_.stream_.rate_control_ != encode::Rate_Control_Mode::CQP;
				params.pic_fields.bits.pps_loop_filter_across_slices_enabled_flag = 1U;
				params.pic_fields.bits.screen_content_flag = binding_.public_.screen_content_ ? 1U : 0U;
				params.scc_fields.bits.pps_curr_pic_ref_enabled_flag = plan_.intra_block_copy_ ? 1U : 0U;
				auto picture_buffer = create_buffer(VAEncPictureParameterBufferType, sizeof(params), 1, &params);

				const auto ctu_width  = align_up(plan_.stream_.width_, ctu_size) / ctu_size;
				const auto ctu_height = align_up(plan_.stream_.height_, ctu_size) / ctu_size;
				const auto total_ctus = ctu_width * ctu_height;
				std::vector<VAEncSliceParameterBufferHEVC> slices(plan_.slice_count_);
				std::vector<Packed_Slice_Reference> packed_references;
				for (const auto id: dpb_ids) {
					const Frame_Id frame{.value_ = id};
					packed_references.push_back(Packed_Slice_Reference{
						.frame_ = frame, .poc_ = references_.at(id).poc_,
						._used = std::ranges::find(picture._retained_references, frame) == picture._retained_references.end(),
					});
				}
				std::uint32_t next_ctu = 0U;
				std::optional<Media_Error> slice_buffer_error;
				for (std::uint32_t index = 0; index < slices.size(); ++index) {
					auto &slice = slices[index];
					const auto remaining = total_ctus - next_ctu;
					const auto groups = static_cast<std::uint32_t>(slices.size()) - index;
					slice.slice_segment_address = next_ctu;
					slice.num_ctu_in_slice = (remaining + groups - 1U) / groups;
					next_ctu += slice.num_ctu_in_slice;
					slice.slice_type = static_cast<std::uint8_t>(picture.type_);
					slice.num_ref_idx_l0_active_minus1 = params.num_ref_idx_l0_default_active_minus1;
					slice.num_ref_idx_l1_active_minus1 = params.num_ref_idx_l1_default_active_minus1;
					for (auto &reference: slice.ref_pic_list0) {
						reference.picture_id = VA_INVALID_SURFACE;
						reference.pic_order_cnt = 0;
						reference.flags = VA_PICTURE_HEVC_INVALID;
					}
					for (auto &reference: slice.ref_pic_list1) {
						reference.picture_id = VA_INVALID_SURFACE;
						reference.pic_order_cnt = 0;
						reference.flags = VA_PICTURE_HEVC_INVALID;
					}
					for (std::size_t ref = 0; ref < picture.references_l0_.size(); ++ref) {
						const auto &stored = references_.at(picture.references_l0_[ref].value_);
						slice.ref_pic_list0[ref].picture_id = stored.surface_;
						slice.ref_pic_list0[ref].pic_order_cnt = stored.poc_;
						slice.ref_pic_list0[ref].flags = VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE;
					}
					for (std::size_t ref = 0; ref < picture.references_l1_.size(); ++ref) {
						const auto &stored = references_.at(picture.references_l1_[ref].value_);
						slice.ref_pic_list1[ref].picture_id = stored.surface_;
						slice.ref_pic_list1[ref].pic_order_cnt = stored.poc_;
						slice.ref_pic_list1[ref].flags = VA_PICTURE_HEVC_RPS_ST_CURR_AFTER;
					}
					slice.max_num_merge_cand = 5U;
					slice.slice_qp_delta = static_cast<std::int8_t>(static_cast<std::int32_t>(qp) -
														plan_.rate_control_.initial_qp_);
					slice.slice_fields.bits.last_slice_of_pic_flag = index + 1U == slices.size() ? 1U : 0U;
					slice.slice_fields.bits.slice_temporal_mvp_enabled_flag =
							plan_.temporal_mvp_ && picture.type_ != H265_Slice_Type::I ? 1U : 0U;
					slice.slice_fields.bits.slice_sao_luma_flag = plan_.sample_adaptive_offset_ ? 1U : 0U;
					slice.slice_fields.bits.slice_sao_chroma_flag = plan_.sample_adaptive_offset_ ? 1U : 0U;
					slice.slice_fields.bits.slice_loop_filter_across_slices_enabled_flag = 1U;
					slice.slice_fields.bits.collocated_from_l0_flag =
							picture.type_ == H265_Slice_Type::I ? 0U : 1U;
					const auto header = packed_slice(plan_, picture, packed_references,
													 slice.slice_segment_address, total_ctus, qp);
					auto packed = create_packed_header(VAEncPackedHeaderHEVC_Slice, header);
					if (!packed && !slice_buffer_error.has_value()) { slice_buffer_error = packed.error(); }
					auto created = create_buffer(VAEncSliceParameterBufferType, sizeof(slice), 1, &slice);
					if (!created && !slice_buffer_error.has_value()) { slice_buffer_error = created.error(); }
				}

				std::optional<Media_Error> misc_buffer_error;
				std::vector<std::uint8_t> rc_bytes;
				std::vector<std::uint8_t> fps_bytes;
				if (plan_.stream_.rate_control_ != encode::Rate_Control_Mode::CQP) {
					rc_bytes = misc_parameter(VAEncMiscParameterTypeRateControl, sizeof(VAEncMiscParameterRateControl));
					auto *rc = reinterpret_cast<VAEncMiscParameterRateControl *>(
							reinterpret_cast<VAEncMiscParameterBuffer *>(rc_bytes.data())->data);
					rc->bits_per_second = plan_.rate_control_.maximum_bitrate_ == 0U
											  ? plan_.rate_control_.target_bitrate_
											  : plan_.rate_control_.maximum_bitrate_;
					rc->target_percentage = plan_.rate_control_.target_percentage_;
					rc->initial_qp = plan_.rate_control_.initial_qp_;
					rc->min_qp = plan_.rate_control_.minimum_qp_;
					rc->max_qp = plan_.rate_control_.maximum_qp_;
					rc->ICQ_quality_factor = plan_.rate_control_.initial_qp_;
					rc->quality_factor = plan_.rate_control_.initial_qp_;
					rc->rc_flags.bits.disable_frame_skip = 1;
					auto created_rc = create_buffer(VAEncMiscParameterBufferType, rc_bytes.size(), 1, rc_bytes.data());
					if (!created_rc) { misc_buffer_error = created_rc.error(); }

				}
				fps_bytes = misc_parameter(VAEncMiscParameterTypeFrameRate, sizeof(VAEncMiscParameterFrameRate));
				auto *fps = reinterpret_cast<VAEncMiscParameterFrameRate *>(
						reinterpret_cast<VAEncMiscParameterBuffer *>(fps_bytes.data())->data);
				fps->framerate = (std::min(plan_.stream_.framerate_den_, 0xffffU) << 16U) |
							 std::min(plan_.stream_.framerate_num_, 0xffffU);
				auto created_fps = create_buffer(VAEncMiscParameterBufferType, fps_bytes.size(), 1, fps_bytes.data());
				if (!created_fps && !misc_buffer_error.has_value()) { misc_buffer_error = created_fps.error(); }

				std::optional<VABufferID> quality_buffer;
				std::vector<std::uint8_t> quality_bytes;
				if (plan_.rate_control_.driver_quality_level_ != 0U) {
					quality_bytes = misc_parameter(VAEncMiscParameterTypeQualityLevel,
											 sizeof(VAEncMiscParameterBufferQualityLevel));
					auto *quality = reinterpret_cast<VAEncMiscParameterBufferQualityLevel *>(
							reinterpret_cast<VAEncMiscParameterBuffer *>(quality_bytes.data())->data);
					quality->quality_level = plan_.rate_control_.driver_quality_level_;
					auto created = create_buffer(VAEncMiscParameterBufferType, quality_bytes.size(), 1, quality_bytes.data());
					if (created) { quality_buffer = *created; }
				}
				(void) quality_buffer;
				if (!sequence_buffer || !sequence_headers || !picture_buffer || slice_buffer_error.has_value() ||
					misc_buffer_error.has_value()) {
					Media_Error failure = !sequence_buffer ? sequence_buffer.error() :
									  (!sequence_headers ? sequence_headers.error() :
									   (!picture_buffer ? picture_buffer.error() :
										(slice_buffer_error.has_value() ? *slice_buffer_error : *misc_buffer_error)));
					destroy_buffers();
					release_reconstructed();
					return std::unexpected(std::move(failure));
				}

				VAStatus status = vaBeginPicture(display_, context_, input->surface);
				if (status_ok(status)) {
					status = vaRenderPicture(display_, context_, buffers.data(), static_cast<int>(buffers.size()));
				}
				if (status_ok(status)) { status = vaEndPicture(display_, context_); }
				if (status_ok(status)) { status = vaSyncBuffer(display_, coded_, 2'000'000'000ULL); }
				if (status_ok(status) && prepared) { next_input_ = (next_input_ + 1) % input_surfaces_.size(); }
				if (!status_ok(status)) {
					destroy_buffers();
					discard_input();
					release_reconstructed();
					return std::unexpected(error(Media_Error_Code::VAAPI_SUBMISSION_FAILED,
											 std::string{"failed to encode H.265 picture: "} + vaErrorStr(status)));
				}

				H265_Encoded_Picture output{.frame_ = picture.frame_, .annex_b_ = {}, .reconstruction_ = {}};
				void *mapped = nullptr;
				status = vaMapBuffer(display_, coded_, &mapped);
				if (status_ok(status)) {
					for (auto *segment = static_cast<VACodedBufferSegment *>(mapped); segment != nullptr;
						 segment = static_cast<VACodedBufferSegment *>(segment->next)) {
						constexpr std::uint32_t fatal = VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK |
														VA_CODED_BUF_STATUS_FRAME_SIZE_OVERFLOW |
														VA_CODED_BUF_STATUS_BAD_BITSTREAM;
						if ((segment->status & fatal) != 0U || segment->bit_offset != 0U ||
							segment->size > 16U * 1024U * 1024U - output.annex_b_.size() ||
							(segment->size != 0U && segment->buf == nullptr)) {
							status = VA_STATUS_ERROR_ENCODING_ERROR;
							break;
						}
						if (segment->size == 0U) { continue; }
						const auto *begin = static_cast<const std::uint8_t *>(segment->buf);
						output.annex_b_.insert(output.annex_b_.end(), begin, begin + segment->size);
					}
					const VAStatus unmap = vaUnmapBuffer(display_, coded_);
					if (status_ok(status)) { status = unmap; }
				}
				destroy_buffers();
				if (!status_ok(status) || output.annex_b_.empty()) {
					discard_input();
					release_reconstructed();
					return std::unexpected(error(Media_Error_Code::VAAPI_SUBMISSION_FAILED,
											 std::string{"failed to obtain the encoded H.265 access unit: "} +
													 vaErrorStr(status)));
				}
				if (picture.read_reconstruction_) {
					auto reconstruction = read_reconstruction(display_, reconstructed, layout, binding_.legacy_block_sizes_);
					if (!reconstruction) {
						release_reconstructed();
						return std::unexpected(reconstruction.error());
					}
					output.reconstruction_ = std::move(*reconstruction);
				}
				if (picture.reference_) {
					const auto existing = references_.find(picture.frame_.value_);
					if (existing != references_.end()) {
						free_surfaces_.push_back(existing->second.surface_);
						references_.erase(existing);
					}
					references_.insert_or_assign(picture.frame_.value_, Stored_Reference{
							.surface_ = reconstructed, .poc_ = picture.picture_order_count_});
				} else {
					release_reconstructed();
				}
				return output;
			}

			[[nodiscard]] Result<void> release_reference(Frame_Id frame) override {
				const auto found = references_.find(frame.value_);
				if (found == references_.end()) {
					return std::unexpected(error(Media_Error_Code::INVALID_REFERENCE_STATE,
											 "cannot release an H.265 reference which is not resident"));
				}
				free_surfaces_.push_back(found->second.surface_);
				references_.erase(found);
				return {};
			}

		private:
			void discard_input() {
				if (coded_ != VA_INVALID_ID) {
					(void) vaDestroyBuffer(display_, std::exchange(coded_, VA_INVALID_ID));
				}
				for (auto &slot : input_surfaces_) {
					if (slot != VA_INVALID_SURFACE) {
						auto surface = std::exchange(slot, VA_INVALID_SURFACE);
						(void) vaDestroySurfaces(display_, &surface, 1);
					}
				}
				owner_ = new_surface_owner();
				next_input_ = 0;
			}
			struct Stored_Reference {
				VASurfaceID  surface_ = VA_INVALID_SURFACE;
				std::int32_t poc_     = 0;
			};

			void clear_references() {
				for (const auto &[id, reference]: references_) {
					(void) id;
					free_surfaces_.push_back(reference.surface_);
				}
				references_.clear();
			}

			H265_Encode_Plan plan_;
			Mode_Binding binding_;
			std::shared_ptr<Va_Display> display_owner_;
			VADisplay display_ = nullptr;
			VAConfigID config_ = VA_INVALID_ID;
			VAContextID context_ = VA_INVALID_ID;
			std::array<VASurfaceID, 5> input_surfaces_{
				VA_INVALID_SURFACE, VA_INVALID_SURFACE, VA_INVALID_SURFACE, VA_INVALID_SURFACE, VA_INVALID_SURFACE,
			};
			std::size_t next_input_ = 0;
			std::uint64_t owner_ = new_surface_owner();
			VABufferID coded_ = VA_INVALID_ID;
			std::vector<VASurfaceID> surfaces_;
			std::vector<VASurfaceID> free_surfaces_;
			std::unordered_map<std::uint64_t, Stored_Reference> references_;
		};
	} // namespace

	Result<Capability_Snapshot> query_encode_plan_restrictions(const std::filesystem::path &render_node) {
		thread_local std::unordered_map<std::filesystem::path, Capability_Snapshot> snapshots;
		if (const auto found = snapshots.find(render_node); found != snapshots.end()) { return found->second; }
		auto display = Va_Display::open(render_node);
		if (!display) { return std::unexpected(display.error()); }
		auto result = query((*display)->get());
		if (result) { snapshots.emplace(render_node, *result); }
		return result;
	}

	Result<std::unique_ptr<Va_Encode_Context>>
	create_encode_context(const H265_Encode_Plan &plan, const Capability_Snapshot &snapshot,
					  const std::filesystem::path &render_node, std::optional<std::uint32_t> reference_limit) {
		if (plan.stream_.restrictions_identity_ != snapshot.public_.identity_) {
			return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT,
								 "H.265 encode plan was not selected from this restriction snapshot"));
		}
		const auto binding = std::ranges::find_if(snapshot.modes_, [&](const Mode_Binding &candidate) {
			return candidate.public_.id_ == plan.stream_.mode_;
		});
		if (binding == snapshot.modes_.end()) {
			return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT,
								 "H.265 encode plan selects an unknown restriction mode"));
		}
		if (binding->public_.implemented_source_formats_.empty() ||
			std::ranges::find(binding->public_.implemented_source_formats_, plan.stream_.source_format_) ==
					binding->public_.implemented_source_formats_.end()) {
			return std::unexpected(error(Media_Error_Code::UNSUPPORTED_DRIVER_FEATURE,
								 "selected H.265 mode has no implemented source conversion/import path"));
		}
		if (!encode::contains_rate_control(binding->public_.rate_control_, plan.stream_.rate_control_)) {
			return std::unexpected(error(Media_Error_Code::UNSUPPORTED_DRIVER_FEATURE,
								 "selected H.265 mode does not expose the requested rate-control mode"));
		}
		if (plan.stream_.width_ == 0U || plan.stream_.height_ == 0U || plan.stream_.framerate_num_ == 0U ||
			plan.stream_.framerate_den_ == 0U ||
			plan.stream_.width_ > binding->public_.dimensions_.maximum_width_ ||
			plan.stream_.height_ > binding->public_.dimensions_.maximum_height_ ||
			plan.stream_.width_ % binding->public_.dimensions_.width_alignment_ != 0U ||
			plan.stream_.height_ % binding->public_.dimensions_.height_alignment_ != 0U) {
			return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT,
								 "H.265 stream dimensions or cadence violate the selected mode restrictions"));
		}
		if (plan.stream_.width_ > std::numeric_limits<std::uint16_t>::max() ||
			plan.stream_.height_ > std::numeric_limits<std::uint16_t>::max()) {
			return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT,
								 "H.265 VA sequence dimensions exceed the public 16-bit field"));
		}
		if (plan.chroma_qp_offset_ < -12 || plan.chroma_qp_offset_ > 12) {
			return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT,
								 "H.265 chroma QP offset must be within -12..12"));
		}
		if (plan.slice_count_ == 0U || plan.slice_count_ > binding->public_.max_slices_) {
			return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT,
								 "H.265 slice count violates the selected mode restrictions"));
		}
		const auto ctu_rows = (plan.stream_.height_ + binding->public_.coding_tree_block_size_ - 1U) /
				binding->public_.coding_tree_block_size_;
		if (plan.slice_count_ > 1U && (!binding->public_.slices_equal_ctu_rows_ ||
			ctu_rows % plan.slice_count_ != 0U)) {
			return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT,
								 "H.265 slice count must partition this mode into equal CTU rows"));
		}
		const bool color_is_420 = plan.output_color_.subsampling_x_ && plan.output_color_.subsampling_y_;
		const bool color_is_422 = plan.output_color_.subsampling_x_ && !plan.output_color_.subsampling_y_;
		const bool color_is_444 = !plan.output_color_.subsampling_x_ && !plan.output_color_.subsampling_y_;
		const bool chroma_matches =
				(binding->public_.chroma_ == encode::Chroma_Layout::YUV420 && color_is_420) ||
				(binding->public_.chroma_ == encode::Chroma_Layout::YUV422 && color_is_422) ||
				(binding->public_.chroma_ == encode::Chroma_Layout::YUV444 && color_is_444);
		if (plan.output_color_.monochrome_ || plan.output_color_.bit_depth_ != binding->public_.bit_depth_ ||
			!chroma_matches) {
			return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT,
								 "H.265 output color encoding does not match the selected exact mode"));
		}
		if (plan.stream_.framerate_num_ > 0xffffU || plan.stream_.framerate_den_ > 0xffffU) {
			return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT,
								 "H.265 VA frame-rate rational components must fit in 16 bits"));
		}
		const auto level = std::ranges::find_if(binding->public_.levels_, [&](const auto &candidate) {
			return candidate.level_value_ == plan.level_idc_;
		});
		const std::uint64_t picture_samples = static_cast<std::uint64_t>(plan.stream_.width_) * plan.stream_.height_;
		const std::uint64_t sample_rate = picture_samples * plan.stream_.framerate_num_ / plan.stream_.framerate_den_;
		const std::uint64_t requested_bitrate = std::max(plan.rate_control_.target_bitrate_,
															  plan.rate_control_.maximum_bitrate_);
		if (level == binding->public_.levels_.end() || picture_samples > level->maximum_picture_samples_ ||
			sample_rate > level->maximum_sample_rate_ ||
			(plan.high_tier_ && level->maximum_high_tier_bitrate_ == 0U) ||
			(requested_bitrate != 0U && requested_bitrate >
					(plan.high_tier_ ? level->maximum_high_tier_bitrate_ : level->maximum_main_tier_bitrate_))) {
			return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT,
								 "H.265 resolution, cadence, tier, or bitrate exceeds the selected level restriction"));
		}
		if (plan.ip_period_ == 0U || plan.picture_order_count_lsb_bits_ < 4U ||
			plan.picture_order_count_lsb_bits_ > 16U || plan.rate_control_.initial_qp_ > 51U ||
			plan.rate_control_.minimum_qp_ > plan.rate_control_.maximum_qp_ ||
			plan.rate_control_.maximum_qp_ > 51U || plan.rate_control_.target_percentage_ > 100U) {
			return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT,
								 "H.265 sequence or rate-control fields are outside their legal domains"));
		}
		if (plan._decoded_picture_buffer_size > 16U ||
			(plan._decoded_picture_buffer_size != 0U && plan._decoded_picture_buffer_size < plan.ip_period_)) {
			return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT, "invalid H.265 decoded picture buffer size"));
		}
		if (plan.stream_.rate_control_ != encode::Rate_Control_Mode::CQP &&
			plan.stream_.rate_control_ != encode::Rate_Control_Mode::ICQ &&
			plan.rate_control_.target_bitrate_ == 0U) {
			return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT,
								 "driver-rate-controlled H.265 plans require a target bitrate"));
		}
		if ((plan.palette_mode_ || plan.intra_block_copy_) && !binding->public_.screen_content_) {
			return std::unexpected(error(Media_Error_Code::INVALID_ARGUMENT,
								 "H.265 palette or intra-block-copy tools require an SCC mode"));
		}
		auto display = Va_Display::open(render_node);
		if (!display) { return std::unexpected(display.error()); }
		std::array<VAConfigAttrib, 3> attributes{{
				{.type = VAConfigAttribRTFormat, .value = binding->va_rt_format_},
				{.type = VAConfigAttribRateControl, .value = va_rate_control(plan.stream_.rate_control_)},
				{.type = VAConfigAttribEncPackedHeaders,
				 .value = VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_SLICE},
		}};
		VAConfigID config = VA_INVALID_ID;
		VAStatus status = vaCreateConfig((*display)->get(), static_cast<VAProfile>(binding->va_profile_),
									 static_cast<VAEntrypoint>(binding->va_entrypoint_), attributes.data(),
									 attributes.size(), &config);
		if (!status_ok(status)) {
			return std::unexpected(error(Media_Error_Code::UNSUPPORTED_DRIVER_FEATURE,
								 std::string{"failed to create H.265 VA encode config: "} + vaErrorStr(status)));
		}
		const auto context_width = align_up(plan.stream_.width_, 64U);
		const auto context_height = align_up(plan.stream_.height_, 64U);
		auto mode = *binding;
		if (reference_limit) {
			mode.public_.max_reference_l0_ = std::min(mode.public_.max_reference_l0_, *reference_limit);
			mode.public_.max_reference_l1_ = 0;
		}
		const auto surface_count = plan.intra_idr_period_ == 1U ? 1U : reference_limit
			? *reference_limit + 1 : plan._decoded_picture_buffer_size != 0U
			? plan._decoded_picture_buffer_size
			: std::max<std::uint32_t>(mode.public_.max_reference_l0_ + mode.public_.max_reference_l1_ + 2U, 4U);
		std::vector<VASurfaceID> surfaces;
		const Frame_Layout recon_layout{
			.width = context_width, .height = context_height, .depth = binding->public_.bit_depth_,
			.chroma = chroma_idc(binding->public_.chroma_) == 1U ? 420U :
					  chroma_idc(binding->public_.chroma_) == 2U ? 422U : 444U,
		};
		for (std::uint32_t i = 0; i < surface_count; ++i) {
			auto surface = create_surface((*display)->get(), recon_layout, VA_SURFACE_ATTRIB_USAGE_HINT_ENCODER);
			if (!surface) {
				if (!surfaces.empty()) { (void) vaDestroySurfaces((*display)->get(), surfaces.data(), static_cast<int>(surfaces.size())); }
				(void) vaDestroyConfig((*display)->get(), config);
				return std::unexpected(surface.error());
			}
			surfaces.push_back(*surface);
		}
		VAContextID context = VA_INVALID_ID;
		status = vaCreateContext((*display)->get(), config, static_cast<int>(context_width), static_cast<int>(context_height),
								 VA_PROGRESSIVE, surfaces.data(), static_cast<int>(surfaces.size()), &context);
		if (!status_ok(status)) {
			(void) vaDestroySurfaces((*display)->get(), surfaces.data(), static_cast<int>(surfaces.size()));
			(void) vaDestroyConfig((*display)->get(), config);
			return std::unexpected(error(Media_Error_Code::VAAPI_SUBMISSION_FAILED,
								 std::string{"failed to create H.265 VA encode context: "} + vaErrorStr(status)));
		}
		return std::make_unique<Vaapi_H265_Encode_Context>(plan, std::move(mode), std::move(*display), config, context,
																  std::move(surfaces));
	}
} // namespace video_editor::h265::encoder
