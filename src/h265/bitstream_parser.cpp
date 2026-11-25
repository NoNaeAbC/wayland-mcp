#include <cstdint>
#include "h265/decoder_internal.hpp"
#include "video_editor/media/annex_b.hpp"
#include "h265/rbsp_reader.hpp"
#include "video_editor/color/hdr10plus_t35.hpp"

#include <va/va.h>

#include <algorithm>
#include <array>
#include <cstdlib>
#include <iostream>
#include <limits>
#include <memory>
#include <optional>
#include <span>
#include <string>
#include <utility>
#include <vector>

namespace video_editor::h265::decoder {
	namespace {
		constexpr std::size_t kVirtual_Dpb_Slot_Count = 16U;

		[[nodiscard]] Media_Error h265_error(std::uint64_t byte_offset, std::uint64_t bit_offset, std::string message) {
			return Media_Error{
					.code_        = Media_Error_Code::INVALID_H265_BITSTREAM,
					.byte_offset_ = byte_offset,
					.bit_offset_  = bit_offset,
					.message_     = std::move(message),
			};
		}

		[[nodiscard]] Media_Error syntax_error(std::uint64_t offset, const Syntax_Error &failure) {
			const auto byte = offset + failure.bit / 8;
			auto detail = std::string(failure.detail);
			if (failure.field) {
				detail += " in " + std::string(failure.field);
			}
			detail += " at coded byte " + std::to_string(byte) + ", bit " + std::to_string(failure.bit % 8);
			auto error = h265_error(byte, byte > UINT64_MAX / 8 ? UINT64_MAX : byte * 8 + failure.bit % 8,
					std::move(detail));
			if (failure.unsupported) {
				error.code_ = Media_Error_Code::UNSUPPORTED_BITSTREAM_FEATURE;
			}
			return error;
		}

		[[nodiscard]] bool is_vcl_type(Nal_Unit_Type type) {
			return type <= Nal_Unit_Type::RASL_R ||
				   (type >= Nal_Unit_Type::BLA_W_LP && type <= Nal_Unit_Type::CRA);
		}

		[[nodiscard]] bool is_rasl_or_radl(Nal_Unit_Type type) {
			return type >= Nal_Unit_Type::RADL_N && type <= Nal_Unit_Type::RASL_R;
		}

		[[nodiscard]] bool is_non_reference_vcl(Nal_Unit_Type type) {
			return is_vcl_type(type) && type < Nal_Unit_Type::BLA_W_LP &&
					(static_cast<unsigned>(type) & 1U) == 0;
		}

		void inherit_dependent_slice_header(Slice_Header &slice, const std::optional<Slice_Header> &independent) {
			if (slice.dependent_slice_segment_flag == 0U || !independent) { return; }

			const auto segment = slice;
			slice = *independent;
			slice.first_slice_segment_in_pic_flag = segment.first_slice_segment_in_pic_flag;
			slice.dependent_slice_segment_flag = segment.dependent_slice_segment_flag;
			slice.no_output_of_prior_pics_flag = segment.no_output_of_prior_pics_flag;
			slice.segment_address = segment.segment_address;
			slice.header_bytes = segment.header_bytes;
			slice.n_emulation_prevention_bytes = segment.n_emulation_prevention_bytes;
			slice.num_entry_point_offsets = segment.num_entry_point_offsets;
			slice.entry_point_offsets = segment.entry_point_offsets;
		}

		[[nodiscard]] std::shared_ptr<H265_Parameter_Snapshot> snapshot_parameter_sets(const Slice_Header &slice) {
			auto snapshot  = std::make_shared<H265_Parameter_Snapshot>();
			snapshot->pps_ = *slice.pps;
			snapshot->sps_ = *slice.pps->sps;
			if (slice.pps->sps->vps != nullptr) {
				snapshot->vps_     = *slice.pps->sps->vps;
				snapshot->sps_.vps = &snapshot->vps_;
			} else {
				snapshot->sps_.vps = nullptr;
			}
			snapshot->pps_.sps = &snapshot->sps_;
			return snapshot;
		}

		[[nodiscard]] H265_Slice_Type to_internal_slice_type(std::uint8_t type) {
			switch (type) {
				case kSlice_B:
					return H265_Slice_Type::B;
				case kSlice_P:
					return H265_Slice_Type::P;
				case kSlice_I:
					return H265_Slice_Type::I;
				default:
					return H265_Slice_Type::I;
			}
		}

		struct Field_Picture_Info {
			bool field_picture_ = false;
			bool bottom_field_  = false;
		};

		[[nodiscard]] std::optional<Field_Picture_Info> field_picture_info_from_pic_struct(std::uint8_t pic_struct) {
			switch (pic_struct) {
				case 1:
				case 9:
				case 11:
					return Field_Picture_Info{.field_picture_ = true, .bottom_field_ = false};
				case 2:
				case 10:
				case 12:
					return Field_Picture_Info{.field_picture_ = true, .bottom_field_ = true};
				default:
					return Field_Picture_Info{};
			}
		}

		[[nodiscard]] Field_Picture_Info field_picture_info_for_slice(const Slice_Header &slice,
																	  std::optional<std::uint8_t>  pic_struct) {
			if (pic_struct) { return field_picture_info_from_pic_struct(*pic_struct).value_or(Field_Picture_Info{}); }
			const auto *pps = slice.pps;
			const auto *sps = pps == nullptr ? nullptr : pps->sps;
			if (sps != nullptr && sps->vui_params.field_seq_flag != 0U) {
				return Field_Picture_Info{.field_picture_ = true, .bottom_field_ = false};
			}
			return {};
		}

		struct Sei_Info {
			std::optional<color::Hdr10_Plus> hdr10_plus_;
			std::optional<std::uint8_t> pic_struct_;
			std::optional<av1::Mastering_Display_Color_Volume> mastering_display_;
			std::optional<av1::Content_Light_Level> content_light_;
		};

		[[nodiscard]] Sei_Info sei_info(const Sequence_Parameter_Set *sps, const Nal_View &nal) {
			Sei_Info info;
			const bool complete = color::visit_hdr_sei(nal.payload(), [&](std::size_t type,
					std::span<const std::uint8_t> payload) {
				// H.265 Annex D payload types 1, 4, 137 and 144.
				const auto be16 = [&](std::size_t i) { return (std::uint32_t{payload[i]} << 8) | payload[i + 1]; };
				const auto be32 = [&](std::size_t i) { return (be16(i) << 16) | be16(i + 2); };
				switch (type) {
					case 1:
						if (sps && sps->vui_params.frame_field_info_present_flag && !payload.empty()) {
							info.pic_struct_ = payload[0] >> 4;
						}
						break;
					case 4: {
						constexpr std::array<std::uint8_t, 6> signature{0xb5, 0, 0x3c, 0, 1, 4};
						if (payload.size() > signature.size() && std::ranges::equal(payload.first(signature.size()), signature)) {
							const auto app4 = color::parse_app4_registered_t35(payload[0], payload.subspan(1));
							info.hdr10_plus_ = app4 ? app4->preview : color::Hdr10_Plus{};
						}
						break;
					}
					case 137:
						if (payload.size() >= 24) {
							const auto point = [&](std::size_t i) {
								return av1::Color_Chromaticity{.x_ = be16(i) / 50000.0, .y_ = be16(i + 2) / 50000.0};
							};
							info.mastering_display_ = av1::Mastering_Display_Color_Volume{
								.red_ = point(8), .green_ = point(0), .blue_ = point(4), .white_ = point(12),
								.min_luminance_ = be32(20) / 10000.0, .max_luminance_ = be32(16) / 10000.0,
							};
						}
						break;
					case 144:
						if (payload.size() >= 4) {
							info.content_light_ = av1::Content_Light_Level{static_cast<std::uint16_t>(be16(0)),
																		   static_cast<std::uint16_t>(be16(2))};
						}
						break;
					default: break;
				}
			});
			if (!complete) { return Sei_Info{.hdr10_plus_ = {}, .pic_struct_ = info.pic_struct_, .mastering_display_ = {}, .content_light_ = {}}; }
			return info;
		}

		[[nodiscard]] bool profile_compatible(const Profile_Tier_Level &ptl, std::uint8_t profile_idc) {
			return ptl.profile_idc == profile_idc || (profile_idc < std::size(ptl.profile_compatibility_flag) &&
													  ptl.profile_compatibility_flag[profile_idc] != 0U);
		}

