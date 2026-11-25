#pragma once

#include "h265/decoder_internal.hpp"

#include <algorithm>
#include <array>
#include <cstring>

namespace video_editor::h265::decoder {
	namespace {
		[[nodiscard]] StdVideoH265LevelIdc level_idc(std::uint8_t value) {
			switch (value) {
				case 30: return STD_VIDEO_H265_LEVEL_IDC_1_0;
				case 60: return STD_VIDEO_H265_LEVEL_IDC_2_0;
				case 63: return STD_VIDEO_H265_LEVEL_IDC_2_1;
				case 90: return STD_VIDEO_H265_LEVEL_IDC_3_0;
				case 93: return STD_VIDEO_H265_LEVEL_IDC_3_1;
				case 120: return STD_VIDEO_H265_LEVEL_IDC_4_0;
				case 123: return STD_VIDEO_H265_LEVEL_IDC_4_1;
				case 150: return STD_VIDEO_H265_LEVEL_IDC_5_0;
				case 153: return STD_VIDEO_H265_LEVEL_IDC_5_1;
				case 156: return STD_VIDEO_H265_LEVEL_IDC_5_2;
				case 180: return STD_VIDEO_H265_LEVEL_IDC_6_0;
				case 183: return STD_VIDEO_H265_LEVEL_IDC_6_1;
				case 186: return STD_VIDEO_H265_LEVEL_IDC_6_2;
				default: return STD_VIDEO_H265_LEVEL_IDC_INVALID;
			}
		}

		void lower_profile(const Profile_Tier_Level &source, StdVideoH265ProfileTierLevel &target) {
			target.flags.general_tier_flag = source.tier_flag;
			target.flags.general_progressive_source_flag = source.progressive_source_flag;
			target.flags.general_interlaced_source_flag = source.interlaced_source_flag;
			target.flags.general_non_packed_constraint_flag = source.non_packed_constraint_flag;
			target.flags.general_frame_only_constraint_flag = source.frame_only_constraint_flag;
			target.general_profile_idc = static_cast<StdVideoH265ProfileIdc>(source.profile_idc);
			target.general_level_idc = level_idc(source.level_idc);
		}

		template<typename Latency>
		void lower_dpb(const std::uint8_t *buffering, const std::uint8_t *reorder, const Latency *latency,
				StdVideoH265DecPicBufMgr &target) {
			for (std::size_t index = 0; index < STD_VIDEO_H265_SUBLAYERS_LIST_SIZE; ++index) {
				target.max_dec_pic_buffering_minus1[index] = buffering[index];
				target.max_num_reorder_pics[index] = reorder[index];
				target.max_latency_increase_plus1[index] = latency[index];
			}
		}

		void copy_scaling_list(std::span<std::uint8_t> destination, const std::uint8_t *source, std::uint8_t edge) {
			(void) edge;
			std::copy_n(source, destination.size(), destination.begin());
		}

		void lower_scaling(const Scaling_Lists &source, StdVideoH265ScalingLists &target) {
			for (std::size_t index = 0; index < 6U; ++index) {
				copy_scaling_list(target.ScalingList4x4[index], source.scaling_lists_4x4[index], 4U);
				copy_scaling_list(target.ScalingList8x8[index], source.scaling_lists_8x8[index], 8U);
				copy_scaling_list(target.ScalingList16x16[index], source.scaling_lists_16x16[index], 8U);
				target.ScalingListDCCoef16x16[index] =
						static_cast<std::uint8_t>(source.scaling_list_dc_coef_minus8_16x16[index] + 8);
				if (index < 2U) {
					copy_scaling_list(target.ScalingList32x32[index], source.scaling_lists_32x32[index], 8U);
					target.ScalingListDCCoef32x32[index] =
							static_cast<std::uint8_t>(source.scaling_list_dc_coef_minus8_32x32[index] + 8);
				}
			}
		}

