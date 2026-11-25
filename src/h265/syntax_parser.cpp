#include "h265/syntax_parser.hpp"
#include "h265/rbsp_reader.hpp"

#include <algorithm>
#include <bit>
#include <type_traits>

namespace video_editor::h265::decoder {
	namespace {
		using Bits = Rbsp_Reader;

		template<class T>
		Syntax_Result<T> at_field(Syntax_Result<T> value, const char *field) {
			if (!value) {
				value.error().field = field;
			}
			return value;
		}

		// Each assignment propagates a failed read with the affected syntax field.
#define READ(field, count)                                                                                             \
	field = static_cast<std::remove_reference_t<decltype((field))>>(                                                   \
			VIDEO_EDITOR_TRY(at_field(bits.bits(count), #field)))
#define FLAG(field) READ(field, 1)
#define UE(field, limit)                                                                                               \
	field = static_cast<std::remove_reference_t<decltype((field))>>(VIDEO_EDITOR_TRY(at_field(bits.ue(limit), #field)))
#define SE(field, low, high)                                                                                           \
	field = static_cast<std::remove_reference_t<decltype((field))>>(                                                   \
			VIDEO_EDITOR_TRY(at_field(bits.se(low, high), #field)))

		Syntax_Result<void> read_profile(Bits &bits, Sublayer_Ordering &out) {
			if (out.max_sub_layers_minus1 >= kMax_Sublayers) {
				return bits.fail("too many temporal sublayers");
			}
			auto &profile = out.profile_tier_level;
			VIDEO_EDITOR_TRY(bits.skip(2)); // profile_space
			FLAG(profile.tier_flag);
			READ(profile.profile_idc, 5);
			for (auto &flag: profile.profile_compatibility_flag) { FLAG(flag); }
			FLAG(profile.progressive_source_flag);
			FLAG(profile.interlaced_source_flag);
			FLAG(profile.non_packed_constraint_flag);
			FLAG(profile.frame_only_constraint_flag);
			VIDEO_EDITOR_TRY(bits.skip(44)); // remaining profile constraints
			READ(profile.level_idc, 8);

			std::array<std::uint8_t, kMax_Sublayers> sublayer_fields{};
			for (unsigned i = 0; i < out.max_sub_layers_minus1; ++i) { READ(sublayer_fields[i], 2); }
			if (out.max_sub_layers_minus1) {
				VIDEO_EDITOR_TRY(bits.skip(2 * (8 - out.max_sub_layers_minus1)));
			}
			for (unsigned i = 0; i < out.max_sub_layers_minus1; ++i) {
				if (sublayer_fields[i] & 2U) {
					VIDEO_EDITOR_TRY(bits.skip(88));
				}
				if (sublayer_fields[i] & 1U) {
					VIDEO_EDITOR_TRY(bits.skip(8));
				}
			}
			return {};
		}

		Syntax_Result<void> read_ordering(Bits &bits, Sublayer_Ordering &out) {
			FLAG(out.sub_layer_ordering_info_present_flag);
			const unsigned first = out.sub_layer_ordering_info_present_flag ? 0 : out.max_sub_layers_minus1;
			for (unsigned i = first; i <= out.max_sub_layers_minus1; ++i) {
				UE(out.max_dec_pic_buffering_minus1[i], 15);
				UE(out.max_num_reorder_pics[i], out.max_dec_pic_buffering_minus1[i]);
				UE(out.max_latency_increase_plus1[i], UINT32_MAX);
			}
			for (unsigned i = 0; i < first; ++i) {
				out.max_dec_pic_buffering_minus1[i] = out.max_dec_pic_buffering_minus1[first];
				out.max_num_reorder_pics[i]         = out.max_num_reorder_pics[first];
				out.max_latency_increase_plus1[i]   = out.max_latency_increase_plus1[first];
			}
			return {};
		}

		struct Hrd_Common {
			bool nal{}, vcl{}, sub_picture{};
		};

		Syntax_Result<void> skip_hrd(Bits &bits, Hrd_Common &hrd, unsigned max_sublayer, bool common_present) {
			if (common_present) {
				FLAG(hrd.nal);
				FLAG(hrd.vcl);
				hrd.sub_picture = false;
				if (hrd.nal || hrd.vcl) {
					FLAG(hrd.sub_picture);
					VIDEO_EDITOR_TRY(bits.skip(hrd.sub_picture ? 46 : 23));
				}
			}
			for (unsigned layer = 0; layer <= max_sublayer; ++layer) {
				const bool general_rate = VIDEO_EDITOR_TRY(bits.bits(1));
				const bool fixed_rate   = general_rate || VIDEO_EDITOR_TRY(bits.bits(1));
				bool       low_delay    = false;
				if (fixed_rate) {
					VIDEO_EDITOR_TRY(bits.ue(2047));
				} else {
					FLAG(low_delay);
				}
				const auto cpb_count = low_delay ? 1U : VIDEO_EDITOR_TRY(bits.ue(31)) + 1U;
				const auto entries   = cpb_count * (unsigned(hrd.nal) + unsigned(hrd.vcl));
				for (unsigned i = 0; i < entries; ++i) {
					const unsigned values = hrd.sub_picture ? 4U : 2U;
					for (unsigned parameter = 0; parameter < values; ++parameter) {
						VIDEO_EDITOR_TRY(bits.ue());
					}
					VIDEO_EDITOR_TRY(bits.skip(1)); // cbr_flag
				}
			}
			return {};
		}

		Syntax_Result<void> read_vui(Bits &bits, Sequence_Parameter_Set &sps) {
			auto &vui = sps.vui_params;
			FLAG(vui.aspect_ratio_info_present_flag);
			if (vui.aspect_ratio_info_present_flag) {
				READ(vui.aspect_ratio_idc, 8);
				if (vui.aspect_ratio_idc == 255) {
					READ(vui.sar_width, 16);
					READ(vui.sar_height, 16);
				}
			}
			FLAG(vui.overscan_info_present_flag);
			if (vui.overscan_info_present_flag) { FLAG(vui.overscan_appropriate_flag); }
			FLAG(vui.video_signal_type_present_flag);
			if (vui.video_signal_type_present_flag) {
				READ(vui.video_format, 3);
				FLAG(vui.video_full_range_flag);
				FLAG(vui.colour_description_present_flag);
				if (vui.colour_description_present_flag) {
					READ(vui.colour_primaries, 8);
					READ(vui.transfer_characteristics, 8);
					READ(vui.matrix_coefficients, 8);
				}
			}
			FLAG(vui.chroma_loc_info_present_flag);
			if (vui.chroma_loc_info_present_flag) {
				UE(vui.chroma_sample_loc_type_top_field, 5);
				UE(vui.chroma_sample_loc_type_bottom_field, 5);
			}
			FLAG(vui.neutral_chroma_indication_flag);
			FLAG(vui.field_seq_flag);
			FLAG(vui.frame_field_info_present_flag);
			FLAG(vui.default_display_window_flag);
			if (vui.default_display_window_flag) {
				UE(vui.def_disp_win_left_offset, UINT16_MAX);
				UE(vui.def_disp_win_right_offset, UINT16_MAX);
				UE(vui.def_disp_win_top_offset, UINT16_MAX);
				UE(vui.def_disp_win_bottom_offset, UINT16_MAX);
			}
			FLAG(vui.timing_info_present_flag);
			if (vui.timing_info_present_flag) {
				READ(vui.num_units_in_tick, 32);
				READ(vui.time_scale, 32);
				FLAG(vui.poc_proportional_to_timing_flag);
				if (vui.poc_proportional_to_timing_flag) { UE(vui.num_ticks_poc_diff_one_minus1, UINT32_MAX); }
				if (VIDEO_EDITOR_TRY(bits.bits(1))) {
					Hrd_Common hrd;
					VIDEO_EDITOR_TRY(skip_hrd(bits, hrd, sps.max_sub_layers_minus1, true));
				}
			}
			FLAG(vui.bitstream_restriction_flag);
			if (vui.bitstream_restriction_flag) {
				FLAG(vui.tiles_fixed_structure_flag);
				FLAG(vui.motion_vectors_over_pic_boundaries_flag);
				FLAG(vui.restricted_ref_pic_lists_flag);
				UE(vui.min_spatial_segmentation_idc, 4095);
				UE(vui.max_bytes_per_pic_denom, 16);
				UE(vui.max_bits_per_min_cu_denom, 16);
				UE(vui.log2_max_mv_length_horizontal, 15);
				UE(vui.log2_max_mv_length_vertical, 15);
			}
			return {};
		}

		// Table 7-6, converted from raster to the diagonal scan order of the syntax.
		constexpr std::uint8_t kDefault_Intra[64] = {16, 16, 16, 16, 17, 18, 21, 24, 16, 16, 16, 16, 17, 19, 22, 25,
													 16, 16, 17, 18, 20, 22, 25, 29, 16, 16, 18, 21, 24, 27, 31, 36,
													 17, 17, 20, 24, 30, 35, 41, 47, 18, 19, 22, 27, 35, 44, 54, 65,
													 21, 22, 25, 31, 41, 54, 70, 88, 24, 25, 29, 36, 47, 65, 88, 115};
		constexpr std::uint8_t kDefault_Inter[64] = {16, 16, 16, 16, 17, 18, 20, 24, 16, 16, 16, 17, 18, 20, 24, 25,
													 16, 16, 17, 18, 20, 24, 25, 28, 16, 17, 18, 20, 24, 25, 28, 33,
													 17, 18, 20, 24, 25, 28, 33, 41, 18, 20, 24, 25, 28, 33, 41, 54,
													 20, 24, 25, 28, 33, 41, 54, 71, 24, 25, 28, 33, 41, 54, 71, 91};

		void default_scaling(std::span<std::uint8_t> list, unsigned matrix_id) {
			if (list.size() == 16) {
				std::ranges::fill(list, 16);
				return;
			}
			const auto &raster = matrix_id < 3 ? kDefault_Intra : kDefault_Inter;
			unsigned    i      = 0;
			for (int diagonal = 0; diagonal <= 14; ++diagonal) {
				for (int x = std::max(0, diagonal - 7); x <= std::min(7, diagonal); ++x) {
					list[i++] = raster[(diagonal - x) * 8 + x];
				}
			}
		}

		template<std::size_t Matrices, std::size_t Coefficients>
		Syntax_Result<void> read_scaling_size(Bits &bits, std::uint8_t (&lists)[Matrices][Coefficients],
											  std::span<std::int16_t> dc = {}) {
			constexpr unsigned step = Matrices == 2 ? 3 : 1;
			for (unsigned i = 0; i < Matrices; ++i) {
				if (!VIDEO_EDITOR_TRY(bits.bits(1))) {
					const auto delta = VIDEO_EDITOR_TRY(bits.ue(i));
					if (delta == 0) {
						default_scaling(lists[i], i * step);
						if (!dc.empty()) {
							dc[i] = 8;
						}
					} else {
						std::ranges::copy(lists[i - delta], lists[i]);
						if (!dc.empty()) {
							dc[i] = dc[i - delta];
						}
					}
					continue;
				}
				int next = 8;
				if (!dc.empty()) {
					SE(dc[i], -7, 247);
					next = dc[i] + 8;
				}
				for (auto &coefficient: lists[i]) {
					next        = (next + VIDEO_EDITOR_TRY(bits.se(-128, 127)) + 256) & 255;
					coefficient = static_cast<std::uint8_t>(next);
				}
			}
			return {};
		}

		Syntax_Result<void> read_scaling(Bits &bits, Scaling_Lists &lists) {
			VIDEO_EDITOR_TRY(read_scaling_size(bits, lists.scaling_lists_4x4));
			VIDEO_EDITOR_TRY(read_scaling_size(bits, lists.scaling_lists_8x8));
			VIDEO_EDITOR_TRY(
					read_scaling_size(bits, lists.scaling_lists_16x16, lists.scaling_list_dc_coef_minus8_16x16));
			return read_scaling_size(bits, lists.scaling_lists_32x32, lists.scaling_list_dc_coef_minus8_32x32);
		}

		Syntax_Result<Short_Term_Reference_Set>
		read_short_term_set(Bits &bits, std::span<const Short_Term_Reference_Set> preceding, bool from_slice) {
			Short_Term_Reference_Set out;
			if (preceding.empty() || !VIDEO_EDITOR_TRY(bits.bits(1))) {
				UE(out.NumNegativePics, 16);
				UE(out.NumPositivePics, 16U - out.NumNegativePics);
				std::int32_t poc = 0;
				for (unsigned i = 0; i < out.NumNegativePics; ++i) {
					poc -= static_cast<int>(VIDEO_EDITOR_TRY(bits.ue(32767)) + 1U);
					out.DeltaPocS0[i] = poc;
					FLAG(out.UsedByCurrPicS0[i]);
				}
				poc = 0;
				for (unsigned i = 0; i < out.NumPositivePics; ++i) {
					poc += static_cast<int>(VIDEO_EDITOR_TRY(bits.ue(32767)) + 1U);
					out.DeltaPocS1[i] = poc;
					FLAG(out.UsedByCurrPicS1[i]);
				}
				return out;
			}

			const auto  distance        = from_slice ? VIDEO_EDITOR_TRY(bits.ue(preceding.size() - 1)) + 1 : 1U;
			const auto &ref             = preceding[preceding.size() - distance];
			const bool  subtract        = VIDEO_EDITOR_TRY(bits.bits(1));
			const auto  magnitude       = static_cast<int>(VIDEO_EDITOR_TRY(bits.ue(32767)) + 1U);
			const auto  delta           = subtract ? -magnitude : magnitude;
			out.NumDeltaPocsOfRefRpsIdx = ref.NumNegativePics + ref.NumPositivePics;
			struct Reference {
				std::int32_t poc;
				std::uint8_t used;
			};
			std::array<Reference, 17> derived{};
			unsigned                  count = 0;
			for (unsigned i = 0; i <= out.NumDeltaPocsOfRefRpsIdx; ++i) {
				const auto used   = VIDEO_EDITOR_TRY(bits.bits(1));
				const bool retain = used || VIDEO_EDITOR_TRY(bits.bits(1));
				const auto base   = i < ref.NumNegativePics           ? ref.DeltaPocS0[i]
									: i < out.NumDeltaPocsOfRefRpsIdx ? ref.DeltaPocS1[i - ref.NumNegativePics]
																	  : 0;
				if (retain && base + delta != 0) {
					derived[count++] = {base + delta, static_cast<std::uint8_t>(used)};
				}
			}
			if (count > 16) {
				return bits.fail("short-term reference set exceeds 16 pictures");
			}
			std::ranges::sort(std::span(derived).first(count), {}, &Reference::poc);
			// Negative POCs are in decreasing order, positive POCs in increasing order.
			unsigned first_positive = 0;
			while (first_positive < count && derived[first_positive].poc < 0) {
				++first_positive;
			}
			out.NumNegativePics = static_cast<std::uint8_t>(first_positive);
			out.NumPositivePics = static_cast<std::uint8_t>(count - first_positive);
			for (unsigned i = 0; i < first_positive; ++i) {
				out.DeltaPocS0[i]      = derived[first_positive - i - 1].poc;
				out.UsedByCurrPicS0[i] = derived[first_positive - i - 1].used;
			}
			for (unsigned i = first_positive; i < count; ++i) {
				out.DeltaPocS1[i - first_positive]      = derived[i].poc;
				out.UsedByCurrPicS1[i - first_positive] = derived[i].used;
			}
			return out;
		}

		Syntax_Result<void> read_sps_extensions(Bits &bits, Sequence_Parameter_Set &sps) {
			if (!VIDEO_EDITOR_TRY(bits.bits(1))) {
				return bits.align();
			}
			FLAG(sps.sps_range_extension_flag);
			const bool multilayer = VIDEO_EDITOR_TRY(bits.bits(1));
			const bool three_d    = VIDEO_EDITOR_TRY(bits.bits(1));
			FLAG(sps.sps_scc_extension_flag);
			const auto extension = VIDEO_EDITOR_TRY(bits.bits(4));
			if (sps.sps_range_extension_flag) {
				auto &range = sps.sps_extension_params;
				FLAG(range.transform_skip_rotation_enabled_flag);
				FLAG(range.transform_skip_context_enabled_flag);
				FLAG(range.implicit_rdpcm_enabled_flag);
				FLAG(range.explicit_rdpcm_enabled_flag);
				FLAG(range.extended_precision_processing_flag);
				FLAG(range.intra_smoothing_disabled_flag);
				FLAG(range.high_precision_offsets_enabled_flag);
				FLAG(range.persistent_rice_adaptation_enabled_flag);
				FLAG(range.cabac_bypass_alignment_enabled_flag);
			}
			if (multilayer) {
				VIDEO_EDITOR_TRY(bits.skip(1)); // inter_view_mv_vert_constraint_flag, Annex F
			}
			if (three_d) {
				// Annex I tools do not enter the base-layer hardware path.
				if (sps.sps_scc_extension_flag) {
					return bits.fail("combined H.265 3D and SCC extensions", true);
				}
				return {};
			}
			if (sps.sps_scc_extension_flag) {
				auto &scc = sps.sps_scc_extension_params;
				FLAG(scc.sps_curr_pic_ref_enabled_flag);
				FLAG(scc.palette_mode_enabled_flag);
				if (scc.palette_mode_enabled_flag) {
					UE(scc.palette_max_size, 64);
					UE(scc.delta_palette_max_predictor_size, 128U - scc.palette_max_size);
					FLAG(scc.sps_palette_predictor_initializers_present_flag);
					if (scc.sps_palette_predictor_initializers_present_flag) {
						UE(scc.sps_num_palette_predictor_initializer_minus1, 127);
						for (unsigned c = 0; c < (sps.chroma_format_idc == 0 ? 1U : 3U); ++c) {
							for (unsigned i = 0; i <= scc.sps_num_palette_predictor_initializer_minus1; ++i) {
								READ(scc.sps_palette_predictor_initializer[c][i],
									 8 + (c == 0 ? sps.bit_depth_luma_minus8 : sps.bit_depth_chroma_minus8));
							}
						}
					}
				}
				READ(scc.motion_vector_resolution_control_idc, 2);
				FLAG(scc.intra_boundary_filtering_disabled_flag);
			}
			return extension ? Syntax_Result<void>{} : bits.align();
		}
	} // namespace

	Syntax_Result<Nal_View> parse_nal_header(std::span<const std::uint8_t> bytes) {
		if (bytes.size() < Nal_View::header_bytes) {
			return std::unexpected(Syntax_Error{0, "truncated NAL header"});
		}
		const auto temporal_id_plus1 = static_cast<std::uint8_t>(bytes[1] & 7U);
		if ((bytes[0] & 0x80U) || temporal_id_plus1 == 0) {
			return std::unexpected(Syntax_Error{0, "invalid NAL header"});
		}
		return Nal_View{bytes, static_cast<Nal_Unit_Type>((bytes[0] >> 1) & 63U),
						static_cast<std::uint8_t>(((bytes[0] & 1U) << 5) | (bytes[1] >> 3)), temporal_id_plus1};
	}

	Syntax_Result<Video_Parameter_Set> parse_vps(const Nal_View &nal) {
		Bits                bits(nal.payload());
		Video_Parameter_Set vps;
		READ(vps.id, 4);
		VIDEO_EDITOR_TRY(bits.skip(8)); // base layer flags and max_layers_minus1
		READ(vps.max_sub_layers_minus1, 3);
		FLAG(vps.temporal_id_nesting_flag);
		VIDEO_EDITOR_TRY(bits.skip(16)); // reserved
		VIDEO_EDITOR_TRY(read_profile(bits, vps));
		VIDEO_EDITOR_TRY(read_ordering(bits, vps));
		const auto layer_count       = VIDEO_EDITOR_TRY(bits.bits(6)) + 1;
		const auto layer_sets_minus1 = VIDEO_EDITOR_TRY(bits.ue(1023));
		VIDEO_EDITOR_TRY(bits.skip(layer_count * layer_sets_minus1));
		FLAG(vps.timing_info_present_flag);
		if (vps.timing_info_present_flag) {
			READ(vps.num_units_in_tick, 32);
			READ(vps.time_scale, 32);
			FLAG(vps.poc_proportional_to_timing_flag);
			if (vps.poc_proportional_to_timing_flag) { UE(vps.num_ticks_poc_diff_one_minus1, UINT32_MAX); }
			const auto count = VIDEO_EDITOR_TRY(bits.ue(1024));
			Hrd_Common hrd;
			for (unsigned i = 0; i < count; ++i) {
				VIDEO_EDITOR_TRY(bits.ue(layer_sets_minus1));
				const bool common = i == 0 || VIDEO_EDITOR_TRY(bits.bits(1));
				VIDEO_EDITOR_TRY(skip_hrd(bits, hrd, vps.max_sub_layers_minus1, common));
			}
		}
		if (!VIDEO_EDITOR_TRY(bits.bits(1))) {
			VIDEO_EDITOR_TRY(bits.align());
		}
		return vps;
	}

	Syntax_Result<Sequence_Parameter_Set> parse_sps(const Nal_View &nal) {
		Bits                   bits(nal.payload());
		Sequence_Parameter_Set sps;
		READ(sps.vps_id, 4);
		READ(sps.max_sub_layers_minus1, 3);
		FLAG(sps.temporal_id_nesting_flag);
		VIDEO_EDITOR_TRY(read_profile(bits, sps));
		UE(sps.id, 15);
		UE(sps.chroma_format_idc, 3);
		if (sps.chroma_format_idc == 3) { FLAG(sps.separate_colour_plane_flag); }
		sps.chroma_array_type = sps.separate_colour_plane_flag ? 0 : sps.chroma_format_idc;
		UE(sps.pic_width_in_luma_samples, UINT32_MAX);
		UE(sps.pic_height_in_luma_samples, UINT32_MAX);
		if (!sps.pic_width_in_luma_samples || !sps.pic_height_in_luma_samples) {
			return bits.fail("zero SPS picture dimension");
		}
		FLAG(sps.conformance_window_flag);
		if (sps.conformance_window_flag) {
			UE(sps.conf_win_left_offset, sps.pic_width_in_luma_samples - 1);
			UE(sps.conf_win_right_offset, sps.pic_width_in_luma_samples - 1 - sps.conf_win_left_offset);
			UE(sps.conf_win_top_offset, sps.pic_height_in_luma_samples - 1);
			UE(sps.conf_win_bottom_offset, sps.pic_height_in_luma_samples - 1 - sps.conf_win_top_offset);
		}
		const unsigned sub_width   = sps.chroma_array_type == 1 || sps.chroma_array_type == 2 ? 2 : 1;
		const unsigned sub_height  = sps.chroma_array_type == 1 ? 2 : 1;
		const auto     crop_width  = std::uint64_t{sps.conf_win_left_offset + sps.conf_win_right_offset} * sub_width;
		const auto     crop_height = std::uint64_t{sps.conf_win_top_offset + sps.conf_win_bottom_offset} * sub_height;
		if (crop_width >= sps.pic_width_in_luma_samples || crop_height >= sps.pic_height_in_luma_samples) {
			return bits.fail("SPS conformance window lies outside the picture");
		}
		sps.crop_rect_x      = sps.conf_win_left_offset * sub_width;
		sps.crop_rect_y      = sps.conf_win_top_offset * sub_height;
		sps.crop_rect_width  = sps.pic_width_in_luma_samples - static_cast<std::uint32_t>(crop_width);
		sps.crop_rect_height = sps.pic_height_in_luma_samples - static_cast<std::uint32_t>(crop_height);
		UE(sps.bit_depth_luma_minus8, 247);
		UE(sps.bit_depth_chroma_minus8, 247);
		// Keep the depth information for the existing unsupported-format preflight.
		if (sps.bit_depth_luma_minus8 > 4 || sps.bit_depth_chroma_minus8 > 4) {
			return sps;
		}
		UE(sps.log2_max_pic_order_cnt_lsb_minus4, 12);
		VIDEO_EDITOR_TRY(read_ordering(bits, sps));
		UE(sps.log2_min_luma_coding_block_size_minus3, 3);
		UE(sps.log2_diff_max_min_luma_coding_block_size, 3U - sps.log2_min_luma_coding_block_size_minus3);
		UE(sps.log2_min_transform_block_size_minus2, 3);
		UE(sps.log2_diff_max_min_transform_block_size, 3U - sps.log2_min_transform_block_size_minus2);
		UE(sps.max_transform_hierarchy_depth_inter, 4);
		UE(sps.max_transform_hierarchy_depth_intra, 4);
		FLAG(sps.scaling_list_enabled_flag);
		if (sps.scaling_list_enabled_flag) {
			FLAG(sps.scaling_list_data_present_flag);
			if (sps.scaling_list_data_present_flag) {
				VIDEO_EDITOR_TRY(read_scaling(bits, sps.scaling_list));
			}
		}
		FLAG(sps.amp_enabled_flag);
		FLAG(sps.sample_adaptive_offset_enabled_flag);
		FLAG(sps.pcm_enabled_flag);
		if (sps.pcm_enabled_flag) {
			READ(sps.pcm_sample_bit_depth_luma_minus1, 4);
			READ(sps.pcm_sample_bit_depth_chroma_minus1, 4);
			UE(sps.log2_min_pcm_luma_coding_block_size_minus3, 2);
			UE(sps.log2_diff_max_min_pcm_luma_coding_block_size, 2);
			FLAG(sps.pcm_loop_filter_disabled_flag);
		}
		UE(sps.num_short_term_ref_pic_sets, 64);
		for (unsigned i = 0; i < sps.num_short_term_ref_pic_sets; ++i) {
			sps.short_term_ref_pic_set[i] =
					VIDEO_EDITOR_TRY(read_short_term_set(bits, {sps.short_term_ref_pic_set, i}, false));
		}
		FLAG(sps.long_term_ref_pics_present_flag);
		if (sps.long_term_ref_pics_present_flag) {
			UE(sps.num_long_term_ref_pics_sps, 32);
			for (unsigned i = 0; i < sps.num_long_term_ref_pics_sps; ++i) {
				READ(sps.lt_ref_pic_poc_lsb_sps[i], sps.log2_max_pic_order_cnt_lsb_minus4 + 4);
				FLAG(sps.used_by_curr_pic_lt_sps_flag[i]);
			}
		}
		FLAG(sps.temporal_mvp_enabled_flag);
		FLAG(sps.strong_intra_smoothing_enabled_flag);
		FLAG(sps.vui_parameters_present_flag);
		if (sps.vui_parameters_present_flag) {
			VIDEO_EDITOR_TRY(read_vui(bits, sps));
		}
		VIDEO_EDITOR_TRY(read_sps_extensions(bits, sps));
		return sps;
	}

	namespace {
		bool profile_compatible(const Profile_Tier_Level &profile, std::uint8_t id) {
			return profile.profile_idc == id || profile.profile_compatibility_flag[id] != 0;
		}

		Syntax_Result<void> read_pps_extensions(Bits &bits, Picture_Parameter_Set &pps) {
			if (!VIDEO_EDITOR_TRY(bits.bits(1))) {
				return bits.align();
			}
			// Some base profiles permit and ignore foreign PPS extension data.
			const auto &sps = *pps.sps;
			const bool  scc_profile =
					sps.sps_scc_extension_flag || profile_compatible(sps.profile_tier_level, kProfile_Screen_Content);
			const bool   range_profile = scc_profile || sps.sps_range_extension_flag ||
										 profile_compatible(sps.profile_tier_level, kProfile_Range_Extension) ||
										 profile_compatible(sps.profile_tier_level, kProfile_High_Throughput);
			std::uint8_t flags{};
			READ(flags, 8);
			const bool multilayer        = (flags & 0x40) != 0;
			const bool three_d           = (flags & 0x20) != 0;
			pps.pps_range_extension_flag = (flags & 0x80) != 0 && range_profile;
			pps.pps_scc_extension_flag   = (flags & 0x10) != 0 && scc_profile;
			if (pps.pps_range_extension_flag) {
				auto &range = pps.pps_extension_params;
				if (pps.transform_skip_enabled_flag) {
					const auto signalled =
							VIDEO_EDITOR_TRY(at_field(bits.ue(), "log2_max_transform_skip_block_size_minus2"));
					const unsigned max_transform_size_minus2 =
							sps.log2_min_transform_block_size_minus2 + sps.log2_diff_max_min_transform_block_size;
					// PERSIST_RPARAM_A_RExt_Sony_1 signals a larger limit than any
					// transform in the SPS. Both limits enable the same block sizes.
					range.log2_max_transform_skip_block_size_minus2 =
							static_cast<std::uint8_t>(std::min(signalled, max_transform_size_minus2));
				}
				FLAG(range.cross_component_prediction_enabled_flag);
				FLAG(range.chroma_qp_offset_list_enabled_flag);
				if (range.chroma_qp_offset_list_enabled_flag) {
					UE(range.diff_cu_chroma_qp_offset_depth, sps.log2_diff_max_min_luma_coding_block_size);
					UE(range.chroma_qp_offset_list_len_minus1, 5);
					for (unsigned i = 0; i <= range.chroma_qp_offset_list_len_minus1; ++i) {
						SE(range.cb_qp_offset_list[i], -12, 12);
						SE(range.cr_qp_offset_list[i], -12, 12);
					}
				}
				UE(range.log2_sao_offset_scale_luma, std::max(0, sps.bit_depth_luma_minus8 - 2));
				UE(range.log2_sao_offset_scale_chroma, std::max(0, sps.bit_depth_chroma_minus8 - 2));
			} else if (flags & 0x80) {
				// A profile without range-extension support ignores the extension tail.
				return {};
			}
			if (multilayer) {
				// Read base-layer-compatible syntax from Annex F. Inter-layer mapping
				// is outside the editor's single-layer decode path.
				VIDEO_EDITOR_TRY(bits.skip(1));
				if (VIDEO_EDITOR_TRY(bits.bits(1)) || VIDEO_EDITOR_TRY(bits.ue(62)) || VIDEO_EDITOR_TRY(bits.bits(1))) {
					if (pps.pps_scc_extension_flag) {
						return bits.fail("combined inter-layer PPS mapping and SCC", true);
					}
					return {};
				}
			}
			if (three_d) {
				if (pps.pps_scc_extension_flag) {
					return bits.fail("combined H.265 3D and SCC picture extensions", true);
				}
				return {};
			}
			if (pps.pps_scc_extension_flag) {
				auto &scc = pps.pps_scc_extension_params;
				FLAG(scc.pps_curr_pic_ref_enabled_flag);
				FLAG(scc.residual_adaptive_colour_transform_enabled_flag);
				if (scc.residual_adaptive_colour_transform_enabled_flag) {
					FLAG(scc.pps_slice_act_qp_offsets_present_flag);
					SE(scc.pps_act_y_qp_offset_plus5, -7, 17);
					SE(scc.pps_act_cb_qp_offset_plus5, -7, 17);
					SE(scc.pps_act_cr_qp_offset_plus3, -9, 15);
				}
				FLAG(scc.pps_palette_predictor_initializers_present_flag);
				if (scc.pps_palette_predictor_initializers_present_flag) {
					UE(scc.pps_num_palette_predictor_initializer, 128);
					if (scc.pps_num_palette_predictor_initializer) {
						FLAG(scc.monochrome_palette_flag);
						UE(scc.luma_bit_depth_entry_minus8, 8);
						if (!scc.monochrome_palette_flag) { UE(scc.chroma_bit_depth_entry_minus8, 8); }
						for (unsigned c = 0; c < (scc.monochrome_palette_flag ? 1U : 3U); ++c) {
							for (unsigned i = 0; i < scc.pps_num_palette_predictor_initializer; ++i) {
								READ(scc.pps_palette_predictor_initializer[c][i],
									 8 + (c == 0 ? scc.luma_bit_depth_entry_minus8
												 : scc.chroma_bit_depth_entry_minus8));
							}
						}
					}
				}
			} else if (flags & 0x10) {
				return {};
			}
			return (flags & 15) ? Syntax_Result<void>{} : bits.align();
		}

		Syntax_Result<void> read_tile_sizes(Bits &bits, std::span<std::uint32_t> sizes_minus1, std::uint32_t total,
											bool uniform) {
			std::uint32_t consumed = 0;
			for (std::size_t i = 0; i < sizes_minus1.size(); ++i) {
				std::uint32_t size;
				if (uniform) {
					size = static_cast<std::uint32_t>((i + 1) * total / sizes_minus1.size() -
													  i * total / sizes_minus1.size());
				} else if (i + 1 == sizes_minus1.size()) {
					size = total - consumed;
				} else {
					size = VIDEO_EDITOR_TRY(
								   bits.ue(total - consumed - static_cast<unsigned>(sizes_minus1.size() - i))) +
						   1;
				}
				sizes_minus1[i] = size - 1;
				consumed += size;
			}
			return {};
		}
	} // namespace

	Syntax_Result<Picture_Parameter_Set>
	parse_pps(const Nal_View &nal, std::span<const std::optional<Sequence_Parameter_Set>, 16> sequences) {
		Bits                  bits(nal.payload());
		Picture_Parameter_Set pps;
		UE(pps.id, 63);
		UE(pps.sps_id, 15);
		if (!sequences[pps.sps_id]) {
			return std::unexpected(Syntax_Error{bits.position(), "PPS refers to an unavailable SPS", false, true});
		}
		const auto &sps = *sequences[pps.sps_id];
		pps.sps         = &sps;
		FLAG(pps.dependent_slice_segments_enabled_flag);
		FLAG(pps.output_flag_present_flag);
		READ(pps.num_extra_slice_header_bits, 3);
		FLAG(pps.sign_data_hiding_enabled_flag);
		FLAG(pps.cabac_init_present_flag);
		UE(pps.num_ref_idx_l0_default_active_minus1, 14);
		UE(pps.num_ref_idx_l1_default_active_minus1, 14);
		SE(pps.init_qp_minus26, -26 - 6 * sps.bit_depth_luma_minus8, 25);
		FLAG(pps.constrained_intra_pred_flag);
		FLAG(pps.transform_skip_enabled_flag);
		FLAG(pps.cu_qp_delta_enabled_flag);
		if (pps.cu_qp_delta_enabled_flag) {
			UE(pps.diff_cu_qp_delta_depth, sps.log2_diff_max_min_luma_coding_block_size);
		}
		SE(pps.cb_qp_offset, -12, 12);
		SE(pps.cr_qp_offset, -12, 12);
		FLAG(pps.slice_chroma_qp_offsets_present_flag);
		FLAG(pps.weighted_pred_flag);
		FLAG(pps.weighted_bipred_flag);
		FLAG(pps.transquant_bypass_enabled_flag);
		FLAG(pps.tiles_enabled_flag);
		FLAG(pps.entropy_coding_sync_enabled_flag);
		const auto log2_ctb =
				sps.log2_min_luma_coding_block_size_minus3 + 3 + sps.log2_diff_max_min_luma_coding_block_size;
		const auto ctb_size  = 1U << log2_ctb;
		pps.PicWidthInCtbsY  = (sps.pic_width_in_luma_samples - 1) / ctb_size + 1;
		pps.PicHeightInCtbsY = (sps.pic_height_in_luma_samples - 1) / ctb_size + 1;
		if (std::uint64_t{pps.PicWidthInCtbsY} * pps.PicHeightInCtbsY > UINT32_MAX) {
			return bits.fail("picture has too many CTBs for hardware decoding", true);
		}
		if (pps.tiles_enabled_flag) {
			UE(pps.num_tile_columns_minus1, std::min(pps.PicWidthInCtbsY - 1, 19U));
			UE(pps.num_tile_rows_minus1, std::min(pps.PicHeightInCtbsY - 1, 21U));
			FLAG(pps.uniform_spacing_flag);
			VIDEO_EDITOR_TRY(read_tile_sizes(bits,
											 std::span(pps.column_width_minus1).first(pps.num_tile_columns_minus1 + 1),
											 pps.PicWidthInCtbsY, pps.uniform_spacing_flag));
			VIDEO_EDITOR_TRY(read_tile_sizes(bits, std::span(pps.row_height_minus1).first(pps.num_tile_rows_minus1 + 1),
											 pps.PicHeightInCtbsY, pps.uniform_spacing_flag));
			FLAG(pps.loop_filter_across_tiles_enabled_flag);
		}
		FLAG(pps.loop_filter_across_slices_enabled_flag);
		FLAG(pps.deblocking_filter_control_present_flag);
		if (pps.deblocking_filter_control_present_flag) {
			FLAG(pps.deblocking_filter_override_enabled_flag);
			FLAG(pps.deblocking_filter_disabled_flag);
			if (!pps.deblocking_filter_disabled_flag) {
				SE(pps.beta_offset_div2, -6, 6);
				SE(pps.tc_offset_div2, -6, 6);
			}
		}
		FLAG(pps.scaling_list_data_present_flag);
		if (pps.scaling_list_data_present_flag) {
			VIDEO_EDITOR_TRY(read_scaling(bits, pps.scaling_list));
		}
		FLAG(pps.lists_modification_present_flag);
		UE(pps.log2_parallel_merge_level_minus2, log2_ctb - 2);
		FLAG(pps.slice_segment_header_extension_present_flag);
		VIDEO_EDITOR_TRY(read_pps_extensions(bits, pps));
		return pps;
	}

	namespace {
		Syntax_Result<unsigned> read_reference_sets(Bits &bits, Slice_Header &slice,
													const Sequence_Parameter_Set &sps) {
			READ(slice.pic_order_cnt_lsb, sps.log2_max_pic_order_cnt_lsb_minus4 + 4);
			FLAG(slice.short_term_ref_pic_set_sps_flag);
			const auto sets = std::span(sps.short_term_ref_pic_set).first(sps.num_short_term_ref_pic_sets);
			const Short_Term_Reference_Set *active;
			if (!slice.short_term_ref_pic_set_sps_flag) {
				const auto begin                  = bits.syntax_position();
				slice.short_term_ref_pic_sets     = VIDEO_EDITOR_TRY(read_short_term_set(bits, sets, true));
				slice.short_term_ref_pic_set_size = bits.syntax_position() - begin;
				active                            = &slice.short_term_ref_pic_sets;
			} else {
				if (sets.empty()) {
					return bits.fail("slice selects an unavailable SPS reference set");
				}
				READ(slice.short_term_ref_pic_set_idx, std::bit_width(static_cast<unsigned>(sets.size() - 1)));
				if (slice.short_term_ref_pic_set_idx >= sets.size()) {
					return bits.fail("invalid SPS reference set index");
				}
				active = &sets[slice.short_term_ref_pic_set_idx];
			}
			unsigned used = 0;
			for (unsigned i = 0; i < active->NumNegativePics; ++i) {
				used += active->UsedByCurrPicS0[i];
			}
			for (unsigned i = 0; i < active->NumPositivePics; ++i) {
				used += active->UsedByCurrPicS1[i];
			}
			if (sps.long_term_ref_pics_present_flag) {
				if (sps.num_long_term_ref_pics_sps) {
					UE(slice.num_long_term_sps, std::min<unsigned>(16, sps.num_long_term_ref_pics_sps));
				}
				UE(slice.num_long_term_pics, 16U - slice.num_long_term_sps);
				const auto count = unsigned(slice.num_long_term_sps) + slice.num_long_term_pics;
				for (unsigned i = 0; i < count; ++i) {
					if (i < slice.num_long_term_sps) {
						READ(slice.lt_idx_sps[i], std::bit_width(unsigned(sps.num_long_term_ref_pics_sps) - 1));
						if (slice.lt_idx_sps[i] >= sps.num_long_term_ref_pics_sps) {
							return bits.fail("invalid long-term SPS reference index");
						}
						used += sps.used_by_curr_pic_lt_sps_flag[slice.lt_idx_sps[i]];
					} else {
						READ(slice.poc_lsb_lt[i], sps.log2_max_pic_order_cnt_lsb_minus4 + 4);
						FLAG(slice.used_by_curr_pic_lt_flag[i]);
						used += slice.used_by_curr_pic_lt_flag[i];
					}
					FLAG(slice.delta_poc_msb_present_flag[i]);
					if (slice.delta_poc_msb_present_flag[i]) {
						UE(slice.delta_poc_msb_cycle_lt[i], UINT32_MAX >> (sps.log2_max_pic_order_cnt_lsb_minus4 + 4));
					}
				}
			}
			if (sps.temporal_mvp_enabled_flag) { FLAG(slice.temporal_mvp_enabled_flag); }
			return used;
		}

		Syntax_Result<void> read_ref_list(Bits &bits, unsigned active_count, unsigned total, std::uint8_t &modified,
										  std::uint8_t (&entries)[15]) {
			FLAG(modified);
			if (modified) {
				const auto width = std::bit_width(total - 1);
				for (unsigned i = 0; i < active_count; ++i) {
					READ(entries[i], width);
					if (entries[i] >= total) {
						return bits.fail("reference list entry is out of range");
					}
				}
			}
			return {};
		}

		Syntax_Result<void> read_weight_list(Bits &bits, const Slice_Header &slice, bool list1, unsigned count,
											 std::uint8_t (&luma_flags)[15], std::uint8_t (&chroma_flags)[15],
											 std::int8_t (&luma_weights)[15], std::int16_t (&luma_offsets)[15],
											 std::int8_t (&chroma_weights)[15][2],
											 std::int16_t (&chroma_offsets)[15][2]) {
			const auto &pps   = *slice.pps;
			const auto &sps   = *pps.sps;
			const auto &lists = slice.ref_pic_list_modification;
			const auto  total = static_cast<unsigned>(slice.NumPocTotalCurr);
			const bool  modified =
					list1 ? lists.ref_pic_list_modification_flag_l1 : lists.ref_pic_list_modification_flag_l0;
			const auto *entries    = list1 ? lists.list_entry_l1 : lists.list_entry_l0;
			const auto  is_current = [&](unsigned i) {
				if (!pps.pps_scc_extension_params.pps_curr_pic_ref_enabled_flag) {
					return false;
				}
				if (modified) {
					return entries[i] == total - 1;
				}
				return i % total == total - 1 || (!list1 && total > count && i + 1 == count);
			};
			const bool high_precision = sps.sps_extension_params.high_precision_offsets_enabled_flag;
			const int  half_luma      = 1 << (high_precision ? sps.bit_depth_luma_minus8 + 7 : 7);
			const int  half_chroma    = 1 << (high_precision ? sps.bit_depth_chroma_minus8 + 7 : 7);
			for (unsigned i = 0; i < count; ++i) {
				if (!is_current(i)) {
					FLAG(luma_flags[i]);
				}
			}
			if (sps.chroma_array_type) {
				for (unsigned i = 0; i < count; ++i) {
					if (!is_current(i)) {
						FLAG(chroma_flags[i]);
					}
				}
			}
			for (unsigned i = 0; i < count; ++i) {
				if (luma_flags[i]) {
					SE(luma_weights[i], -128, 127);
					SE(luma_offsets[i], -half_luma, half_luma - 1);
				}
				if (chroma_flags[i]) {
					for (unsigned c = 0; c < 2; ++c) {
						SE(chroma_weights[i][c], -128, 127);
						SE(chroma_offsets[i][c], -4 * half_chroma, 4 * half_chroma - 1);
					}
				}
			}
			return {};
		}

		Syntax_Result<void> read_inter_slice(Bits &bits, Slice_Header &slice) {
			const auto &pps                    = *slice.pps;
			const auto &sps                    = *pps.sps;
			const auto  total                  = static_cast<unsigned>(slice.NumPocTotalCurr);
			const bool  bipred                 = slice.type == kSlice_B;
			slice.num_ref_idx_l0_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
			slice.num_ref_idx_l1_active_minus1 = bipred ? pps.num_ref_idx_l1_default_active_minus1 : 0;
			if (VIDEO_EDITOR_TRY(bits.bits(1))) {
				UE(slice.num_ref_idx_l0_active_minus1, 14);
				if (bipred) { UE(slice.num_ref_idx_l1_active_minus1, 14); }
			}
			const unsigned l0_count = slice.num_ref_idx_l0_active_minus1 + 1;
			const unsigned l1_count = slice.num_ref_idx_l1_active_minus1 + 1;
			auto          &lists    = slice.ref_pic_list_modification;
			if (pps.lists_modification_present_flag && total > 1) {
				VIDEO_EDITOR_TRY(read_ref_list(bits, l0_count, total, lists.ref_pic_list_modification_flag_l0,
											   lists.list_entry_l0));
				if (bipred) {
					VIDEO_EDITOR_TRY(read_ref_list(bits, l1_count, total, lists.ref_pic_list_modification_flag_l1,
												   lists.list_entry_l1));
				}
			}
			if (bipred) { FLAG(slice.mvd_l1_zero_flag); }
			if (pps.cabac_init_present_flag) { FLAG(slice.cabac_init_flag); }
			if (slice.temporal_mvp_enabled_flag) {
				if (bipred) { FLAG(slice.collocated_from_l0_flag); }
				const auto maximum = slice.collocated_from_l0_flag ? slice.num_ref_idx_l0_active_minus1
																   : slice.num_ref_idx_l1_active_minus1;
				if (maximum) { UE(slice.collocated_ref_idx, maximum); }
			}
			if (bipred ? pps.weighted_bipred_flag : pps.weighted_pred_flag) {
				auto &w = slice.pred_weight_table;
				UE(w.luma_log2_weight_denom, 7);
				if (sps.chroma_array_type) {
					SE(w.delta_chroma_log2_weight_denom, -w.luma_log2_weight_denom, 7 - w.luma_log2_weight_denom);
				}
				VIDEO_EDITOR_TRY(read_weight_list(bits, slice, false, l0_count, w.luma_weight_l0_flag,
												  w.chroma_weight_l0_flag, w.delta_luma_weight_l0, w.luma_offset_l0,
												  w.delta_chroma_weight_l0, w.delta_chroma_offset_l0));
				if (bipred) {
					VIDEO_EDITOR_TRY(read_weight_list(bits, slice, true, l1_count, w.luma_weight_l1_flag,
													  w.chroma_weight_l1_flag, w.delta_luma_weight_l1, w.luma_offset_l1,
													  w.delta_chroma_weight_l1, w.delta_chroma_offset_l1));
				}
			}
			UE(slice.five_minus_max_num_merge_cand, 4);
			const auto mv_resolution = sps.sps_scc_extension_params.motion_vector_resolution_control_idc;
			if (mv_resolution == 2) {
				FLAG(slice.use_integer_mv_flag);
			} else {
				slice.use_integer_mv_flag = mv_resolution == 1;
			}
			return {};
		}

		Syntax_Result<void> read_independent_slice(Bits &bits, Slice_Header &slice, Nal_Unit_Type nal_type) {
			const auto &pps = *slice.pps;
			const auto &sps = *pps.sps;
			VIDEO_EDITOR_TRY(bits.skip(pps.num_extra_slice_header_bits));
			UE(slice.type, kSlice_I);
			if (pps.output_flag_present_flag) { FLAG(slice.pic_output_flag); }
			if (sps.separate_colour_plane_flag) {
				READ(slice.colour_plane_id, 2);
				if (slice.colour_plane_id == 3) {
					return bits.fail("invalid separate colour plane");
				}
			}
			slice.NumPocTotalCurr = pps.pps_scc_extension_params.pps_curr_pic_ref_enabled_flag;
			if (!is_idr_type(nal_type)) {
				slice.NumPocTotalCurr += VIDEO_EDITOR_TRY(read_reference_sets(bits, slice, sps));
			}
			if (sps.sample_adaptive_offset_enabled_flag) {
				FLAG(slice.sao_luma_flag);
				if (sps.chroma_array_type) { FLAG(slice.sao_chroma_flag); }
			}
			if (slice.type != kSlice_I) {
				if (!slice.NumPocTotalCurr) {
					return bits.fail("inter slice has no active picture references");
				}
				VIDEO_EDITOR_TRY(read_inter_slice(bits, slice));
			}
			SE(slice.qp_delta, -87, 77);
			if (pps.slice_chroma_qp_offsets_present_flag) {
				SE(slice.cb_qp_offset, -12, 12);
				SE(slice.cr_qp_offset, -12, 12);
			}
			if (pps.pps_scc_extension_params.pps_slice_act_qp_offsets_present_flag) {
				SE(slice.slice_act_y_qp_offset, -12, 12);
				SE(slice.slice_act_cb_qp_offset, -12, 12);
				SE(slice.slice_act_cr_qp_offset, -12, 12);
			}
			if (pps.pps_extension_params.chroma_qp_offset_list_enabled_flag) {
				FLAG(slice.cu_chroma_qp_offset_enabled_flag);
			}
			slice.deblocking_filter_disabled_flag = pps.deblocking_filter_disabled_flag;
			slice.beta_offset_div2                = pps.beta_offset_div2;
			slice.tc_offset_div2                  = pps.tc_offset_div2;
			if (pps.deblocking_filter_override_enabled_flag) { FLAG(slice.deblocking_filter_override_flag); }
			if (slice.deblocking_filter_override_flag) {
				FLAG(slice.deblocking_filter_disabled_flag);
				if (!slice.deblocking_filter_disabled_flag) {
					SE(slice.beta_offset_div2, -6, 6);
					SE(slice.tc_offset_div2, -6, 6);
				}
			}
			slice.loop_filter_across_slices_enabled_flag = pps.loop_filter_across_slices_enabled_flag;
			if (pps.loop_filter_across_slices_enabled_flag &&
				(slice.sao_luma_flag || slice.sao_chroma_flag || !slice.deblocking_filter_disabled_flag)) {
				FLAG(slice.loop_filter_across_slices_enabled_flag);
			}
			return {};
		}
	} // namespace

	Syntax_Result<Slice_Header> parse_slice_header(const Nal_View                                           &nal,
												   std::span<const std::optional<Picture_Parameter_Set>, 64> pictures) {
		Bits         bits(nal.payload());
		Slice_Header slice;
		FLAG(slice.first_slice_segment_in_pic_flag);
		if (is_irap_type(nal.type)) { FLAG(slice.no_output_of_prior_pics_flag); }
		UE(slice.pps_id, 63);
		if (!pictures[slice.pps_id] || !pictures[slice.pps_id]->sps) {
			return bits.fail("slice refers to an unavailable PPS or SPS");
		}
		slice.pps       = &*pictures[slice.pps_id];
		const auto &pps = *slice.pps;
		if (!slice.first_slice_segment_in_pic_flag) {
			if (pps.dependent_slice_segments_enabled_flag) { FLAG(slice.dependent_slice_segment_flag); }
			const auto ctb_count = pps.PicWidthInCtbsY * pps.PicHeightInCtbsY;
			READ(slice.segment_address, std::bit_width(ctb_count - 1));
			if (slice.segment_address == 0 || slice.segment_address >= ctb_count) {
				return bits.fail("invalid slice segment address");
			}
		}
		if (!slice.dependent_slice_segment_flag) {
			VIDEO_EDITOR_TRY(read_independent_slice(bits, slice, nal.type));
		}

		if (pps.tiles_enabled_flag || pps.entropy_coding_sync_enabled_flag) {
			const unsigned columns = pps.tiles_enabled_flag ? pps.num_tile_columns_minus1 + 1U : 1U;
			const unsigned rows =
					pps.entropy_coding_sync_enabled_flag ? pps.PicHeightInCtbsY : pps.num_tile_rows_minus1 + 1U;
			UE(slice.num_entry_point_offsets, columns * rows - 1);
			if (slice.num_entry_point_offsets) {
				const auto width = VIDEO_EDITOR_TRY(bits.ue(31)) + 1;
				if (std::uint64_t{slice.num_entry_point_offsets} * width > nal.payload().size() * 8 - bits.position()) {
					return bits.fail("truncated slice entry-point offsets");
				}
				auto offsets = std::make_shared<std::vector<std::uint32_t>>();
				for (unsigned i = 0; i < slice.num_entry_point_offsets; ++i) {
					offsets->push_back(VIDEO_EDITOR_TRY(bits.bits(width)));
				}
				slice.entry_point_offsets = std::move(offsets);
			}
		}
		if (pps.slice_segment_header_extension_present_flag) {
			const auto count = VIDEO_EDITOR_TRY(bits.ue(256));
			VIDEO_EDITOR_TRY(bits.skip(8 * count));
		}
		VIDEO_EDITOR_TRY(bits.align());
		slice.header_bytes                 = bits.position() / 8;
		slice.n_emulation_prevention_bytes = bits.escapes();
		return slice;
	}

#undef READ
#undef FLAG
#undef UE
#undef SE
} // namespace video_editor::h265::decoder