		[[nodiscard]] H265_Profile to_internal_profile(const Sequence_Parameter_Set &sps) {
			const auto &ptl       = sps.profile_tier_level;
			const auto  bit_depth = static_cast<std::uint8_t>(sps.bit_depth_luma_minus8 + 8U);
			// Vulkan Video has no StdVideoH265ProfileIdc value for the High
			// Throughput profiles (IDC 5).  It is only valid to use the Range
			// Extensions profile when the bitstream explicitly declares itself
			// compatible with IDC 4 as well.
			if (profile_compatible(ptl, kProfile_High_Throughput) &&
				!profile_compatible(ptl, kProfile_Range_Extension)) {
				return H265_Profile::UNSUPPORTED_EXTENSION;
			}
			if (profile_compatible(ptl, kProfile_Screen_Content) || sps.sps_scc_extension_flag) {
				if (sps.chroma_format_idc == 3U) {
					return bit_depth > 8U ? H265_Profile::SCC_MAIN_444_10 : H265_Profile::SCC_MAIN_444;
				}
				return bit_depth > 8U ? H265_Profile::SCC_MAIN_10 : H265_Profile::SCC_MAIN;
			}
			if (profile_compatible(ptl, kProfile_Main_Still_Picture)) {
				return H265_Profile::MAIN_STILL_PICTURE;
			}
			if (sps.chroma_format_idc == 2U) {
				return bit_depth > 10U ? H265_Profile::MAIN_422_12 : H265_Profile::MAIN_422_10;
			}
			if (sps.chroma_format_idc == 3U) {
				if (bit_depth > 10U) { return H265_Profile::MAIN_444_12; }
				return bit_depth > 8U ? H265_Profile::MAIN_444_10 : H265_Profile::MAIN_444;
			}
			if (bit_depth > 10U) { return H265_Profile::MAIN_12; }
			if (bit_depth > 8U) { return H265_Profile::MAIN_10; }
			if (profile_compatible(ptl, kProfile_Main) ||
				profile_compatible(ptl, kProfile_Range_Extension) ||
				profile_compatible(ptl, kProfile_High_Throughput) || sps.sps_range_extension_flag != 0U) {
				return H265_Profile::MAIN;
			}
			if (sps.chroma_format_idc == 1U) { return H265_Profile::MAIN; }
			return H265_Profile::UNSUPPORTED_EXTENSION;
		}

		[[nodiscard]] bool is_range_extension_profile(const Sequence_Parameter_Set &sps) {
			const auto &ptl = sps.profile_tier_level;
			return profile_compatible(ptl, kProfile_Range_Extension) ||
				   profile_compatible(ptl, kProfile_High_Throughput);
		}

		[[nodiscard]] Chroma_Sample_Position h265_chroma_sample_position(std::uint8_t value) {
			switch (value) {
				case 0U:
					return Chroma_Sample_Position::COSITED;
				case 1U:
					return Chroma_Sample_Position::MIDPOINT;
				default:
					return Chroma_Sample_Position::UNKNOWN;
			}
		}

		[[nodiscard]] Color_Encoding color_from_sps(const Sequence_Parameter_Set &sps) {
			Color_Encoding color;
			color.bit_depth_              = static_cast<std::uint8_t>(sps.bit_depth_luma_minus8 + 8U);
			color.range_                  = Color_Range::LIMITED;
			color.matrix_                 = Matrix_Coefficients::UNSPECIFIED;
			color.primaries_              = Color_Primaries::UNSPECIFIED;
			color.transfer_               = Transfer_Function::UNSPECIFIED;
			color.chroma_sample_position_ = Chroma_Sample_Position::UNKNOWN;
			switch (static_cast<H265_Chroma_Format>(sps.chroma_format_idc)) {
				case H265_Chroma_Format::MONOCHROME:
					color.monochrome_    = true;
					color.subsampling_x_ = true;
					color.subsampling_y_ = true;
					break;
				case H265_Chroma_Format::YUV420:
					color.subsampling_x_ = true;
					color.subsampling_y_ = true;
					break;
				case H265_Chroma_Format::YUV422:
					color.subsampling_x_ = true;
					color.subsampling_y_ = false;
					break;
				case H265_Chroma_Format::YUV444:
					color.subsampling_x_ = false;
					color.subsampling_y_ = false;
					break;
			}

			if (sps.vui_parameters_present_flag != 0U) {
				const auto &vui = sps.vui_params;
				color.range_    = vui.video_full_range_flag != 0U ? Color_Range::FULL : Color_Range::LIMITED;
				if (vui.colour_description_present_flag != 0U) {
					color.primaries_ = av1::color_primaries_from_h273_code(vui.colour_primaries);
					color.transfer_  = av1::transfer_function_from_h273_code(vui.transfer_characteristics);
					color.matrix_    = av1::matrix_coefficients_from_h273_code(vui.matrix_coefficients);
				}
				if (vui.chroma_loc_info_present_flag != 0U) {
					color.chroma_sample_position_ = h265_chroma_sample_position(vui.chroma_sample_loc_type_top_field);
				}
			}
			return color;
		}

		[[nodiscard]] H265_Sequence_Info sequence_from_sps(const Sequence_Parameter_Set &sps) {
			const auto profile = to_internal_profile(sps);
			const auto color   = color_from_sps(sps);
			return H265_Sequence_Info{
					.profile_          = profile,
					.max_width_        = sps.pic_width_in_luma_samples,
					.max_height_       = sps.pic_height_in_luma_samples,
					.bit_depth_        = color.bit_depth_,
					.chroma_bit_depth_ = static_cast<std::uint8_t>(sps.bit_depth_chroma_minus8 + 8U),
					.chroma_format_    = static_cast<H265_Chroma_Format>(sps.chroma_format_idc),
					.range_extension_  = sps.sps_range_extension_flag != 0U || is_range_extension_profile(sps),
					.screen_content_ = sps.sps_scc_extension_flag != 0U || profile == H265_Profile::SCC_MAIN ||
									   profile == H265_Profile::SCC_MAIN_10 || profile == H265_Profile::SCC_MAIN_444 ||
									   profile == H265_Profile::SCC_MAIN_444_10,
					.high_precision_offsets_ = sps.sps_extension_params.high_precision_offsets_enabled_flag != 0U,
					.extended_precision_processing_ = sps.sps_extension_params.extended_precision_processing_flag != 0U,
					.color_                         = color,
			};
		}

		struct Indexed_Nal {
			Nal_View nalu_{};
			std::uint64_t logical_start_code_ = 0;
			std::uint64_t logical_nal_ = 0;
			std::uint64_t logical_end_ = 0;
			bool configuration_ = false;
		};

		[[nodiscard]] Result<std::vector<Indexed_Nal>> identify_annex_b_nals(
				std::span<const std::uint8_t> bitstream) {
			std::vector<Indexed_Nal> result;
			for (std::uint64_t offset = 0; offset < bitstream.size();) {
				const auto code = media::find_annex_b_start_code(bitstream, offset);
				if (!code) {
					if (std::ranges::all_of(bitstream.subspan(offset), [](auto byte) { return byte == 0; })) {
						break;
					}
					return std::unexpected(h265_error(offset, offset * 8, "H.265 elementary stream must use Annex B start codes"));
				}
				const auto next = media::find_annex_b_start_code(bitstream, code->payload_);
				const auto physical_end = next ? next->begin_ : bitstream.size();
				auto end = physical_end;
				if (next) {
					while (end > code->payload_ && bitstream[end - 1] == 0) {
						--end;
					}
				}
				const auto nal = parse_nal_header(bitstream.subspan(code->payload_, end - code->payload_));
				if (!nal) {
					return std::unexpected(syntax_error(code->payload_, nal.error()));
				}
				result.push_back({*nal, code->begin_, code->payload_, end, false});
				offset = physical_end;
			}
			return result;
		}

		[[nodiscard]] Result<void> append_hevc_nals(
				std::span<const std::uint8_t> bytes, std::uint8_t length_size, std::uint64_t logical_begin,
				std::vector<Indexed_Nal> &output, bool configuration = false) {
			if (logical_begin > UINT64_MAX - bytes.size()) {
				return std::unexpected(h265_error(logical_begin, 0, "HEVC sample file position overflows 64 bits"));
			}
			for (std::uint64_t offset = 0; offset < bytes.size();) {
				if (bytes.size() - offset < length_size) {
					return std::unexpected(h265_error(logical_begin + offset, 0, "Truncated HEVC NAL length"));
				}
				std::uint64_t size = 0;
				for (std::uint8_t i = 0; i < length_size; ++i) {
					size = (size << 8) | bytes[offset + i];
				}
				const auto begin = offset + length_size;
				if (size > bytes.size() - begin) {
					return std::unexpected(
							h265_error(logical_begin + offset, 0, "HEVC NAL length exceeds its mapped sample"));
				}
				const auto nal = parse_nal_header(bytes.subspan(begin, size));
				if (!nal) {
					return std::unexpected(syntax_error(logical_begin + begin, nal.error()));
				}
				offset = begin + size;
				output.push_back({*nal, logical_begin + begin - length_size,
					logical_begin + begin, logical_begin + offset, configuration});
			}
			return {};
		}