		void lower_rps(const Short_Term_Reference_Set &source, StdVideoH265ShortTermRefPicSet &target) {
			// Store a semantically equivalent explicit RPS. Slice headers select
			// the set by index; prediction syntax is irrelevant after parameters
			// have been materialized into the session object.
			target.num_negative_pics = std::min<std::uint8_t>(source.NumNegativePics, STD_VIDEO_H265_MAX_DPB_SIZE);
			target.num_positive_pics = std::min<std::uint8_t>(source.NumPositivePics, STD_VIDEO_H265_MAX_DPB_SIZE);
			std::int32_t previous = 0;
			for (std::uint8_t index = 0; index < target.num_negative_pics; ++index) {
				target.delta_poc_s0_minus1[index] = static_cast<std::uint16_t>(
						std::max<std::int32_t>(0, previous - source.DeltaPocS0[index] - 1));
				previous = source.DeltaPocS0[index];
				if (source.UsedByCurrPicS0[index] != 0U) {
					target.used_by_curr_pic_s0_flag |= static_cast<std::uint16_t>(1U << index);
				}
			}
			previous = 0;
			for (std::uint8_t index = 0; index < target.num_positive_pics; ++index) {
				target.delta_poc_s1_minus1[index] = static_cast<std::uint16_t>(
						std::max<std::int32_t>(0, source.DeltaPocS1[index] - previous - 1));
				previous = source.DeltaPocS1[index];
				if (source.UsedByCurrPicS1[index] != 0U) {
					target.used_by_curr_pic_s1_flag |= static_cast<std::uint16_t>(1U << index);
				}
			}
		}

		struct Parameter_Bundle {
			StdVideoH265ProfileTierLevel vps_profile_{};
			StdVideoH265DecPicBufMgr vps_dpb_{};
			StdVideoH265VideoParameterSet vps_{};
			StdVideoH265ProfileTierLevel sps_profile_{};
			StdVideoH265DecPicBufMgr sps_dpb_{};
			StdVideoH265ScalingLists sps_scaling_{};
			std::array<StdVideoH265ShortTermRefPicSet, STD_VIDEO_H265_MAX_SHORT_TERM_REF_PIC_SETS> rps_{};
			StdVideoH265LongTermRefPicsSps long_term_{};
			StdVideoH265SequenceParameterSetVui vui_{};
			StdVideoH265PredictorPaletteEntries sps_palette_{};
			StdVideoH265SequenceParameterSet sps_{};
			StdVideoH265ScalingLists pps_scaling_{};
			StdVideoH265PredictorPaletteEntries pps_palette_{};
			StdVideoH265PictureParameterSet pps_{};

			explicit Parameter_Bundle(const H265_Parameter_Snapshot &source) { lower(source); }

