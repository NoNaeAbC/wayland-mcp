#pragma once

#include "video_editor/h265/api.hpp"

#include <array>
#include <cstdint>
#include <expected>
#include <memory>
#include <span>

namespace video_editor::h265::decoder {
	enum class Nal_Unit_Type : std::uint8_t {
		TRAIL_N    = 0,
		TRAIL_R    = 1,
		TSA_N      = 2,
		TSA_R      = 3,
		STSA_N     = 4,
		STSA_R     = 5,
		RADL_N     = 6,
		RADL_R     = 7,
		RASL_N     = 8,
		RASL_R     = 9,
		BLA_W_LP   = 16,
		BLA_W_RADL = 17,
		BLA_N_LP   = 18,
		IDR_W_RADL = 19,
		IDR_N_LP   = 20,
		CRA        = 21,
		VPS        = 32,
		SPS        = 33,
		PPS        = 34,
		AUD        = 35,
		EOS        = 36,
		EOB        = 37,
		FD         = 38,
		PREFIX_SEI = 39,
		SUFFIX_SEI = 40,
	};

	constexpr bool is_irap_type(Nal_Unit_Type type) {
		return type >= Nal_Unit_Type::BLA_W_LP && static_cast<unsigned>(type) <= 23;
	}
	constexpr bool is_idr_type(Nal_Unit_Type type) {
		return type == Nal_Unit_Type::IDR_W_RADL || type == Nal_Unit_Type::IDR_N_LP;
	}
	constexpr bool is_bla_type(Nal_Unit_Type type) {
		return type >= Nal_Unit_Type::BLA_W_LP && type <= Nal_Unit_Type::BLA_N_LP;
	}
	constexpr bool is_rasl_type(Nal_Unit_Type type) {
		return type == Nal_Unit_Type::RASL_N || type == Nal_Unit_Type::RASL_R;
	}

	inline constexpr std::uint8_t kSlice_B = 0, kSlice_P = 1, kSlice_I = 2;
	inline constexpr std::uint8_t kProfile_Main = 1, kProfile_Main_Still_Picture = 3, kProfile_Range_Extension = 4,
								  kProfile_High_Throughput = 5, kProfile_Screen_Content = 9;
	inline constexpr std::uint8_t kMax_Sublayers = 7;

	struct Nal_View {
		std::span<const std::uint8_t>  bytes;
		Nal_Unit_Type                  type{};
		std::uint8_t                   layer_id{}, temporal_id_plus1{};
		static constexpr std::uint64_t header_bytes = 2;

		[[nodiscard]] std::span<const std::uint8_t> payload() const { return bytes.subspan(header_bytes); }
	};

	struct Profile_Tier_Level {
		std::uint8_t tier_flag{}, profile_idc{}, profile_compatibility_flag[32]{};
		std::uint8_t progressive_source_flag{}, interlaced_source_flag{}, non_packed_constraint_flag{},
				frame_only_constraint_flag{}, level_idc{};
	};

	struct Sublayer_Ordering {
		std::uint8_t       max_sub_layers_minus1{}, temporal_id_nesting_flag{}, sub_layer_ordering_info_present_flag{};
		std::uint8_t       max_dec_pic_buffering_minus1[kMax_Sublayers]{}, max_num_reorder_pics[kMax_Sublayers]{};
		std::uint32_t      max_latency_increase_plus1[kMax_Sublayers]{};
		Profile_Tier_Level profile_tier_level;
	};

	struct Video_Parameter_Set : Sublayer_Ordering {
		std::uint8_t  id{}, timing_info_present_flag{}, poc_proportional_to_timing_flag{};
		std::uint32_t num_units_in_tick{}, time_scale{}, num_ticks_poc_diff_one_minus1{};
	};

	struct Short_Term_Reference_Set {
		std::uint8_t NumNegativePics{}, NumPositivePics{};
		std::uint8_t UsedByCurrPicS0[16]{}, UsedByCurrPicS1[16]{};
		std::int32_t DeltaPocS0[16]{}, DeltaPocS1[16]{};
		std::uint8_t NumDeltaPocsOfRefRpsIdx{};
	};