		void mark_unsupported(Parsed_Bitstream &parsed, std::uint64_t offset, std::string message) {
			if (parsed.requires_unsupported_) { return; }
			parsed.requires_unsupported_ = true;
			parsed.unsupported_offset_   = offset;
			parsed.unsupported_message_  = std::move(message);
		}

		[[nodiscard]] H265_Frame_Decode_Metadata make_frame_metadata(std::uint64_t begin, std::uint64_t end,
																	 H265_Slice_Type slice_type, bool irap) {
			H265_Frame_Decode_Metadata metadata;
			metadata.bitstream_          = Encoded_Byte_Interval{.begin_ = begin, .size_ = end - begin};
			metadata.primary_slice_type_ = slice_type;
			metadata.irap_               = irap;
			return metadata;
		}

		void copy_visible_rect_from_sps(H265_Frame_Decode_Metadata &metadata, const Sequence_Parameter_Set *sps) {
			if (sps == nullptr) { return; }
			metadata.visible_x_      = sps->crop_rect_x;
			metadata.visible_y_      = sps->crop_rect_y;
			metadata.visible_width_  = sps->crop_rect_width;
			metadata.visible_height_ = sps->crop_rect_height;
			if (metadata.visible_width_ == 0U || metadata.visible_height_ == 0U) {
				metadata.visible_x_      = 0;
				metadata.visible_y_      = 0;
				metadata.visible_width_  = sps->pic_width_in_luma_samples;
				metadata.visible_height_ = sps->pic_height_in_luma_samples;
			}
		}

		struct Decoded_Frame_State {
			std::uint32_t frame_index_       = 0;
			std::int32_t  pic_order_cnt_     = 0;
			std::uint16_t pic_order_cnt_lsb_ = 0;
			bool          non_reference_     = false;
			bool          field_picture_     = false;
			bool          bottom_field_      = false;
		};

		struct Reference_Target {
			std::int32_t  pic_order_cnt_     = 0;
			std::uint16_t pic_order_cnt_lsb_ = 0;
			bool          exact_poc_         = true;
			bool          long_term_         = false;
			std::uint32_t va_flags_          = 0;
			bool          used_by_current_   = true;
		};

		struct Decoded_Reference_Lookup {
			std::optional<Decoded_Frame_State> frame_;
			bool                               ambiguous_lsb_ = false;
		};

		struct Virtual_Dpb_Entry {
			bool          allocated_     = false;
			bool          unavailable_   = false;
			bool          short_ref_     = false;
			bool          long_ref_      = false;
			bool          output_        = false;
			bool          field_picture_ = false;
			bool          bottom_field_  = false;
			std::uint32_t frame_index_   = kInvalid_H265_Frame_Index;
			std::int32_t  pic_order_cnt_ = 0;
			std::uint16_t pic_order_lsb_ = 0;
			std::uint32_t va_flags_      = 0;
		};

		struct Dpb_Reference_Lookup {
			std::optional<std::uint8_t> slot_;
			bool                        ambiguous_lsb_ = false;
		};

		struct Virtual_Dpb_State {
			std::array<Virtual_Dpb_Entry, kVirtual_Dpb_Slot_Count> slots_{};
			std::optional<std::uint32_t>                           open_frame_;
			std::optional<std::uint8_t>                            open_slot_;

			[[nodiscard]] std::optional<std::uint8_t> first_free_slot() const {
				for (std::uint8_t slot = 0; slot < slots_.size(); ++slot) {
					if (!slots_[slot].allocated_) { return slot; }
				}
				return std::nullopt;
			}

			[[nodiscard]] std::optional<std::uint8_t> first_reclaimable_slot() const {
				std::optional<std::uint8_t> oldest_slot;
				std::int32_t                oldest_poc = std::numeric_limits<std::int32_t>::max();
				for (std::uint8_t slot = 0; slot < slots_.size(); ++slot) {
					const auto &entry = slots_[slot];
					if (!entry.allocated_) { continue; }
					if (open_slot_ && slot == *open_slot_) { continue; }
					if (entry.short_ref_ || entry.long_ref_ || entry.output_) { continue; }
					if (!oldest_slot || entry.pic_order_cnt_ < oldest_poc) {
						oldest_slot = slot;
						oldest_poc  = entry.pic_order_cnt_;
					}
				}
				return oldest_slot;
			}

			[[nodiscard]] std::optional<std::uint8_t> first_reclaimable_reference_slot() const {
				std::optional<std::uint8_t> oldest_slot;
				std::int32_t                oldest_poc = std::numeric_limits<std::int32_t>::max();
				for (std::uint8_t slot = 0; slot < slots_.size(); ++slot) {
					const auto &entry = slots_[slot];
					if (!entry.allocated_) { continue; }
					if (open_slot_ && slot == *open_slot_) { continue; }
					if (!(entry.short_ref_ || entry.long_ref_) || entry.output_ || entry.unavailable_) { continue; }
					if (!oldest_slot || entry.pic_order_cnt_ < oldest_poc) {
						oldest_slot = slot;
						oldest_poc  = entry.pic_order_cnt_;
					}
				}
				return oldest_slot;
			}

			[[nodiscard]] std::optional<std::uint8_t> allocate_current(std::uint32_t                frame_index,
																	   const Frame_Reference_State &state) {
				auto slot = first_free_slot();
				if (!slot) { slot = first_reclaimable_slot(); }
				if (!slot) { slot = first_reclaimable_reference_slot(); }
				if (!slot) { return std::nullopt; }
				auto &entry          = slots_[*slot];
				entry                = {};
				entry.allocated_     = true;
				entry.frame_index_   = frame_index;
				entry.pic_order_cnt_ = state.pic_order_cnt_;
				entry.pic_order_lsb_ = state.pic_order_cnt_lsb_;
				entry.output_        = state.output_;
				entry.short_ref_     = !state.non_reference_;
				entry.field_picture_ = state.field_picture_;
				entry.bottom_field_  = state.bottom_field_;
				open_frame_          = frame_index;
				open_slot_           = *slot;
				return slot;
			}

			[[nodiscard]] std::optional<std::uint8_t> allocate_decoded_reference(const Decoded_Frame_State &frame) {
				auto slot = first_free_slot();
				if (!slot) { slot = first_reclaimable_slot(); }
				if (!slot) { slot = first_reclaimable_reference_slot(); }
				if (!slot) { return std::nullopt; }
				auto &entry          = slots_[*slot];
				entry                = {};
				entry.allocated_     = true;
				entry.frame_index_   = frame.frame_index_;
				entry.pic_order_cnt_ = frame.pic_order_cnt_;
				entry.pic_order_lsb_ = frame.pic_order_cnt_lsb_;
				entry.field_picture_ = frame.field_picture_;
				entry.bottom_field_  = frame.bottom_field_;
				return slot;
			}

			void trace_state(const char *event, std::uint32_t frame_index, std::int32_t current_poc) const {
				if (std::getenv("H265_TRACE_DPB") == nullptr) { return; }
				std::cerr << "h265_dpb event=" << event << " frame=" << frame_index << " poc=" << current_poc;
				for (std::uint8_t slot = 0; slot < slots_.size(); ++slot) {
					const auto &entry = slots_[slot];
					if (!entry.allocated_) { continue; }
					std::cerr << " [" << static_cast<std::uint32_t>(slot) << ":poc=" << entry.pic_order_cnt_
							  << ",f=" << entry.frame_index_ << ",s=" << entry.short_ref_ << ",l=" << entry.long_ref_
							  << ",o=" << entry.output_ << ",u=" << entry.unavailable_
							  << ",field=" << entry.field_picture_ << ",bottom=" << entry.bottom_field_ << "]";
				}
				std::cerr << "\n";
			}

			void begin_current_reference_marking(std::uint8_t current_slot) {
				for (std::uint8_t slot = 0; slot < slots_.size(); ++slot) {
					if (!slots_[slot].allocated_ || slot == current_slot) { continue; }
					slots_[slot].short_ref_ = false;
					slots_[slot].long_ref_  = false;
					slots_[slot].va_flags_  = 0;
				}
			}