			void lower(const H265_Parameter_Snapshot &source) {
				const auto &vps = source.vps_;
				const auto &sps = source.sps_;
				const auto &pps = source.pps_;
				const auto profile_compatible = [&](std::uint8_t profile_idc) {
					return sps.profile_tier_level.profile_idc == profile_idc ||
						   (profile_idc < std::size(sps.profile_tier_level.profile_compatibility_flag) &&
							sps.profile_tier_level.profile_compatibility_flag[profile_idc] != 0U);
				};
				// PPS extension bits cannot enable coding tools that the active SPS
				// profile does not permit.  In particular, PS_A_VIDYO_3 carries an
				// intentionally incomplete Main-profile PPS extension; forwarding its
				// parsed tail as RExt/SCC state changes the hardware reconstruction.
				const bool range_profile = profile_compatible(kProfile_Range_Extension) ||
						profile_compatible(kProfile_High_Throughput) || sps.sps_range_extension_flag != 0U;
				const bool scc_profile = profile_compatible(kProfile_Screen_Content) ||
						sps.sps_scc_extension_flag != 0U;
				const bool has_sps_range_extension = (range_profile || scc_profile) && sps.sps_range_extension_flag;
				const bool has_sps_scc_extension = scc_profile && sps.sps_scc_extension_flag;
				const bool has_pps_range_extension = (range_profile || scc_profile) && pps.pps_range_extension_flag;
				const bool has_pps_scc_extension = scc_profile && pps.pps_scc_extension_flag;
				lower_profile(vps.profile_tier_level, vps_profile_);
				lower_dpb(vps.max_dec_pic_buffering_minus1, vps.max_num_reorder_pics,
						vps.max_latency_increase_plus1, vps_dpb_);
				vps_.flags.vps_temporal_id_nesting_flag = vps.temporal_id_nesting_flag;
				vps_.flags.vps_sub_layer_ordering_info_present_flag = vps.sub_layer_ordering_info_present_flag;
				vps_.flags.vps_timing_info_present_flag = vps.timing_info_present_flag;
				vps_.flags.vps_poc_proportional_to_timing_flag = vps.poc_proportional_to_timing_flag;
				vps_.vps_video_parameter_set_id = vps.id;
				vps_.vps_max_sub_layers_minus1 = vps.max_sub_layers_minus1;
				vps_.vps_num_units_in_tick = vps.num_units_in_tick;
				vps_.vps_time_scale = vps.time_scale;
				vps_.vps_num_ticks_poc_diff_one_minus1 = vps.num_ticks_poc_diff_one_minus1;
				vps_.pDecPicBufMgr = &vps_dpb_;
				vps_.pProfileTierLevel = &vps_profile_;

				lower_profile(sps.profile_tier_level, sps_profile_);
				lower_dpb(sps.max_dec_pic_buffering_minus1, sps.max_num_reorder_pics,
						sps.max_latency_increase_plus1, sps_dpb_);
				sps_.flags.sps_temporal_id_nesting_flag = sps.temporal_id_nesting_flag;
				sps_.flags.separate_colour_plane_flag = sps.separate_colour_plane_flag;
				sps_.flags.conformance_window_flag = sps.conformance_window_flag;
				sps_.flags.sps_sub_layer_ordering_info_present_flag = sps.sub_layer_ordering_info_present_flag;
				sps_.flags.scaling_list_enabled_flag = sps.scaling_list_enabled_flag;
				sps_.flags.sps_scaling_list_data_present_flag = sps.scaling_list_data_present_flag;
				sps_.flags.amp_enabled_flag = sps.amp_enabled_flag;
				sps_.flags.sample_adaptive_offset_enabled_flag = sps.sample_adaptive_offset_enabled_flag;
				sps_.flags.pcm_enabled_flag = sps.pcm_enabled_flag;
				sps_.flags.pcm_loop_filter_disabled_flag = sps.pcm_loop_filter_disabled_flag;
				sps_.flags.long_term_ref_pics_present_flag = sps.long_term_ref_pics_present_flag;
				sps_.flags.sps_temporal_mvp_enabled_flag = sps.temporal_mvp_enabled_flag;
				sps_.flags.strong_intra_smoothing_enabled_flag = sps.strong_intra_smoothing_enabled_flag;
				sps_.flags.vui_parameters_present_flag = sps.vui_parameters_present_flag;
				sps_.flags.sps_extension_present_flag = has_sps_range_extension || has_sps_scc_extension;
				sps_.flags.sps_range_extension_flag = has_sps_range_extension;
				const auto &range = sps.sps_extension_params;
				if (has_sps_range_extension) {
					sps_.flags.transform_skip_rotation_enabled_flag = range.transform_skip_rotation_enabled_flag;
					sps_.flags.transform_skip_context_enabled_flag = range.transform_skip_context_enabled_flag;
					sps_.flags.implicit_rdpcm_enabled_flag = range.implicit_rdpcm_enabled_flag;
					sps_.flags.explicit_rdpcm_enabled_flag = range.explicit_rdpcm_enabled_flag;
					sps_.flags.extended_precision_processing_flag = range.extended_precision_processing_flag;
					sps_.flags.intra_smoothing_disabled_flag = range.intra_smoothing_disabled_flag;
					sps_.flags.high_precision_offsets_enabled_flag = range.high_precision_offsets_enabled_flag;
					sps_.flags.persistent_rice_adaptation_enabled_flag = range.persistent_rice_adaptation_enabled_flag;
					sps_.flags.cabac_bypass_alignment_enabled_flag = range.cabac_bypass_alignment_enabled_flag;
				}
				const auto &scc = sps.sps_scc_extension_params;
				sps_.flags.sps_scc_extension_flag = has_sps_scc_extension;
				if (has_sps_scc_extension) {
					sps_.flags.sps_curr_pic_ref_enabled_flag = scc.sps_curr_pic_ref_enabled_flag;
					sps_.flags.palette_mode_enabled_flag = scc.palette_mode_enabled_flag;
					sps_.flags.sps_palette_predictor_initializers_present_flag =
							scc.sps_palette_predictor_initializers_present_flag;
					sps_.flags.intra_boundary_filtering_disabled_flag = scc.intra_boundary_filtering_disabled_flag;
				}
				sps_.chroma_format_idc = static_cast<StdVideoH265ChromaFormatIdc>(sps.chroma_format_idc);
				sps_.pic_width_in_luma_samples = sps.pic_width_in_luma_samples;
				sps_.pic_height_in_luma_samples = sps.pic_height_in_luma_samples;
				sps_.sps_video_parameter_set_id = sps.vps_id;
				sps_.sps_max_sub_layers_minus1 = sps.max_sub_layers_minus1;
				sps_.sps_seq_parameter_set_id = sps.id;
				sps_.bit_depth_luma_minus8 = sps.bit_depth_luma_minus8;
				sps_.bit_depth_chroma_minus8 = sps.bit_depth_chroma_minus8;
				sps_.log2_max_pic_order_cnt_lsb_minus4 = sps.log2_max_pic_order_cnt_lsb_minus4;
				sps_.log2_min_luma_coding_block_size_minus3 = sps.log2_min_luma_coding_block_size_minus3;
				sps_.log2_diff_max_min_luma_coding_block_size = sps.log2_diff_max_min_luma_coding_block_size;
				sps_.log2_min_luma_transform_block_size_minus2 = sps.log2_min_transform_block_size_minus2;
				sps_.log2_diff_max_min_luma_transform_block_size = sps.log2_diff_max_min_transform_block_size;
				sps_.max_transform_hierarchy_depth_inter = sps.max_transform_hierarchy_depth_inter;
				sps_.max_transform_hierarchy_depth_intra = sps.max_transform_hierarchy_depth_intra;
				sps_.num_short_term_ref_pic_sets = std::min<std::uint8_t>(
						sps.num_short_term_ref_pic_sets, STD_VIDEO_H265_MAX_SHORT_TERM_REF_PIC_SETS);
				sps_.num_long_term_ref_pics_sps = sps.num_long_term_ref_pics_sps;
				sps_.pcm_sample_bit_depth_luma_minus1 = sps.pcm_sample_bit_depth_luma_minus1;
				sps_.pcm_sample_bit_depth_chroma_minus1 = sps.pcm_sample_bit_depth_chroma_minus1;
				sps_.log2_min_pcm_luma_coding_block_size_minus3 = sps.log2_min_pcm_luma_coding_block_size_minus3;
				sps_.log2_diff_max_min_pcm_luma_coding_block_size = sps.log2_diff_max_min_pcm_luma_coding_block_size;
				if (has_sps_scc_extension) {
					sps_.palette_max_size = scc.palette_max_size;
					sps_.delta_palette_max_predictor_size = scc.delta_palette_max_predictor_size;
					sps_.motion_vector_resolution_control_idc = scc.motion_vector_resolution_control_idc;
					sps_.sps_num_palette_predictor_initializers_minus1 =
							scc.sps_num_palette_predictor_initializer_minus1;
				}
				sps_.conf_win_left_offset = sps.conf_win_left_offset;
				sps_.conf_win_right_offset = sps.conf_win_right_offset;
				sps_.conf_win_top_offset = sps.conf_win_top_offset;
				sps_.conf_win_bottom_offset = sps.conf_win_bottom_offset;
				sps_.pProfileTierLevel = &sps_profile_;
				sps_.pDecPicBufMgr = &sps_dpb_;
				if (sps.scaling_list_enabled_flag != 0U && sps.scaling_list_data_present_flag != 0U) {
					lower_scaling(sps.scaling_list, sps_scaling_);
					sps_.pScalingLists = &sps_scaling_;
				}
				for (std::uint8_t index = 0; index < sps_.num_short_term_ref_pic_sets; ++index) {
					lower_rps(sps.short_term_ref_pic_set[index], rps_[index]);
				}
				sps_.pShortTermRefPicSet = rps_.data();
				for (std::uint8_t index = 0; index < sps_.num_long_term_ref_pics_sps; ++index) {
					long_term_.lt_ref_pic_poc_lsb_sps[index] = sps.lt_ref_pic_poc_lsb_sps[index];
					if (sps.used_by_curr_pic_lt_sps_flag[index] != 0U) {
						long_term_.used_by_curr_pic_lt_sps_flag |= 1U << index;
					}
				}
				if (sps.long_term_ref_pics_present_flag != 0U) { sps_.pLongTermRefPicsSps = &long_term_; }
				if (sps.vui_parameters_present_flag != 0U) {
					const auto &vui = sps.vui_params;
					vui_.flags.aspect_ratio_info_present_flag = vui.aspect_ratio_info_present_flag;
					vui_.flags.overscan_info_present_flag = vui.overscan_info_present_flag;
					vui_.flags.overscan_appropriate_flag = vui.overscan_appropriate_flag;
					vui_.flags.video_signal_type_present_flag = vui.video_signal_type_present_flag;
					vui_.flags.video_full_range_flag = vui.video_full_range_flag;
					vui_.flags.colour_description_present_flag = vui.colour_description_present_flag;
					vui_.flags.chroma_loc_info_present_flag = vui.chroma_loc_info_present_flag;
					vui_.flags.neutral_chroma_indication_flag = vui.neutral_chroma_indication_flag;
					vui_.flags.field_seq_flag = vui.field_seq_flag;
					vui_.flags.frame_field_info_present_flag = vui.frame_field_info_present_flag;
					vui_.flags.default_display_window_flag = vui.default_display_window_flag;
					vui_.flags.vui_timing_info_present_flag = vui.timing_info_present_flag;
					vui_.flags.vui_poc_proportional_to_timing_flag = vui.poc_proportional_to_timing_flag;
					vui_.flags.bitstream_restriction_flag = vui.bitstream_restriction_flag;
					vui_.flags.tiles_fixed_structure_flag = vui.tiles_fixed_structure_flag;
					vui_.flags.motion_vectors_over_pic_boundaries_flag = vui.motion_vectors_over_pic_boundaries_flag;
					vui_.flags.restricted_ref_pic_lists_flag = vui.restricted_ref_pic_lists_flag;
					vui_.aspect_ratio_idc = static_cast<StdVideoH265AspectRatioIdc>(vui.aspect_ratio_idc);
					vui_.sar_width = vui.sar_width;
					vui_.sar_height = vui.sar_height;
					vui_.video_format = vui.video_format;
					vui_.colour_primaries = vui.colour_primaries;
					vui_.transfer_characteristics = vui.transfer_characteristics;
					vui_.matrix_coeffs = vui.matrix_coefficients;
					vui_.chroma_sample_loc_type_top_field = vui.chroma_sample_loc_type_top_field;
					vui_.chroma_sample_loc_type_bottom_field = vui.chroma_sample_loc_type_bottom_field;
					vui_.def_disp_win_left_offset = static_cast<std::uint16_t>(vui.def_disp_win_left_offset);
					vui_.def_disp_win_right_offset = static_cast<std::uint16_t>(vui.def_disp_win_right_offset);
					vui_.def_disp_win_top_offset = static_cast<std::uint16_t>(vui.def_disp_win_top_offset);
					vui_.def_disp_win_bottom_offset = static_cast<std::uint16_t>(vui.def_disp_win_bottom_offset);
					vui_.vui_num_units_in_tick = vui.num_units_in_tick;
					vui_.vui_time_scale = vui.time_scale;
					vui_.vui_num_ticks_poc_diff_one_minus1 = vui.num_ticks_poc_diff_one_minus1;
					vui_.min_spatial_segmentation_idc = vui.min_spatial_segmentation_idc;
					vui_.max_bytes_per_pic_denom = vui.max_bytes_per_pic_denom;
					vui_.max_bits_per_min_cu_denom = vui.max_bits_per_min_cu_denom;
					vui_.log2_max_mv_length_horizontal = vui.log2_max_mv_length_horizontal;
					vui_.log2_max_mv_length_vertical = vui.log2_max_mv_length_vertical;
					sps_.pSequenceParameterSetVui = &vui_;
				}
				if (has_sps_scc_extension && scc.sps_palette_predictor_initializers_present_flag != 0U) {
					for (std::size_t component = 0; component < 3U; ++component) {
						for (std::size_t index = 0; index < 128U; ++index) {
							sps_palette_.PredictorPaletteEntries[component][index] =
									static_cast<std::uint16_t>(scc.sps_palette_predictor_initializer[component][index]);
						}
					}
					sps_.pPredictorPaletteEntries = &sps_palette_;
				}

				pps_.flags.dependent_slice_segments_enabled_flag = pps.dependent_slice_segments_enabled_flag;
				pps_.flags.output_flag_present_flag = pps.output_flag_present_flag;
				pps_.flags.sign_data_hiding_enabled_flag = pps.sign_data_hiding_enabled_flag;
				pps_.flags.cabac_init_present_flag = pps.cabac_init_present_flag;
				pps_.flags.constrained_intra_pred_flag = pps.constrained_intra_pred_flag;
				pps_.flags.transform_skip_enabled_flag = pps.transform_skip_enabled_flag;
				pps_.flags.cu_qp_delta_enabled_flag = pps.cu_qp_delta_enabled_flag;
				pps_.flags.pps_slice_chroma_qp_offsets_present_flag = pps.slice_chroma_qp_offsets_present_flag;
				pps_.flags.weighted_pred_flag = pps.weighted_pred_flag;
				pps_.flags.weighted_bipred_flag = pps.weighted_bipred_flag;
				pps_.flags.transquant_bypass_enabled_flag = pps.transquant_bypass_enabled_flag;
				pps_.flags.tiles_enabled_flag = pps.tiles_enabled_flag;
				pps_.flags.entropy_coding_sync_enabled_flag = pps.entropy_coding_sync_enabled_flag;
				pps_.flags.uniform_spacing_flag = pps.uniform_spacing_flag;
				pps_.flags.loop_filter_across_tiles_enabled_flag = pps.loop_filter_across_tiles_enabled_flag;
				pps_.flags.pps_loop_filter_across_slices_enabled_flag = pps.loop_filter_across_slices_enabled_flag;
				pps_.flags.deblocking_filter_control_present_flag = pps.deblocking_filter_control_present_flag;
				pps_.flags.deblocking_filter_override_enabled_flag = pps.deblocking_filter_override_enabled_flag;
				pps_.flags.pps_deblocking_filter_disabled_flag = pps.deblocking_filter_disabled_flag;
				pps_.flags.pps_scaling_list_data_present_flag = pps.scaling_list_data_present_flag;
				pps_.flags.lists_modification_present_flag = pps.lists_modification_present_flag;
				pps_.flags.slice_segment_header_extension_present_flag = pps.slice_segment_header_extension_present_flag;
				pps_.flags.pps_extension_present_flag = has_pps_range_extension || has_pps_scc_extension;
				pps_.flags.pps_range_extension_flag = has_pps_range_extension;
				const auto &pps_range = pps.pps_extension_params;
				if (has_pps_range_extension) {
					pps_.flags.cross_component_prediction_enabled_flag = pps_range.cross_component_prediction_enabled_flag;
					pps_.flags.chroma_qp_offset_list_enabled_flag = pps_range.chroma_qp_offset_list_enabled_flag;
				}
				const auto &pps_scc = pps.pps_scc_extension_params;
				if (has_pps_scc_extension) {
					pps_.flags.pps_curr_pic_ref_enabled_flag = pps_scc.pps_curr_pic_ref_enabled_flag;
					pps_.flags.residual_adaptive_colour_transform_enabled_flag =
							pps_scc.residual_adaptive_colour_transform_enabled_flag;
					pps_.flags.pps_slice_act_qp_offsets_present_flag = pps_scc.pps_slice_act_qp_offsets_present_flag;
					pps_.flags.pps_palette_predictor_initializers_present_flag =
							pps_scc.pps_palette_predictor_initializers_present_flag;
					pps_.flags.monochrome_palette_flag = pps_scc.monochrome_palette_flag;
				}
				pps_.pps_pic_parameter_set_id = static_cast<std::uint8_t>(pps.id);
				pps_.pps_seq_parameter_set_id = static_cast<std::uint8_t>(pps.sps_id);
				pps_.sps_video_parameter_set_id = sps.vps_id;
				pps_.num_extra_slice_header_bits = pps.num_extra_slice_header_bits;
				pps_.num_ref_idx_l0_default_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
				pps_.num_ref_idx_l1_default_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
				pps_.init_qp_minus26 = pps.init_qp_minus26;
				pps_.diff_cu_qp_delta_depth = pps.diff_cu_qp_delta_depth;
				pps_.pps_cb_qp_offset = pps.cb_qp_offset;
				pps_.pps_cr_qp_offset = pps.cr_qp_offset;
				pps_.pps_beta_offset_div2 = pps.beta_offset_div2;
				pps_.pps_tc_offset_div2 = pps.tc_offset_div2;
				pps_.log2_parallel_merge_level_minus2 = pps.log2_parallel_merge_level_minus2;
				if (has_pps_range_extension) {
					pps_.log2_max_transform_skip_block_size_minus2 =
							static_cast<std::uint8_t>(pps_range.log2_max_transform_skip_block_size_minus2);
					pps_.diff_cu_chroma_qp_offset_depth = pps_range.diff_cu_chroma_qp_offset_depth;
					pps_.chroma_qp_offset_list_len_minus1 = pps_range.chroma_qp_offset_list_len_minus1;
					std::memcpy(pps_.cb_qp_offset_list, pps_range.cb_qp_offset_list, sizeof(pps_.cb_qp_offset_list));
					std::memcpy(pps_.cr_qp_offset_list, pps_range.cr_qp_offset_list, sizeof(pps_.cr_qp_offset_list));
					pps_.log2_sao_offset_scale_luma = pps_range.log2_sao_offset_scale_luma;
					pps_.log2_sao_offset_scale_chroma = pps_range.log2_sao_offset_scale_chroma;
				}
				if (has_pps_scc_extension) {
					pps_.pps_act_y_qp_offset_plus5 = static_cast<std::int8_t>(pps_scc.pps_act_y_qp_offset_plus5);
					pps_.pps_act_cb_qp_offset_plus5 = static_cast<std::int8_t>(pps_scc.pps_act_cb_qp_offset_plus5);
					pps_.pps_act_cr_qp_offset_plus3 = static_cast<std::int8_t>(pps_scc.pps_act_cr_qp_offset_plus3);
					pps_.pps_num_palette_predictor_initializers = pps_scc.pps_num_palette_predictor_initializer;
					pps_.luma_bit_depth_entry_minus8 = pps_scc.luma_bit_depth_entry_minus8;
					pps_.chroma_bit_depth_entry_minus8 = static_cast<std::uint8_t>(pps_scc.chroma_bit_depth_entry_minus8);
				}
				pps_.num_tile_columns_minus1 = pps.num_tile_columns_minus1;
				pps_.num_tile_rows_minus1 = pps.num_tile_rows_minus1;
				for (std::size_t index = 0; index < std::size(pps_.column_width_minus1); ++index) {
					pps_.column_width_minus1[index] = static_cast<std::uint16_t>(pps.column_width_minus1[index]);
				}
				for (std::size_t index = 0; index < std::size(pps_.row_height_minus1); ++index) {
					pps_.row_height_minus1[index] = static_cast<std::uint16_t>(pps.row_height_minus1[index]);
				}
				if (pps.scaling_list_data_present_flag != 0U) {
					lower_scaling(pps.scaling_list, pps_scaling_);
					pps_.pScalingLists = &pps_scaling_;
				}
				if (has_pps_scc_extension && pps_scc.pps_palette_predictor_initializers_present_flag != 0U) {
					for (std::size_t component = 0; component < 3U; ++component) {
						for (std::size_t index = 0; index < 128U; ++index) {
							pps_palette_.PredictorPaletteEntries[component][index] = static_cast<std::uint16_t>(
									pps_scc.pps_palette_predictor_initializer[component][index]);
						}
					}
					pps_.pPredictorPaletteEntries = &pps_palette_;
				}
			}
		};
	}
}