	struct Video_Usability_Info {
		std::uint8_t  aspect_ratio_info_present_flag{}, aspect_ratio_idc{};
		std::uint16_t sar_width{}, sar_height{};
		std::uint8_t  overscan_info_present_flag{}, overscan_appropriate_flag{}, video_signal_type_present_flag{},
				video_format{5}, video_full_range_flag{}, colour_description_present_flag{}, colour_primaries{2},
				transfer_characteristics{2}, matrix_coefficients{2};
		std::uint8_t chroma_loc_info_present_flag{}, chroma_sample_loc_type_top_field{},
				chroma_sample_loc_type_bottom_field{};
		std::uint8_t neutral_chroma_indication_flag{}, field_seq_flag{}, frame_field_info_present_flag{},
				default_display_window_flag{};
		std::uint16_t def_disp_win_left_offset{}, def_disp_win_right_offset{}, def_disp_win_top_offset{},
				def_disp_win_bottom_offset{};
		std::uint8_t  timing_info_present_flag{}, poc_proportional_to_timing_flag{};
		std::uint32_t num_units_in_tick{}, time_scale{}, num_ticks_poc_diff_one_minus1{};
		std::uint8_t  bitstream_restriction_flag{}, tiles_fixed_structure_flag{},
				motion_vectors_over_pic_boundaries_flag{1}, restricted_ref_pic_lists_flag{};
		std::uint16_t min_spatial_segmentation_idc{};
		std::uint8_t  max_bytes_per_pic_denom{2}, max_bits_per_min_cu_denom{1}, log2_max_mv_length_horizontal{15},
				log2_max_mv_length_vertical{15};
	};

	struct Sequence_Range_Extension {
		std::uint8_t transform_skip_rotation_enabled_flag{}, transform_skip_context_enabled_flag{},
				implicit_rdpcm_enabled_flag{}, explicit_rdpcm_enabled_flag{}, extended_precision_processing_flag{},
				intra_smoothing_disabled_flag{}, high_precision_offsets_enabled_flag{},
				persistent_rice_adaptation_enabled_flag{}, cabac_bypass_alignment_enabled_flag{};
	};

	struct Sequence_Screen_Content_Extension {
		std::uint8_t sps_curr_pic_ref_enabled_flag{}, palette_mode_enabled_flag{}, palette_max_size{},
				delta_palette_max_predictor_size{}, sps_palette_predictor_initializers_present_flag{},
				sps_num_palette_predictor_initializer_minus1{};
		std::uint16_t sps_palette_predictor_initializer[3][128]{};
		std::uint8_t  motion_vector_resolution_control_idc{}, intra_boundary_filtering_disabled_flag{};
	};

	struct Picture_Range_Extension {
		std::uint8_t log2_max_transform_skip_block_size_minus2{}, cross_component_prediction_enabled_flag{},
				chroma_qp_offset_list_enabled_flag{}, diff_cu_chroma_qp_offset_depth{},
				chroma_qp_offset_list_len_minus1{};
		std::int8_t  cb_qp_offset_list[6]{}, cr_qp_offset_list[6]{};
		std::uint8_t log2_sao_offset_scale_luma{}, log2_sao_offset_scale_chroma{};
	};

	struct Picture_Screen_Content_Extension {
		std::uint8_t pps_curr_pic_ref_enabled_flag{}, residual_adaptive_colour_transform_enabled_flag{},
				pps_slice_act_qp_offsets_present_flag{};
		std::int8_t  pps_act_y_qp_offset_plus5{}, pps_act_cb_qp_offset_plus5{}, pps_act_cr_qp_offset_plus3{};
		std::uint8_t pps_palette_predictor_initializers_present_flag{}, pps_num_palette_predictor_initializer{},
				monochrome_palette_flag{}, luma_bit_depth_entry_minus8{}, chroma_bit_depth_entry_minus8{};
		std::uint16_t pps_palette_predictor_initializer[3][128]{};
	};

	struct Scaling_Lists {
		std::int16_t scaling_list_dc_coef_minus8_16x16[6]{}, scaling_list_dc_coef_minus8_32x32[2]{};
		std::uint8_t scaling_lists_4x4[6][16]{}, scaling_lists_8x8[6][64]{}, scaling_lists_16x16[6][64]{},
				scaling_lists_32x32[2][64]{};
	};