			[[nodiscard]] Dpb_Reference_Lookup find_reference_slot(const Reference_Target &target) const {
				std::optional<std::uint8_t> lsb_match;
				std::optional<std::uint8_t> unavailable_match;
				for (std::uint8_t slot = 0; slot < slots_.size(); ++slot) {
					const auto &entry = slots_[slot];
					if (!entry.allocated_) { continue; }
					if (open_slot_ && slot == *open_slot_) { continue; }
					if (target.exact_poc_) {
						if (entry.pic_order_cnt_ != target.pic_order_cnt_) { continue; }
						if (!entry.unavailable_ && entry.frame_index_ != kInvalid_H265_Frame_Index) {
							return Dpb_Reference_Lookup{.slot_ = slot};
						}
						if (!unavailable_match) { unavailable_match = slot; }
						continue;
					}
					if (entry.pic_order_lsb_ != target.pic_order_cnt_lsb_) { continue; }
					if (entry.unavailable_ || entry.frame_index_ == kInvalid_H265_Frame_Index) {
						if (!unavailable_match) { unavailable_match = slot; }
						continue;
					}
					if (lsb_match) { return Dpb_Reference_Lookup{.slot_ = std::nullopt, .ambiguous_lsb_ = true}; }
					lsb_match = slot;
				}
				return Dpb_Reference_Lookup{.slot_ = lsb_match ? lsb_match : unavailable_match};
			}

			[[nodiscard]] std::optional<std::uint8_t> allocate_unavailable(const Reference_Target &target) {
				auto slot = first_free_slot();
				if (!slot) { slot = first_reclaimable_slot(); }
				if (!slot) { return std::nullopt; }
				auto &entry          = slots_[*slot];
				entry                = {};
				entry.allocated_     = true;
				entry.unavailable_   = true;
				entry.frame_index_   = kInvalid_H265_Frame_Index;
				entry.pic_order_cnt_ = target.pic_order_cnt_;
				entry.pic_order_lsb_ = target.pic_order_cnt_lsb_;
				return slot;
			}

			void mark_reference(std::uint8_t slot, const Reference_Target &target) {
				auto &entry      = slots_[slot];
				entry.short_ref_ = !target.long_term_;
				entry.long_ref_  = target.long_term_;
				entry.va_flags_ |= target.va_flags_;
			}

			void clear_output(std::uint8_t slot) {
				auto &entry   = slots_[slot];
				entry.output_ = false;
				if (!entry.short_ref_ && !entry.long_ref_) { entry = {}; }
			}

			void bump_output_if_needed(std::uint8_t max_dec_pic_buffering) {
				if (first_free_slot()) { return; }
				std::uint8_t dpb_count = 0;
				for (const auto &entry: slots_) {
					if (!entry.allocated_) { continue; }
					if (entry.output_ || entry.short_ref_ || entry.long_ref_ || entry.unavailable_) { ++dpb_count; }
				}
				if (dpb_count < max_dec_pic_buffering) { return; }

				std::optional<std::uint8_t> min_output_slot;
				std::int32_t                min_output_poc = std::numeric_limits<std::int32_t>::max();
				for (std::uint8_t slot = 0; slot < slots_.size(); ++slot) {
					const auto &entry = slots_[slot];
					if (!entry.allocated_ || !entry.output_ || entry.short_ref_ || entry.long_ref_ ||
						entry.unavailable_) {
						continue;
					}
					if (!min_output_slot || entry.pic_order_cnt_ < min_output_poc) {
						min_output_slot = slot;
						min_output_poc  = entry.pic_order_cnt_;
					}
				}
				if (min_output_slot) { clear_output(*min_output_slot); }
			}

			void output_one_if_needed(std::uint8_t max_num_reorder_pics, bool no_output_of_prior_pics) {
				if (no_output_of_prior_pics) {
					for (std::uint8_t slot = 0; slot < slots_.size(); ++slot) {
						const auto &entry = slots_[slot];
						if (!entry.allocated_ || !entry.output_) { continue; }
						if (open_slot_ && slot == *open_slot_) { continue; }
						clear_output(slot);
					}
				}
				std::uint8_t                output_count = 0;
				std::optional<std::uint8_t> min_output_slot;
				std::int32_t                min_output_poc = std::numeric_limits<std::int32_t>::max();
				for (std::uint8_t slot = 0; slot < slots_.size(); ++slot) {
					const auto &entry = slots_[slot];
					if (!entry.allocated_ || !entry.output_) { continue; }
					++output_count;
					if (!min_output_slot || entry.pic_order_cnt_ < min_output_poc) {
						min_output_slot = slot;
						min_output_poc  = entry.pic_order_cnt_;
					}
				}
				if (output_count > max_num_reorder_pics && min_output_slot) { clear_output(*min_output_slot); }
			}

			void release_unmarked() {
				for (auto &entry: slots_) {
					if (!entry.allocated_) { continue; }
					if (entry.short_ref_ || entry.long_ref_ || entry.output_) { continue; }
					entry = {};
				}
			}

			void reset_for_recovery() {
				for (auto &entry: slots_) { entry = {}; }
				open_frame_ = std::nullopt;
				open_slot_  = std::nullopt;
			}
		};

		[[nodiscard]] std::uint8_t h265_sps_max_sublayer_index(const Sequence_Parameter_Set *sps) {
			if (sps == nullptr) { return 0; }
			return static_cast<std::uint8_t>(
					std::min<std::uint8_t>(sps->max_sub_layers_minus1, kMax_Sublayers - 1U));
		}

		[[nodiscard]] std::uint8_t max_num_reorder_pics(const Sequence_Parameter_Set *sps) {
			if (sps == nullptr) { return 0U; }
			return static_cast<std::uint8_t>(
					std::min<std::uint32_t>(sps->max_num_reorder_pics[h265_sps_max_sublayer_index(sps)], 16U));
		}

		[[nodiscard]] std::uint8_t max_dec_pic_buffering(const Sequence_Parameter_Set *sps) {
			if (sps == nullptr) { return 1U; }
			const auto sublayer = h265_sps_max_sublayer_index(sps);
			return static_cast<std::uint8_t>(
					std::min<std::uint32_t>(static_cast<std::uint32_t>(sps->max_dec_pic_buffering_minus1[sublayer]) + 1U,
									   static_cast<std::uint32_t>(kVirtual_Dpb_Slot_Count)));
		}

		[[nodiscard]] std::int32_t calculate_poc(const Slice_Header &slice, Nal_Unit_Type nal_type,
												 std::int32_t prev_tid0_pic) {
			const auto *pps = slice.pps;
			const auto *sps = pps == nullptr ? nullptr : pps->sps;
			if (sps == nullptr) { return static_cast<std::int32_t>(slice.pic_order_cnt_lsb); }
			const std::int32_t max_poc_lsb =
					static_cast<std::int32_t>(1U << (sps->log2_max_pic_order_cnt_lsb_minus4 + 4U));
			const std::int32_t prev_poc_lsb = prev_tid0_pic % max_poc_lsb;
			const std::int32_t prev_poc_msb = prev_tid0_pic - prev_poc_lsb;
			const std::int32_t poc_lsb      = slice.pic_order_cnt_lsb;

			std::int32_t poc_msb = 0;
			if (is_idr_type(nal_type) || is_bla_type(nal_type)) {
				poc_msb = 0;
			} else if (poc_lsb < prev_poc_lsb && prev_poc_lsb - poc_lsb >= max_poc_lsb / 2) {
				poc_msb = prev_poc_msb + max_poc_lsb;
			} else if (poc_lsb > prev_poc_lsb && poc_lsb - prev_poc_lsb > max_poc_lsb / 2) {
				poc_msb = prev_poc_msb - max_poc_lsb;
			} else {
				poc_msb = prev_poc_msb;
			}
			return poc_msb + poc_lsb;
		}

		[[nodiscard]] const Short_Term_Reference_Set *active_short_term_rps(const Slice_Header &slice) {
			const auto *pps = slice.pps;
			const auto *sps = pps == nullptr ? nullptr : pps->sps;
			if (sps == nullptr) { return nullptr; }
			if (slice.short_term_ref_pic_set_sps_flag != 0U) {
				if (slice.short_term_ref_pic_set_idx >= sps->num_short_term_ref_pic_sets) { return nullptr; }
				return &sps->short_term_ref_pic_set[slice.short_term_ref_pic_set_idx];
			}
			return &slice.short_term_ref_pic_sets;
		}

		void append_short_term_targets(std::vector<Reference_Target> &before, std::vector<Reference_Target> &after,
									   std::vector<Reference_Target> &all, const Slice_Header &slice,
									   std::int32_t current_poc) {
			const auto *rps = active_short_term_rps(slice);
			if (rps == nullptr) { return; }
			for (std::uint8_t index = 0; index < rps->NumNegativePics; ++index) {
				const bool       used = rps->UsedByCurrPicS0[index] != 0U;
				Reference_Target target{
						.pic_order_cnt_     = current_poc + rps->DeltaPocS0[index],
						.pic_order_cnt_lsb_ = static_cast<std::uint16_t>(current_poc + rps->DeltaPocS0[index]),
						.exact_poc_         = true,
						.long_term_         = false,
						.va_flags_          = used ? kReference_St_Curr_Before : 0U,
						.used_by_current_   = used,
				};
				all.push_back(target);
				if (used) { before.push_back(target); }
			}
			for (std::uint8_t index = 0; index < rps->NumPositivePics; ++index) {
				const bool       used = rps->UsedByCurrPicS1[index] != 0U;
				Reference_Target target{
						.pic_order_cnt_     = current_poc + rps->DeltaPocS1[index],
						.pic_order_cnt_lsb_ = static_cast<std::uint16_t>(current_poc + rps->DeltaPocS1[index]),
						.exact_poc_         = true,
						.long_term_         = false,
						.va_flags_          = used ? kReference_St_Curr_After : 0U,
						.used_by_current_   = used,
				};
				all.push_back(target);
				if (used) { after.push_back(target); }
			}
		}

		void append_long_term_targets(std::vector<Reference_Target> &lt, std::vector<Reference_Target> &all,
									  const Slice_Header &slice, std::int32_t current_poc) {
			const auto *pps = slice.pps;
			const auto *sps = pps == nullptr ? nullptr : pps->sps;
			if (sps == nullptr || sps->long_term_ref_pics_present_flag == 0U) { return; }
			const std::int32_t max_poc_lsb =
					static_cast<std::int32_t>(1U << (sps->log2_max_pic_order_cnt_lsb_minus4 + 4U));
			std::uint32_t      previous_delta_poc_msb = 0;
			const std::uint8_t count                  = static_cast<std::uint8_t>(
					std::min<std::uint32_t>(slice.num_long_term_sps + slice.num_long_term_pics, 16U));
			for (std::uint8_t index = 0; index < count; ++index) {
				std::uint32_t lsb  = 0;
				bool          used = false;
				if (index < slice.num_long_term_sps) {
					const auto lt_index = slice.lt_idx_sps[index];
					if (lt_index >= sps->num_long_term_ref_pics_sps) { continue; }
					lsb  = sps->lt_ref_pic_poc_lsb_sps[lt_index];
					used = sps->used_by_curr_pic_lt_sps_flag[lt_index] != 0U;
				} else {
					lsb  = slice.poc_lsb_lt[index];
					used = slice.used_by_curr_pic_lt_flag[index] != 0U;
				}

				Reference_Target target{
						.pic_order_cnt_     = 0,
						.pic_order_cnt_lsb_ = static_cast<std::uint16_t>(lsb),
						.exact_poc_         = false,
						.long_term_         = true,
						.va_flags_          = used ? kReference_Lt_Curr : 0U,
						.used_by_current_   = used,
				};
				if (slice.delta_poc_msb_present_flag[index] != 0U) {
					previous_delta_poc_msb += slice.delta_poc_msb_cycle_lt[index];
					target.pic_order_cnt_ = current_poc -
											static_cast<std::int32_t>(previous_delta_poc_msb) * max_poc_lsb -
											(current_poc & (max_poc_lsb - 1)) + static_cast<std::int32_t>(lsb);
					target.exact_poc_     = true;
				}
				all.push_back(target);
				if (used) { lt.push_back(target); }
			}
		}

		struct Reference_Sets {
			std::vector<Reference_Target> before_;
			std::vector<Reference_Target> after_;
			std::vector<Reference_Target> lt_;
			std::vector<Reference_Target> all_;
		};

		[[nodiscard]] Reference_Sets derive_reference_sets(const Slice_Header &slice, std::int32_t current_poc) {
			Reference_Sets sets;
			append_short_term_targets(sets.before_, sets.after_, sets.all_, slice, current_poc);
			append_long_term_targets(sets.lt_, sets.all_, slice, current_poc);
			return sets;
		}

		[[nodiscard]] std::uint8_t active_ref_count(const Slice_Header &slice, std::uint8_t list) {
			if (list == 0U) {
				return static_cast<std::uint8_t>(std::min<std::uint32_t>(slice.num_ref_idx_l0_active_minus1 + 1U, 15U));
			}
			if (slice.type != kSlice_B) { return 0U; }
			return static_cast<std::uint8_t>(std::min<std::uint32_t>(slice.num_ref_idx_l1_active_minus1 + 1U, 15U));
		}

		[[nodiscard]] std::vector<Reference_Target> build_ref_pic_list(const Slice_Header &slice,
																	   const Reference_Sets &sets, std::uint8_t list) {
			const auto                    active = active_ref_count(slice, list);
			std::vector<Reference_Target> result;
			if (active == 0U) { return result; }
			const std::array<const std::vector<Reference_Target> *, 3> order =
					list == 0U ? std::array{&sets.before_, &sets.after_, &sets.lt_}
							   : std::array{&sets.after_, &sets.before_, &sets.lt_};
			std::vector<Reference_Target> temp;
			for (const auto *group: order) {
				for (const auto &target: *group) { temp.push_back(target); }
			}
			if (temp.empty()) { return result; }
			const auto &mod      = slice.ref_pic_list_modification;
			const bool  modified = list == 0U ? mod.ref_pic_list_modification_flag_l0 != 0U
											  : mod.ref_pic_list_modification_flag_l1 != 0U;
			const auto *entries  = list == 0U ? mod.list_entry_l0 : mod.list_entry_l1;
			if (modified) {
				for (std::uint8_t index = 0; index < active; ++index) {
					if (entries[index] < temp.size()) { result.push_back(temp[entries[index]]); }
				}
			} else {
				while (result.size() < active) {
					const auto before_size = result.size();
					for (const auto &target: temp) {
						if (result.size() >= active) { break; }
						result.push_back(target);
					}
					if (result.size() == before_size) { break; }
				}
			}
			return result;
		}

		[[nodiscard]] Decoded_Reference_Lookup find_decoded_reference(std::span<const Decoded_Frame_State> decoded,
																	  const Reference_Target              &target) {
			for (auto it = decoded.rbegin(); it != decoded.rend(); ++it) {
				if (target.exact_poc_) {
					if (it->pic_order_cnt_ == target.pic_order_cnt_) { return Decoded_Reference_Lookup{.frame_ = *it}; }
				} else if (it->pic_order_cnt_lsb_ == target.pic_order_cnt_lsb_) {
					return Decoded_Reference_Lookup{.frame_ = *it};
				}
			}
			return {};
		}

		void derive_reference_state(Parsed_Bitstream &parsed, Frame_Reference_State &state,
									std::span<const Decoded_Frame_State> decoded_frames, std::uint64_t logical_nal,
									const Slice_Header &slice, Virtual_Dpb_State &dpb) {
			const auto sets = derive_reference_sets(slice, state.pic_order_cnt_);
			state.ref_pic_set_st_curr_before_.fill(0xFFU);
			state.ref_pic_set_st_curr_after_.fill(0xFFU);
			state.ref_pic_set_lt_curr_.fill(0xFFU);
			if (state.current_dpb_slot_ != 0xFFU) { dpb.begin_current_reference_marking(state.current_dpb_slot_); }

			auto mark_target = [&](const Reference_Target &target) {
				auto dpb_reference = dpb.find_reference_slot(target);
				if (dpb_reference.ambiguous_lsb_) {
					mark_unsupported(parsed, logical_nal,
									 "H.265 long-term reference POC LSB is ambiguous in indexed decode order");
					return std::optional<std::uint8_t>{};
				}
				std::optional<std::uint8_t> slot = dpb_reference.slot_;
				if (!slot) {
					auto reference = find_decoded_reference(decoded_frames, target);
					if (reference.ambiguous_lsb_) {
						mark_unsupported(parsed, logical_nal,
										 "H.265 long-term reference POC LSB is ambiguous in indexed decode order");
						return std::optional<std::uint8_t>{};
					}
					if (reference.frame_) {
						for (std::uint8_t candidate = 0; candidate < dpb.slots_.size(); ++candidate) {
							const auto &entry = dpb.slots_[candidate];
							if (entry.allocated_ && !entry.unavailable_ &&
								entry.frame_index_ == reference.frame_->frame_index_) {
								slot = candidate;
								break;
							}
						}
						if (!slot) { slot = dpb.allocate_decoded_reference(*reference.frame_); }
					}
				}
				if (!slot) { slot = dpb.allocate_unavailable(target); }
				if (!slot) {
					mark_unsupported(parsed, logical_nal, "H.265 frame uses more than 15 active references");
					return std::optional<std::uint8_t>{};
				}
				dpb.mark_reference(*slot, target);
				return slot;
			};

			for (const auto &target: sets.all_) { (void) mark_target(target); }
			const auto preserve_set_order = [&](const std::vector<Reference_Target> &targets, auto &slots) {
				for (std::size_t index = 0; index < targets.size() && index < slots.size(); ++index) {
					if (const auto slot = mark_target(targets[index])) { slots[index] = *slot; }
				}
			};
			preserve_set_order(sets.before_, state.ref_pic_set_st_curr_before_);
			preserve_set_order(sets.after_, state.ref_pic_set_st_curr_after_);
			preserve_set_order(sets.lt_, state.ref_pic_set_lt_curr_);

			for (std::uint8_t list = 0; list < 2U; ++list) {
				const auto rpl = build_ref_pic_list(slice, sets, list);
				for (std::uint8_t index = 0; index < rpl.size() && index < 15U; ++index) {
					auto slot = mark_target(rpl[index]);
					if (slot) { state.ref_pic_list_slot_[static_cast<std::size_t>(list) * 15U + index] = *slot; }
				}
			}
		}