	struct Sequence_Parameter_Set : Sublayer_Ordering {
		std::uint8_t               id{}, vps_id{};
		const Video_Parameter_Set *vps{};
		std::uint8_t               chroma_format_idc{}, separate_colour_plane_flag{}, chroma_array_type{};
		std::uint32_t              pic_width_in_luma_samples{}, pic_height_in_luma_samples{};
		std::uint8_t               conformance_window_flag{};
		std::uint32_t conf_win_left_offset{}, conf_win_right_offset{}, conf_win_top_offset{}, conf_win_bottom_offset{};
		std::uint8_t  bit_depth_luma_minus8{}, bit_depth_chroma_minus8{}, log2_max_pic_order_cnt_lsb_minus4{};
		std::uint8_t  log2_min_luma_coding_block_size_minus3{}, log2_diff_max_min_luma_coding_block_size{},
				log2_min_transform_block_size_minus2{}, log2_diff_max_min_transform_block_size{},
				max_transform_hierarchy_depth_inter{}, max_transform_hierarchy_depth_intra{};
		std::uint8_t  scaling_list_enabled_flag{}, scaling_list_data_present_flag{};
		Scaling_Lists scaling_list;
		std::uint8_t  amp_enabled_flag{}, sample_adaptive_offset_enabled_flag{}, pcm_enabled_flag{},
				pcm_sample_bit_depth_luma_minus1{}, pcm_sample_bit_depth_chroma_minus1{},
				log2_min_pcm_luma_coding_block_size_minus3{}, log2_diff_max_min_pcm_luma_coding_block_size{},
				pcm_loop_filter_disabled_flag{};
		std::uint8_t             num_short_term_ref_pic_sets{};
		Short_Term_Reference_Set short_term_ref_pic_set[64]{};
		std::uint8_t             long_term_ref_pics_present_flag{}, num_long_term_ref_pics_sps{};
		std::uint16_t            lt_ref_pic_poc_lsb_sps[32]{};
		std::uint8_t             used_by_curr_pic_lt_sps_flag[32]{};
		std::uint8_t temporal_mvp_enabled_flag{}, strong_intra_smoothing_enabled_flag{}, vui_parameters_present_flag{};
		Video_Usability_Info              vui_params;
		std::uint8_t                      sps_range_extension_flag{}, sps_scc_extension_flag{};
		Sequence_Range_Extension          sps_extension_params;
		Sequence_Screen_Content_Extension sps_scc_extension_params;
		std::uint32_t                     crop_rect_width{}, crop_rect_height{}, crop_rect_x{}, crop_rect_y{};
	};

	struct Picture_Parameter_Set {
		std::uint8_t                  id{}, sps_id{};
		const Sequence_Parameter_Set *sps{};
		std::uint8_t dependent_slice_segments_enabled_flag{}, output_flag_present_flag{}, num_extra_slice_header_bits{},
				sign_data_hiding_enabled_flag{}, cabac_init_present_flag{}, num_ref_idx_l0_default_active_minus1{},
				num_ref_idx_l1_default_active_minus1{};
		std::int8_t  init_qp_minus26{};
		std::uint8_t constrained_intra_pred_flag{}, transform_skip_enabled_flag{}, cu_qp_delta_enabled_flag{},
				diff_cu_qp_delta_depth{};
		std::int8_t  cb_qp_offset{}, cr_qp_offset{};
		std::uint8_t slice_chroma_qp_offsets_present_flag{}, weighted_pred_flag{}, weighted_bipred_flag{},
				transquant_bypass_enabled_flag{}, tiles_enabled_flag{}, entropy_coding_sync_enabled_flag{},
				num_tile_columns_minus1{}, num_tile_rows_minus1{}, uniform_spacing_flag{};
		std::uint32_t column_width_minus1[20]{}, row_height_minus1[22]{};
		std::uint8_t  loop_filter_across_tiles_enabled_flag{}, loop_filter_across_slices_enabled_flag{},
				deblocking_filter_control_present_flag{}, deblocking_filter_override_enabled_flag{},
				deblocking_filter_disabled_flag{};
		std::int8_t  beta_offset_div2{}, tc_offset_div2{};
		std::uint8_t scaling_list_data_present_flag{}, lists_modification_present_flag{},
				log2_parallel_merge_level_minus2{}, slice_segment_header_extension_present_flag{},
				pps_range_extension_flag{}, pps_scc_extension_flag{};
		Scaling_Lists                    scaling_list;
		Picture_Range_Extension          pps_extension_params;
		Picture_Screen_Content_Extension pps_scc_extension_params;
		std::uint32_t                    PicWidthInCtbsY{}, PicHeightInCtbsY{};
	};