		void finalize_reference_state(Parsed_Bitstream &parsed, H265_Frame_Decode_Metadata &metadata,
									  Frame_Reference_State &state, Virtual_Dpb_State &dpb, std::uint32_t frame_index,
									  std::uint64_t byte_offset) {
			metadata.reference_frame_index_.fill(kInvalid_H265_Frame_Index);
			metadata.used_reference_mask_        = 0;
			metadata.active_reference_mask_      = 0;
			metadata.unavailable_reference_mask_ = 0;
			state.reference_long_term_.fill(false);
			state.reference_unavailable_.fill(false);
			state.reference_field_picture_.fill(false);
			state.reference_bottom_field_.fill(false);
			state.reference_pic_order_cnt_.fill(0);
			state.reference_va_flags_.fill(0);
			state.reference_dpb_slot_.fill(0xFFU);

			std::array<std::uint8_t, kVirtual_Dpb_Slot_Count> dpb_to_va{};
			dpb_to_va.fill(0xFFU);
			std::uint8_t reference_count = 0;
			for (std::uint8_t dpb_slot = 0; dpb_slot < dpb.slots_.size(); ++dpb_slot) {
				const auto &entry = dpb.slots_[dpb_slot];
				if (!entry.allocated_ || !(entry.short_ref_ || entry.long_ref_)) { continue; }
				if (!entry.unavailable_ && entry.frame_index_ == frame_index) { continue; }
				if (reference_count >= metadata.reference_frame_index_.size()) {
					mark_unsupported(parsed, byte_offset, "H.265 frame uses more than 15 active references");
					break;
				}
				dpb_to_va[dpb_slot] = reference_count;
				metadata.used_reference_mask_ |= static_cast<std::uint16_t>(1U << reference_count);
				if (entry.va_flags_ != 0U) {
					metadata.active_reference_mask_ |= static_cast<std::uint16_t>(1U << reference_count);
				}
				if (entry.unavailable_) {
					metadata.unavailable_reference_mask_ |= static_cast<std::uint16_t>(1U << reference_count);
				} else {
					metadata.reference_frame_index_[reference_count] = entry.frame_index_;
				}
				state.reference_unavailable_[reference_count]   = entry.unavailable_;
				state.reference_long_term_[reference_count]     = entry.long_ref_;
				state.reference_field_picture_[reference_count] = entry.field_picture_;
				state.reference_bottom_field_[reference_count]  = entry.bottom_field_;
				state.reference_pic_order_cnt_[reference_count] = entry.pic_order_cnt_;
				state.reference_va_flags_[reference_count]      = entry.va_flags_;
				state.reference_dpb_slot_[reference_count]      = dpb_slot;
				++reference_count;
			}

			const auto remap_dpb_slots = [&](auto &slots) {
			for (auto &slot: slots) {
				if (slot == 0xFFU) { continue; }
				if (slot >= dpb_to_va.size() || dpb_to_va[slot] == 0xFFU) {
					slot = 0xFFU;
					continue;
				}
				slot = dpb_to_va[slot];
			}
			};
			remap_dpb_slots(state.ref_pic_list_slot_);
			remap_dpb_slots(state.ref_pic_set_st_curr_before_);
			remap_dpb_slots(state.ref_pic_set_st_curr_after_);
			remap_dpb_slots(state.ref_pic_set_lt_curr_);
			dpb.release_unmarked();
			dpb.open_frame_ = std::nullopt;
			dpb.open_slot_  = std::nullopt;
		}
	} // namespace

	namespace {
	Result<Parsed_Bitstream> parse_indexed_nals(
		std::span<const Indexed_Nal> indexed_nals, std::optional<std::uint16_t> previous_poc = std::nullopt) {
		Parsed_Bitstream                         parsed;
		std::uint64_t                            pending_access_unit_start = std::numeric_limits<std::uint64_t>::max();
		std::vector<Decoded_Frame_State>         decoded_frames;
		Virtual_Dpb_State                        dpb;
		std::int32_t                             prev_tid0_pic = previous_poc.value_or(0);
		if (previous_poc) {
			// The live bridge supplies the POC of its single decoded predecessor.
			decoded_frames.push_back(Decoded_Frame_State{
				.frame_index_ = kInvalid_H265_Frame_Index - 1,
				.pic_order_cnt_ = *previous_poc,
				.pic_order_cnt_lsb_ = *previous_poc,
			});
			if (!dpb.allocate_decoded_reference(decoded_frames.back())) {
				return std::unexpected(h265_error(0, 0, "cannot seed H.265 previous picture"));
			}
		}
		std::optional<std::uint8_t>                    pending_pic_struct;
		std::optional<color::Hdr10_Plus>          pending_hdr10_plus;
		color::Dynamic_HDR pending_dynamic_hdr;
		std::uint32_t hdr_sequence_id = 0;
		std::optional<Slice_Header>           current_independent_slice;
		bool                                     new_bitstream_or_eos       = true;
		bool                                     no_output_before_recovery  = false;
		bool                                     dropping_open_picture      = false;
		bool                                     non_vcl_after_open_picture = false;
		bool                                     have_open_picture          = false;
		std::uint64_t                            parameter_generation       = 0;
		std::uint64_t                            snapshotted_generation     = std::numeric_limits<std::uint64_t>::max();
		std::shared_ptr<H265_Parameter_Snapshot> parameter_snapshot;
		std::array<std::optional<Video_Parameter_Set>, 16> video_sets;
		std::array<std::optional<Sequence_Parameter_Set>, 16> sequences;
		std::array<std::optional<Picture_Parameter_Set>, 64> pictures;
		std::array<std::optional<Indexed_Nal>, 64> picture_sources;
		const Sequence_Parameter_Set *last_sps = nullptr;

		const auto load_picture = [&](std::uint8_t id) -> Result<void> {
			const auto &source = *picture_sources[id];
			auto picture = parse_pps(source.nalu_, sequences);
			if (!picture) {
				if (picture.error().missing_parameter_set) {
					return {};
				}
				return std::unexpected(syntax_error(source.logical_nal_ + Nal_View::header_bytes, picture.error()));
			}
			pictures[id] = *picture;
			return {};
		};

		const auto finalize_open_frame = [&]() {
			if (!dpb.open_frame_) { return; }
			const auto frame_index = *dpb.open_frame_;
			if (frame_index >= parsed.frame_metadata_.size() || frame_index >= parsed.frame_states_.size()) {
				dpb.open_frame_ = std::nullopt;
				dpb.open_slot_  = std::nullopt;
				return;
			}
			finalize_reference_state(parsed, parsed.frame_metadata_[frame_index], parsed.frame_states_[frame_index],
									 dpb, frame_index, parsed.frame_metadata_[frame_index].bitstream_.begin_);
		};

		for (const auto &indexed_nal: indexed_nals) {
			const auto &nalu = indexed_nal.nalu_;
			const auto logical_start_code = indexed_nal.logical_start_code_;
			const auto logical_nal = indexed_nal.logical_nal_;
			const auto logical_end = indexed_nal.logical_end_;
			if (nalu.layer_id != 0U) {
				mark_unsupported(parsed, logical_nal,
								 "H.265 multilayer/MV-HEVC bitstreams are not exposed by this API");
			}
			parsed.nals_.push_back(Nal_Unit{
					.type_              = nalu.type,
					.layer_id_          = nalu.layer_id,
					.temporal_id_       = static_cast<std::uint8_t>(nalu.temporal_id_plus1 - 1U),
					.start_code_offset_ = logical_start_code,
					.nal_offset_        = logical_nal,
					.payload_offset_    = logical_nal + nalu.header_bytes,
					.end_offset_        = logical_end,
			});
			// Retain enhancement-layer positions for diagnostics. Only layer zero
			// participates in this index's parameter-set and picture state.
			if (nalu.layer_id != 0U) { continue; }

			if (!indexed_nal.configuration_ &&
				pending_access_unit_start == std::numeric_limits<std::uint64_t>::max() && !is_vcl_type(nalu.type)) {
				pending_access_unit_start = logical_start_code;
			}

			const auto payload_offset = logical_nal + Nal_View::header_bytes;
			if (nalu.type == Nal_Unit_Type::VPS) {
				auto vps = parse_vps(nalu);
				if (!vps) {
					return std::unexpected(syntax_error(payload_offset, vps.error()));
				}
				const auto id = vps->id;
				video_sets[id] = *vps;
				for (auto &sps : sequences) {
					if (sps && sps->vps_id == id) {
						sps->vps = &*video_sets[id];
					}
				}
				++parameter_generation;
			} else if (nalu.type == Nal_Unit_Type::SPS) {
				auto sps = parse_sps(nalu);
				if (!sps) {
					return std::unexpected(syntax_error(payload_offset, sps.error()));
				}
				const auto prior_max_width = parsed.has_sps_ ? parsed.sequence_.max_width_ : 0U;
				const auto prior_max_height = parsed.has_sps_ ? parsed.sequence_.max_height_ : 0U;
				parsed.has_sps_ = true;
				parsed.sequence_ = sequence_from_sps(*sps);
				if (sps->bit_depth_luma_minus8 > 4 || sps->bit_depth_chroma_minus8 > 4) {
					parsed.sequence_.profile_ = H265_Profile::UNSUPPORTED_EXTENSION;
					parsed.sequence_.range_extension_ = true;
					return parsed;
				}
				parsed.sequence_.max_width_ = std::max(parsed.sequence_.max_width_, prior_max_width);
				parsed.sequence_.max_height_ = std::max(parsed.sequence_.max_height_, prior_max_height);
				const auto id = sps->id;
				if (video_sets[sps->vps_id]) {
					sps->vps = &*video_sets[sps->vps_id];
				}
				sequences[id] = std::move(*sps);
				last_sps = &*sequences[id];
				for (auto &picture : pictures) {
					if (picture && picture->sps_id == id) {
						picture.reset();
					}
				}
				++parameter_generation;
			} else if (nalu.type == Nal_Unit_Type::PPS) {
				Rbsp_Reader bits(nalu.payload());
				const auto id = bits.ue(63);
				if (!id) {
					return std::unexpected(syntax_error(payload_offset, id.error()));
				}
				picture_sources[*id] = indexed_nal;
				pictures[*id].reset();
				auto loaded = load_picture(static_cast<std::uint8_t>(*id));
				if (!loaded) {
					return std::unexpected(loaded.error());
				}
				++parameter_generation;
			}
			if (nalu.type == Nal_Unit_Type::PREFIX_SEI || nalu.type == Nal_Unit_Type::SUFFIX_SEI) {
				const auto dynamic = color::parse_dynamic_hdr_sei(nalu.payload());
				if (nalu.type == Nal_Unit_Type::PREFIX_SEI) { color::merge_dynamic_hdr(pending_dynamic_hdr, dynamic);
				} else if (!dropping_open_picture && have_open_picture && !parsed.frame_metadata_.empty()) {
					color::merge_dynamic_hdr(parsed.frame_metadata_.back().dynamic_hdr_, dynamic);
				}
				const auto sei = sei_info(last_sps, nalu);
				if (sei.hdr10_plus_) {
					if (nalu.type == Nal_Unit_Type::SUFFIX_SEI && dropping_open_picture) {
						// Metadata on a discarded RASL picture must not alter its predecessor.
					} else if (nalu.type == Nal_Unit_Type::SUFFIX_SEI && have_open_picture && !parsed.frame_metadata_.empty()) {
						parsed.frame_metadata_.back().hdr10_plus_ = sei.hdr10_plus_;
					} else {
						pending_hdr10_plus = sei.hdr10_plus_;
					}
				}
				if (sei.mastering_display_) {
					parsed.sequence_.color_.mastering_display_ = sei.mastering_display_;
				}
				if (sei.content_light_) { parsed.sequence_.color_.content_light_ = sei.content_light_; }
				if (sei.pic_struct_) {
					if (nalu.type == Nal_Unit_Type::PREFIX_SEI) {
						pending_pic_struct = *sei.pic_struct_;
					} else if (have_open_picture && !parsed.frame_states_.empty()) {
						const auto frame_index = static_cast<std::uint32_t>(parsed.frame_states_.size() - 1U);
						const auto info =
								field_picture_info_from_pic_struct(*sei.pic_struct_).value_or(Field_Picture_Info{});
						auto &state          = parsed.frame_states_.back();
						state.field_picture_ = info.field_picture_;
						state.bottom_field_  = info.bottom_field_;
						if (state.current_dpb_slot_ < dpb.slots_.size()) {
							auto &entry          = dpb.slots_[state.current_dpb_slot_];
							entry.field_picture_ = info.field_picture_;
							entry.bottom_field_  = info.bottom_field_;
						}
						for (auto &decoded: decoded_frames) {
							if (decoded.frame_index_ != frame_index) { continue; }
							decoded.field_picture_ = info.field_picture_;
							decoded.bottom_field_  = info.bottom_field_;
							break;
						}
					} else {
						pending_pic_struct = *sei.pic_struct_;
					}
					++parameter_generation;
				}
			}
			if (nalu.type == Nal_Unit_Type::EOS || nalu.type == Nal_Unit_Type::EOB) { new_bitstream_or_eos = true; }

			if (is_vcl_type(nalu.type)) {
				Rbsp_Reader prefix(nalu.payload());
				auto start = prefix.skip(is_irap_type(nalu.type) ? 2 : 1);
				if (!start) {
					return std::unexpected(syntax_error(payload_offset, start.error()));
				}
				const auto id = prefix.ue(63);
				if (!id) {
					return std::unexpected(syntax_error(payload_offset, id.error()));
				}
				if (!pictures[*id] && picture_sources[*id]) {
					auto loaded = load_picture(static_cast<std::uint8_t>(*id));
					if (!loaded) {
						return std::unexpected(loaded.error());
					}
				}
				auto header = parse_slice_header(nalu, pictures);
				if (!header) {
					return std::unexpected(syntax_error(payload_offset, header.error()));
				}
				auto slice = std::move(*header);
				const bool starts_new_frame =
						!have_open_picture || slice.first_slice_segment_in_pic_flag != 0U || non_vcl_after_open_picture;
				if (starts_new_frame) {
					current_independent_slice.reset();
				}
				if (slice.dependent_slice_segment_flag &&
					(!current_independent_slice || current_independent_slice->pps_id != slice.pps_id)) {
					return std::unexpected(h265_error(logical_nal, logical_nal * 8,
							"H.265 dependent slice has no matching independent slice"));
				}
				inherit_dependent_slice_header(slice, current_independent_slice);
				if (!slice.dependent_slice_segment_flag) {
					current_independent_slice = slice;
				}
				const auto pic_order_cnt = calculate_poc(slice, nalu.type, prev_tid0_pic);
				if (starts_new_frame) {
					finalize_open_frame();
					dropping_open_picture = false;
					if (is_idr_type(nalu.type) || is_bla_type(nalu.type)) {
						++hdr_sequence_id;
						no_output_before_recovery = false;
						dpb.reset_for_recovery();
						decoded_frames.clear();
					} else if (nalu.type == Nal_Unit_Type::CRA) {
						no_output_before_recovery = new_bitstream_or_eos;
						if (no_output_before_recovery) {
							++hdr_sequence_id;
							// A CRA that starts a new bitstream/CVS cannot reference pictures
							// preceding the recovery point.  Keeping those pictures in the
							// virtual DPB makes repeated POC values resolve to stale images
							// after the leading RASL pictures have been discarded.
							dpb.reset_for_recovery();
							decoded_frames.clear();
						}
					}
					new_bitstream_or_eos = false;
					if (no_output_before_recovery && is_rasl_type(nalu.type)) {
						pending_access_unit_start = std::numeric_limits<std::uint64_t>::max();
						pending_pic_struct        = std::nullopt;
						pending_hdr10_plus        = std::nullopt;
						pending_dynamic_hdr = {};
						have_open_picture         = true;
						dropping_open_picture     = true;
						continue;
					}
					const auto *active_vps = slice.pps->sps->vps;
					const bool active_parameter_ids_changed = parameter_snapshot &&
							(parameter_snapshot->pps_.id != slice.pps->id ||
							 parameter_snapshot->sps_.id != slice.pps->sps->id ||
							 (parameter_snapshot->sps_.vps == nullptr) != (active_vps == nullptr) ||
							 (active_vps != nullptr && parameter_snapshot->vps_.id != active_vps->id));
					if (snapshotted_generation != parameter_generation || !parameter_snapshot ||
							active_parameter_ids_changed) {
						parameter_snapshot     = snapshot_parameter_sets(slice);
						snapshotted_generation = parameter_generation;
					}
					slice.pps = &parameter_snapshot->pps_;
					parsed.decode_frames_.push_back(Parsed_Decode_Frame{
							.parameters_ = parameter_snapshot,
							.slices_     = {Parsed_Decode_Slice{
									.nalu_ = nalu, .logical_offset_ = logical_nal, .header_ = slice}},
					});
					const auto begin = pending_access_unit_start == std::numeric_limits<std::uint64_t>::max()
											   ? logical_start_code
											   : pending_access_unit_start;
					auto metadata    = make_frame_metadata(begin, logical_end,
														   to_internal_slice_type(slice.type), is_irap_type(nalu.type));
					copy_visible_rect_from_sps(metadata, slice.pps->sps);
					metadata.hdr10_plus_ = std::exchange(pending_hdr10_plus, std::nullopt);
					metadata.dynamic_hdr_ = std::exchange(pending_dynamic_hdr, {});
					metadata.dynamic_hdr_.sequence_id = hdr_sequence_id;

					Frame_Reference_State state;
					state.pic_order_cnt_     = pic_order_cnt;
					state.pic_order_cnt_lsb_ = slice.pic_order_cnt_lsb;
					state.non_reference_     = is_non_reference_vcl(nalu.type);
					state.output_            = slice.pps->output_flag_present_flag == 0U || slice.pic_output_flag != 0U;
					const auto field_info    = field_picture_info_for_slice(slice, pending_pic_struct);
					state.field_picture_     = field_info.field_picture_;
					state.bottom_field_      = field_info.bottom_field_;
					pending_pic_struct       = std::nullopt;
					const auto frame_index   = static_cast<std::uint32_t>(parsed.frame_metadata_.size());
					if (!is_idr_type(nalu.type)) { dpb.bump_output_if_needed(max_dec_pic_buffering(slice.pps->sps)); }
					dpb.trace_state("before_alloc", frame_index, state.pic_order_cnt_);
					auto current_slot = dpb.allocate_current(frame_index, state);
					if (!current_slot) {
						dpb.trace_state("alloc_failed", frame_index, state.pic_order_cnt_);
						mark_unsupported(parsed, logical_nal, "H.265 decoded-picture buffer is full");
					} else {
						state.current_dpb_slot_ = *current_slot;
						dpb.trace_state("after_alloc", frame_index, state.pic_order_cnt_);
						dpb.output_one_if_needed(max_num_reorder_pics(slice.pps->sps),
												 slice.no_output_of_prior_pics_flag != 0U);
						dpb.trace_state("after_output", frame_index, state.pic_order_cnt_);
					}
					derive_reference_state(parsed, state, decoded_frames, logical_nal, slice, dpb);
					dpb.trace_state("after_derive_refs", frame_index, state.pic_order_cnt_);

					parsed.frame_metadata_.push_back(metadata);
					parsed.frame_states_.push_back(state);
					if ((nalu.temporal_id_plus1 - 1U) == 0U && !state.non_reference_ && !is_rasl_or_radl(nalu.type)) {
						prev_tid0_pic = state.pic_order_cnt_;
					}
					decoded_frames.push_back(Decoded_Frame_State{
							.frame_index_       = frame_index,
							.pic_order_cnt_     = state.pic_order_cnt_,
							.pic_order_cnt_lsb_ = state.pic_order_cnt_lsb_,
							.non_reference_     = state.non_reference_,
							.field_picture_     = state.field_picture_,
							.bottom_field_      = state.bottom_field_,
					});
					pending_access_unit_start = std::numeric_limits<std::uint64_t>::max();
					have_open_picture         = true;
				} else if (!parsed.frame_metadata_.empty()) {
					if (dropping_open_picture) {
						continue;
					}
					auto &frame = parsed.frame_metadata_.back();
					auto &state = parsed.frame_states_.back();
					if (parsed.decode_frames_.empty()) {
						return std::unexpected(h265_error(logical_nal, logical_nal * 8U,
														  "H.265 slice lowering state has no open decoded picture"));
					}
					slice.pps = &parsed.decode_frames_.back().parameters_->pps_;
					parsed.decode_frames_.back().slices_.push_back(
							Parsed_Decode_Slice{.nalu_ = nalu, .logical_offset_ = logical_nal, .header_ = slice});
					frame.bitstream_.size_ = logical_end - frame.bitstream_.begin_;
					frame.primary_slice_type_ = std::min(frame.primary_slice_type_, to_internal_slice_type(slice.type));
					frame.irap_               = frame.irap_ || is_irap_type(nalu.type);
					if (frame.visible_width_ == 0U || frame.visible_height_ == 0U) {
						copy_visible_rect_from_sps(frame, slice.pps->sps);
					}
					auto reference_history = std::span<const Decoded_Frame_State>(decoded_frames);
					if (!reference_history.empty() &&
						reference_history.back().frame_index_ + 1U == parsed.frame_metadata_.size()) {
						reference_history = reference_history.first(reference_history.size() - 1U);
					}
					derive_reference_state(parsed, state, reference_history, logical_nal, slice, dpb);
				}
				non_vcl_after_open_picture = false;
			} else if (have_open_picture) {
				non_vcl_after_open_picture = true;
			}

		}

		finalize_open_frame();
		if (!parsed.has_sps_) { return std::unexpected(h265_error(0, 0, "H.265 bitstream does not contain an SPS")); }
		if (parsed.sequence_.max_width_ == 0U || parsed.sequence_.max_height_ == 0U) {
			return std::unexpected(h265_error(0, 0, "H.265 SPS did not expose valid coded dimensions"));
		}
		if (parsed.frame_metadata_.empty()) {
			mark_unsupported(parsed, 0, "H.265 bitstream does not contain decodable VCL frames");
		}
		return parsed;
	}
	} // namespace