	struct Reference_List_Modification {
		std::uint8_t ref_pic_list_modification_flag_l0{}, list_entry_l0[15]{}, ref_pic_list_modification_flag_l1{},
				list_entry_l1[15]{};
	};

	struct Prediction_Weights {
		std::uint8_t luma_log2_weight_denom{};
		std::int8_t  delta_chroma_log2_weight_denom{};
		std::uint8_t luma_weight_l0_flag[15]{}, chroma_weight_l0_flag[15]{}, luma_weight_l1_flag[15]{},
				chroma_weight_l1_flag[15]{};
		std::int8_t delta_luma_weight_l0[15]{}, delta_luma_weight_l1[15]{}, delta_chroma_weight_l0[15][2]{},
				delta_chroma_weight_l1[15][2]{};
		std::int16_t luma_offset_l0[15]{}, luma_offset_l1[15]{}, delta_chroma_offset_l0[15][2]{},
				delta_chroma_offset_l1[15][2]{};
	};

	struct Slice_Header {
		const Picture_Parameter_Set *pps{};
		std::uint8_t                 pps_id{}, first_slice_segment_in_pic_flag{}, no_output_of_prior_pics_flag{},
				dependent_slice_segment_flag{};
		std::uint32_t            segment_address{};
		std::uint8_t             type{}, pic_output_flag{1}, colour_plane_id{};
		std::uint16_t            pic_order_cnt_lsb{};
		std::uint8_t             short_term_ref_pic_set_sps_flag{}, short_term_ref_pic_set_idx{};
		Short_Term_Reference_Set short_term_ref_pic_sets;
		std::uint8_t num_long_term_sps{}, num_long_term_pics{}, lt_idx_sps[16]{}, used_by_curr_pic_lt_flag[16]{},
				delta_poc_msb_present_flag[16]{};
		std::uint32_t poc_lsb_lt[16]{}, delta_poc_msb_cycle_lt[16]{};
		std::uint8_t  temporal_mvp_enabled_flag{}, sao_luma_flag{}, sao_chroma_flag{}, num_ref_idx_l0_active_minus1{},
				num_ref_idx_l1_active_minus1{};
		Reference_List_Modification ref_pic_list_modification;
		std::uint8_t       mvd_l1_zero_flag{}, cabac_init_flag{}, collocated_from_l0_flag{1}, collocated_ref_idx{};
		Prediction_Weights pred_weight_table;
		std::uint8_t       five_minus_max_num_merge_cand{}, use_integer_mv_flag{};
		std::int8_t qp_delta{}, cb_qp_offset{}, cr_qp_offset{}, slice_act_y_qp_offset{}, slice_act_cb_qp_offset{},
				slice_act_cr_qp_offset{};
		std::uint8_t cu_chroma_qp_offset_enabled_flag{}, deblocking_filter_override_flag{},
				deblocking_filter_disabled_flag{};
		std::int8_t                                       beta_offset_div2{}, tc_offset_div2{};
		std::uint8_t                                      loop_filter_across_slices_enabled_flag{};
		std::shared_ptr<const std::vector<std::uint32_t>> entry_point_offsets;
		std::uint32_t                                     num_entry_point_offsets{};
		std::int32_t                                      NumPocTotalCurr{};
		std::uint64_t header_bytes{}, n_emulation_prevention_bytes{}, short_term_ref_pic_set_size{};
	};

	// A syntax error carries the physical bit position within the NAL payload.
	struct Syntax_Error {
		std::uint64_t bit{};
		const char   *detail{};
		bool          unsupported{};
		bool          missing_parameter_set{};
		const char   *field{};
	};
	template<class T>
	using Syntax_Result = std::expected<T, Syntax_Error>;

	[[nodiscard]] Syntax_Result<Nal_View>               parse_nal_header(std::span<const std::uint8_t> bytes);
	[[nodiscard]] Syntax_Result<Video_Parameter_Set>    parse_vps(const Nal_View &nal);
	[[nodiscard]] Syntax_Result<Sequence_Parameter_Set> parse_sps(const Nal_View &nal);
	[[nodiscard]] Syntax_Result<Picture_Parameter_Set>
	parse_pps(const Nal_View &nal, std::span<const std::optional<Sequence_Parameter_Set>, 16> sequences);
	[[nodiscard]] Syntax_Result<Slice_Header>
	parse_slice_header(const Nal_View &nal, std::span<const std::optional<Picture_Parameter_Set>, 64> pictures);
} // namespace video_editor::h265::decoder