	H265_Sequence_Info sequence_info_for_sps(const Sequence_Parameter_Set &sps) {
		return sequence_from_sps(sps);
	}

	Result<Parsed_Bitstream> Annex_B_Parser::parse(std::span<const std::uint8_t> bitstream,
		std::optional<std::uint16_t> previous_poc) const {
		auto nals = identify_annex_b_nals(bitstream);
		if (!nals) { return std::unexpected(nals.error()); }
		return parse_indexed_nals(*nals, previous_poc);
	}

	Result<Parsed_Bitstream> Annex_B_Parser::parse_hevc_samples(
			std::span<const std::uint8_t> configuration,
			std::span<const av1::Borrowed_Bitstream_Extent> access_units,
			std::uint8_t nal_length_size) const {
		if (configuration.size() < 23U || configuration[0] != 1U || nal_length_size == 0U ||
			nal_length_size > 4U || ((configuration[21] & 3U) + 1U) != nal_length_size) {
			return std::unexpected(h265_error(0, 0, "invalid mmap-backed HEVC decoder configuration"));
		}
		std::vector<Indexed_Nal> nals;
		std::size_t cursor = 23U;
		const auto array_count = configuration[22];
		for (std::uint8_t array = 0; array < array_count; ++array) {
			if (configuration.size() - cursor < 3U) {
				return std::unexpected(h265_error(cursor, cursor * 8U, "truncated hvcC NAL array"));
			}
			++cursor; // array completeness/reserved/NAL-unit type
			const auto count = static_cast<std::uint16_t>((configuration[cursor] << 8U) | configuration[cursor + 1U]);
			cursor += 2U;
			for (std::uint16_t i = 0; i < count; ++i) {
				if (configuration.size() - cursor < 2U) {
					return std::unexpected(h265_error(cursor, cursor * 8U, "truncated hvcC NAL length"));
				}
				const auto size = static_cast<std::uint16_t>((configuration[cursor] << 8U) | configuration[cursor + 1U]);
				if (size == 0U || configuration.size() - cursor - 2U < size) {
					return std::unexpected(h265_error(cursor, cursor * 8U, "hvcC NAL exceeds decoder configuration"));
				}
				auto appended = append_hevc_nals(configuration.subspan(cursor, size + 2U), 2U, cursor,
						nals, true);
				if (!appended) { return std::unexpected(appended.error()); }
				cursor += size + 2U;
			}
		}
		if (cursor != configuration.size()) {
			return std::unexpected(h265_error(cursor, cursor * 8U, "hvcC has trailing malformed bytes"));
		}
		std::uint64_t previous_end = 0;
		for (const auto &extent: access_units) {
			if (extent.bytes_.empty() || extent.logical_begin_ < previous_end ||
				extent.logical_begin_ > UINT64_MAX - extent.bytes_.size()) {
				return std::unexpected(h265_error(extent.logical_begin_, extent.logical_begin_ * 8U,
						"HEVC mmap samples must be non-empty, ordered, and individually addressable"));
			}
			auto appended = append_hevc_nals(extent.bytes_, nal_length_size,
					extent.logical_begin_, nals);
			if (!appended) { return std::unexpected(appended.error()); }
			previous_end = extent.logical_begin_ + extent.bytes_.size();
		}
		if (access_units.empty()) { return std::unexpected(h265_error(0, 0, "HEVC sample index is empty")); }
		return parse_indexed_nals(nals);
	}
} // namespace video_editor::h265::decoder
