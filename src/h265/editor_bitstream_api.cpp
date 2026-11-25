#include "h265/frame_vaapi.hpp"
#include "h265/decoder_internal.hpp"
#include "h265/slice_payload.hpp"
#include "h265/vulkan_video.hpp"

#include <algorithm>
#include <array>
#include <cstring>
#include <fcntl.h>
#include <filesystem>
#include <iterator>
#include <limits>
#include <memory>
#include <ranges>
#include <sstream>
#include <unistd.h>
#include <utility>
#include <va/va_dec_hevc.h>
#include <va/va_drm.h>

namespace video_editor::h265 {
	namespace {
		[[nodiscard]] Media_Error error(Media_Error_Code code, std::string message) {
			return Media_Error{.code_ = code, .byte_offset_ = 0, .bit_offset_ = 0, .message_ = std::move(message)};
		}

		[[nodiscard]] bool status_ok(VAStatus status) { return status == VA_STATUS_SUCCESS; }

		[[nodiscard]] std::optional<VAProfile> va_profile_for(H265_Profile profile) {
			switch (profile) {
				case H265_Profile::MAIN:
				case H265_Profile::MAIN_STILL_PICTURE:
					return VAProfileHEVCMain;
				case H265_Profile::MAIN_10:
					return VAProfileHEVCMain10;
				case H265_Profile::MAIN_12:
					return VAProfileHEVCMain12;
				case H265_Profile::MAIN_422_10:
					return VAProfileHEVCMain422_10;
				case H265_Profile::MAIN_422_12:
					return VAProfileHEVCMain422_12;
				case H265_Profile::MAIN_444:
					return VAProfileHEVCMain444;
				case H265_Profile::MAIN_444_10:
					return VAProfileHEVCMain444_10;
				case H265_Profile::MAIN_444_12:
					return VAProfileHEVCMain444_12;
				case H265_Profile::SCC_MAIN:
					return VAProfileHEVCSccMain;
				case H265_Profile::SCC_MAIN_10:
					return VAProfileHEVCSccMain10;
				case H265_Profile::SCC_MAIN_444:
					return VAProfileHEVCSccMain444;
				case H265_Profile::SCC_MAIN_444_10:
					return VAProfileHEVCSccMain444_10;
				case H265_Profile::UNSUPPORTED_EXTENSION:
					return std::nullopt;
			}
			return std::nullopt;
		}

		[[nodiscard]] std::optional<VAProfile> va_profile_for(const H265_Sequence_Info &sequence) {
			if (sequence.chroma_format_ == H265_Chroma_Format::MONOCHROME) {
				if (sequence.bit_depth_ > 10U) { return VAProfileHEVCMain444_12; }
				return sequence.bit_depth_ > 8U ? VAProfileHEVCMain444_10 : VAProfileHEVCMain444;
			}
			return va_profile_for(sequence.profile_);
		}

		[[nodiscard]] bool uses_extension_picture_buffers(const H265_Sequence_Info &sequence) {
			return sequence.range_extension_ || sequence.screen_content_ ||
				   sequence.profile_ == H265_Profile::MAIN_12 || sequence.profile_ == H265_Profile::MAIN_422_10 ||
				   sequence.profile_ == H265_Profile::MAIN_422_12 || sequence.profile_ == H265_Profile::MAIN_444 ||
				   sequence.profile_ == H265_Profile::MAIN_444_10 || sequence.profile_ == H265_Profile::MAIN_444_12;
		}

		using decoder::Parsed_Decode_Frame;
		using decoder::Parsed_Decode_Slice;

		struct Va_Decode_Reference {
			std::uint8_t  slot_          = 0;
			VASurfaceID   surface_       = VA_INVALID_SURFACE;
			std::int32_t  pic_order_cnt_ = 0;
			std::uint16_t pic_order_lsb_ = 0;
			bool          long_term_     = false;
			bool          unavailable_   = false;
			bool          field_picture_ = false;
			bool          bottom_field_  = false;
			std::uint32_t va_flags_      = 0;
		};

		using decoder::Slice_Header;
		using decoder::Scaling_Lists;
		using decoder::Short_Term_Reference_Set;
		using decoder::Sequence_Parameter_Set;
		using decoder::kMax_Sublayers;
		using decoder::kSlice_I;
		using decoder::kSlice_P;
		using decoder::kSlice_B;

		[[nodiscard]] Result<void> validate_decode_frame_shape(const Parsed_Decode_Frame &frame) {
			if (frame.slices_.empty()) {
				return std::unexpected(error(Media_Error_Code::UNSUPPORTED_DRIVER_FEATURE,
											 "H.265 VA decode request has no VCL slices"));
			}
			const auto *pps = frame.slices_.front().header_.pps;
			const auto *sps = pps == nullptr ? nullptr : pps->sps;
			if (pps == nullptr || sps == nullptr) {
				return std::unexpected(
						error(Media_Error_Code::INVALID_H265_BITSTREAM, "H.265 slice did not resolve active PPS/SPS"));
			}
			if (sps->pic_width_in_luma_samples > UINT16_MAX || sps->pic_height_in_luma_samples > UINT16_MAX) {
				return std::unexpected(
						error(Media_Error_Code::UNSUPPORTED_DRIVER_FEATURE,
							  "H.265 picture dimensions exceed the VA-API picture-parameter field width"));
			}
			for (const auto &candidate: frame.slices_) {
				if (candidate.header_.pps != pps) {
					return std::unexpected(error(Media_Error_Code::INVALID_H265_BITSTREAM,
												 "H.265 frame slices disagree on active PPS"));
				}
			}
			std::uint64_t subset_offset_index = 0;
			for (const auto &candidate: frame.slices_) {
				const auto &header = candidate.header_;
				if (header.num_entry_point_offsets > std::numeric_limits<std::uint16_t>::max() ||
					subset_offset_index > std::numeric_limits<std::uint16_t>::max() ||
					header.n_emulation_prevention_bytes > std::numeric_limits<std::uint16_t>::max()) {
					return std::unexpected(
							error(Media_Error_Code::INVALID_H265_BITSTREAM,
								  "H.265 slice metadata exceeds the VA-API HEVC slice-parameter field width"));
				}
				subset_offset_index += header.num_entry_point_offsets;
			}
			return {};
		}

		[[nodiscard]] std::uint32_t slice_data_byte_offset(const Parsed_Decode_Slice &slice) {
			return static_cast<std::uint32_t>(decoder::Nal_View::header_bytes + slice.header_.header_bytes -
					slice.header_.n_emulation_prevention_bytes);
		}

		[[nodiscard]] std::uint8_t h265_sps_max_sublayer_index(const Sequence_Parameter_Set &sps) {
			return static_cast<std::uint8_t>(
					std::min<std::uint8_t>(sps.max_sub_layers_minus1, kMax_Sublayers - 1U));
		}

		[[nodiscard]] VAPictureParameterBufferHEVCExtension
		make_picture_parameters(const Parsed_Decode_Frame &frame, const H265_Sequence_Info &sequence,
								VASurfaceID current_surface, const decoder::Frame_Reference_State &state,
								std::span<const Va_Decode_Reference> references) {
			const auto &first    = frame.slices_.front();
			const auto &slice    = first.header_;
			const auto &pps      = *slice.pps;
			const auto &sps      = *pps.sps;
			const auto  sublayer = h265_sps_max_sublayer_index(sps);

			VAPictureParameterBufferHEVCExtension picture{};
			auto                                 &base = picture.base;
			base.CurrPic.picture_id                    = current_surface;
			base.CurrPic.flags                         = 0;
			if (state.field_picture_) { base.CurrPic.flags |= VA_PICTURE_HEVC_FIELD_PIC; }
			if (state.bottom_field_) { base.CurrPic.flags |= VA_PICTURE_HEVC_BOTTOM_FIELD; }
			base.CurrPic.pic_order_cnt = state.pic_order_cnt_;
			for (auto &reference: base.ReferenceFrames) {
				reference.picture_id    = VA_INVALID_ID;
				reference.flags         = VA_PICTURE_HEVC_INVALID;
				reference.pic_order_cnt = 0;
			}
			for (const auto &reference: references) {
				if (reference.slot_ >= 15U) { continue; }
				auto &picture_reference      = base.ReferenceFrames[reference.slot_];
				picture_reference.picture_id = reference.surface_;
				picture_reference.flags      = reference.va_flags_;
				if (reference.field_picture_) { picture_reference.flags |= VA_PICTURE_HEVC_FIELD_PIC; }
				if (reference.bottom_field_) { picture_reference.flags |= VA_PICTURE_HEVC_BOTTOM_FIELD; }
				if (reference.long_term_) { picture_reference.flags |= VA_PICTURE_HEVC_LONG_TERM_REFERENCE; }
				picture_reference.pic_order_cnt = reference.pic_order_cnt_;
			}

			base.pic_width_in_luma_samples        = sps.pic_width_in_luma_samples;
			base.pic_height_in_luma_samples       = sps.pic_height_in_luma_samples;
			base.sps_max_dec_pic_buffering_minus1 = sps.max_dec_pic_buffering_minus1[sublayer];
			base.bit_depth_luma_minus8            = sps.bit_depth_luma_minus8;
			base.bit_depth_chroma_minus8          = sps.bit_depth_chroma_minus8;
			base.pcm_sample_bit_depth_luma_minus1 =
					sps.pcm_enabled_flag != 0U ? sps.pcm_sample_bit_depth_luma_minus1 : 0U;
			base.pcm_sample_bit_depth_chroma_minus1 =
					sps.pcm_enabled_flag != 0U ? sps.pcm_sample_bit_depth_chroma_minus1 : 0U;
			base.log2_min_luma_coding_block_size_minus3   = sps.log2_min_luma_coding_block_size_minus3;
			base.log2_diff_max_min_luma_coding_block_size = sps.log2_diff_max_min_luma_coding_block_size;
			base.log2_min_transform_block_size_minus2     = sps.log2_min_transform_block_size_minus2;
			base.log2_diff_max_min_transform_block_size   = sps.log2_diff_max_min_transform_block_size;
			base.log2_min_pcm_luma_coding_block_size_minus3 =
					sps.pcm_enabled_flag != 0U ? sps.log2_min_pcm_luma_coding_block_size_minus3 : 0U;
			base.log2_diff_max_min_pcm_luma_coding_block_size =
					sps.pcm_enabled_flag != 0U ? sps.log2_diff_max_min_pcm_luma_coding_block_size : 0U;
			base.max_transform_hierarchy_depth_intra = sps.max_transform_hierarchy_depth_intra;
			base.max_transform_hierarchy_depth_inter = sps.max_transform_hierarchy_depth_inter;
			base.init_qp_minus26                     = pps.init_qp_minus26;
			base.diff_cu_qp_delta_depth              = pps.diff_cu_qp_delta_depth;
			base.pps_cb_qp_offset                    = pps.cb_qp_offset;
			base.pps_cr_qp_offset                    = pps.cr_qp_offset;
			base.log2_parallel_merge_level_minus2    = pps.log2_parallel_merge_level_minus2;
			base.num_tile_columns_minus1             = pps.tiles_enabled_flag ? pps.num_tile_columns_minus1 : 0;
			base.num_tile_rows_minus1                = pps.tiles_enabled_flag ? pps.num_tile_rows_minus1 : 0;
			for (std::size_t index = 0; index < std::min<std::size_t>(20U, std::size(pps.column_width_minus1));
				 ++index) {
				if (index < std::size(base.column_width_minus1)) {
					base.column_width_minus1[index] = static_cast<std::uint16_t>(pps.column_width_minus1[index]);
				}
			}
			for (std::size_t index = 0; index < std::min<std::size_t>(22U, std::size(pps.row_height_minus1)); ++index) {
				if (index < std::size(base.row_height_minus1)) {
					base.row_height_minus1[index] = static_cast<std::uint16_t>(pps.row_height_minus1[index]);
				}
			}
			base.log2_max_pic_order_cnt_lsb_minus4    = sps.log2_max_pic_order_cnt_lsb_minus4;
			base.num_short_term_ref_pic_sets          = sps.num_short_term_ref_pic_sets;
			base.num_long_term_ref_pic_sps            = sps.num_long_term_ref_pics_sps;
			base.num_ref_idx_l0_default_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
			base.num_ref_idx_l1_default_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
			base.pps_beta_offset_div2                 = pps.beta_offset_div2;
			base.pps_tc_offset_div2                   = pps.tc_offset_div2;
			base.num_extra_slice_header_bits          = pps.num_extra_slice_header_bits;
			base.st_rps_bits = slice.short_term_ref_pic_set_sps_flag == 0U ? slice.short_term_ref_pic_set_size : 0U;

			const bool all_intra = std::ranges::all_of(frame.slices_, [](const Parsed_Decode_Slice &candidate) {
				return candidate.header_.type == kSlice_I;
			});

			base.pic_fields.bits.chroma_format_idc                   = sps.chroma_format_idc;
			base.pic_fields.bits.separate_colour_plane_flag          = sps.separate_colour_plane_flag;
			base.pic_fields.bits.pcm_enabled_flag                    = sps.pcm_enabled_flag;
			base.pic_fields.bits.scaling_list_enabled_flag           = sps.scaling_list_enabled_flag;
			base.pic_fields.bits.transform_skip_enabled_flag         = pps.transform_skip_enabled_flag;
			base.pic_fields.bits.amp_enabled_flag                    = sps.amp_enabled_flag;
			base.pic_fields.bits.strong_intra_smoothing_enabled_flag = sps.strong_intra_smoothing_enabled_flag;
			base.pic_fields.bits.sign_data_hiding_enabled_flag       = pps.sign_data_hiding_enabled_flag;
			base.pic_fields.bits.constrained_intra_pred_flag         = pps.constrained_intra_pred_flag;
			base.pic_fields.bits.cu_qp_delta_enabled_flag            = pps.cu_qp_delta_enabled_flag;
			base.pic_fields.bits.weighted_pred_flag                  = pps.weighted_pred_flag;
			base.pic_fields.bits.weighted_bipred_flag                = pps.weighted_bipred_flag;
			base.pic_fields.bits.transquant_bypass_enabled_flag      = pps.transquant_bypass_enabled_flag;
			base.pic_fields.bits.tiles_enabled_flag                  = pps.tiles_enabled_flag;
			base.pic_fields.bits.entropy_coding_sync_enabled_flag    = pps.entropy_coding_sync_enabled_flag;
			base.pic_fields.bits.pps_loop_filter_across_slices_enabled_flag =
					pps.loop_filter_across_slices_enabled_flag;
			base.pic_fields.bits.loop_filter_across_tiles_enabled_flag = pps.loop_filter_across_tiles_enabled_flag;
			base.pic_fields.bits.pcm_loop_filter_disabled_flag         = sps.pcm_loop_filter_disabled_flag;
			base.pic_fields.bits.NoPicReorderingFlag                   = 0;
			base.pic_fields.bits.NoBiPredFlag                          = 0;

			base.slice_parsing_fields.bits.lists_modification_present_flag = pps.lists_modification_present_flag;
			base.slice_parsing_fields.bits.long_term_ref_pics_present_flag = sps.long_term_ref_pics_present_flag;
			base.slice_parsing_fields.bits.sps_temporal_mvp_enabled_flag   = sps.temporal_mvp_enabled_flag;
			base.slice_parsing_fields.bits.cabac_init_present_flag         = pps.cabac_init_present_flag;
			base.slice_parsing_fields.bits.output_flag_present_flag        = pps.output_flag_present_flag;
			base.slice_parsing_fields.bits.dependent_slice_segments_enabled_flag =
					pps.dependent_slice_segments_enabled_flag;
			base.slice_parsing_fields.bits.pps_slice_chroma_qp_offsets_present_flag =
					pps.slice_chroma_qp_offsets_present_flag;
			base.slice_parsing_fields.bits.sample_adaptive_offset_enabled_flag =
					sps.sample_adaptive_offset_enabled_flag;
			base.slice_parsing_fields.bits.deblocking_filter_override_enabled_flag =
					pps.deblocking_filter_override_enabled_flag;
			base.slice_parsing_fields.bits.pps_disable_deblocking_filter_flag = pps.deblocking_filter_disabled_flag;
			base.slice_parsing_fields.bits.slice_segment_header_extension_present_flag =
					pps.slice_segment_header_extension_present_flag;
			base.slice_parsing_fields.bits.RapPicFlag   = is_irap_type(first.nalu_.type);
			base.slice_parsing_fields.bits.IdrPicFlag   = is_idr_type(first.nalu_.type);
			base.slice_parsing_fields.bits.IntraPicFlag = all_intra;

			picture.rext.range_extension_pic_fields.bits.transform_skip_rotation_enabled_flag =
					sps.sps_extension_params.transform_skip_rotation_enabled_flag;
			picture.rext.range_extension_pic_fields.bits.transform_skip_context_enabled_flag =
					sps.sps_extension_params.transform_skip_context_enabled_flag;
			picture.rext.range_extension_pic_fields.bits.implicit_rdpcm_enabled_flag =
					sps.sps_extension_params.implicit_rdpcm_enabled_flag;
			picture.rext.range_extension_pic_fields.bits.explicit_rdpcm_enabled_flag =
					sps.sps_extension_params.explicit_rdpcm_enabled_flag;
			picture.rext.range_extension_pic_fields.bits.extended_precision_processing_flag =
					sps.sps_extension_params.extended_precision_processing_flag;
			picture.rext.range_extension_pic_fields.bits.intra_smoothing_disabled_flag =
					sps.sps_extension_params.intra_smoothing_disabled_flag;
			picture.rext.range_extension_pic_fields.bits.high_precision_offsets_enabled_flag =
					sps.sps_extension_params.high_precision_offsets_enabled_flag;
			picture.rext.range_extension_pic_fields.bits.persistent_rice_adaptation_enabled_flag =
					sps.sps_extension_params.persistent_rice_adaptation_enabled_flag;
			picture.rext.range_extension_pic_fields.bits.cabac_bypass_alignment_enabled_flag =
					sps.sps_extension_params.cabac_bypass_alignment_enabled_flag;
			picture.rext.range_extension_pic_fields.bits.cross_component_prediction_enabled_flag =
					pps.pps_extension_params.cross_component_prediction_enabled_flag;
			picture.rext.range_extension_pic_fields.bits.chroma_qp_offset_list_enabled_flag =
					pps.pps_extension_params.chroma_qp_offset_list_enabled_flag;
			picture.rext.diff_cu_chroma_qp_offset_depth   = pps.pps_extension_params.diff_cu_chroma_qp_offset_depth;
			picture.rext.chroma_qp_offset_list_len_minus1 = pps.pps_extension_params.chroma_qp_offset_list_len_minus1;
			picture.rext.log2_sao_offset_scale_luma       = pps.pps_extension_params.log2_sao_offset_scale_luma;
			picture.rext.log2_sao_offset_scale_chroma     = pps.pps_extension_params.log2_sao_offset_scale_chroma;
			picture.rext.log2_max_transform_skip_block_size_minus2 =
					static_cast<std::uint8_t>(pps.pps_extension_params.log2_max_transform_skip_block_size_minus2);
			for (std::size_t index = 0; index < 6U; ++index) {
				picture.rext.cb_qp_offset_list[index] = pps.pps_extension_params.cb_qp_offset_list[index];
				picture.rext.cr_qp_offset_list[index] = pps.pps_extension_params.cr_qp_offset_list[index];
			}

			const auto predictor_palette_size =
					pps.pps_scc_extension_params.pps_palette_predictor_initializers_present_flag
							? pps.pps_scc_extension_params.pps_num_palette_predictor_initializer
							: (sps.sps_scc_extension_params.sps_palette_predictor_initializers_present_flag
									   ? sps.sps_scc_extension_params.sps_num_palette_predictor_initializer_minus1 + 1U
									   : 0U);
			picture.scc.screen_content_pic_fields.bits.pps_curr_pic_ref_enabled_flag =
					pps.pps_scc_extension_params.pps_curr_pic_ref_enabled_flag;
			picture.scc.screen_content_pic_fields.bits.palette_mode_enabled_flag =
					sps.sps_scc_extension_params.palette_mode_enabled_flag;
			picture.scc.screen_content_pic_fields.bits.motion_vector_resolution_control_idc =
					sps.sps_scc_extension_params.motion_vector_resolution_control_idc;
			picture.scc.screen_content_pic_fields.bits.intra_boundary_filtering_disabled_flag =
					sps.sps_scc_extension_params.intra_boundary_filtering_disabled_flag;
			picture.scc.screen_content_pic_fields.bits.residual_adaptive_colour_transform_enabled_flag =
					pps.pps_scc_extension_params.residual_adaptive_colour_transform_enabled_flag;
			picture.scc.screen_content_pic_fields.bits.pps_slice_act_qp_offsets_present_flag =
					pps.pps_scc_extension_params.pps_slice_act_qp_offsets_present_flag;
			picture.scc.palette_max_size = sps.sps_scc_extension_params.palette_max_size;
			picture.scc.delta_palette_max_predictor_size =
					sps.sps_scc_extension_params.delta_palette_max_predictor_size;
			picture.scc.predictor_palette_size     = predictor_palette_size;
			picture.scc.pps_act_y_qp_offset_plus5  = pps.pps_scc_extension_params.pps_act_y_qp_offset_plus5;
			picture.scc.pps_act_cb_qp_offset_plus5 = pps.pps_scc_extension_params.pps_act_cb_qp_offset_plus5;
			picture.scc.pps_act_cr_qp_offset_plus3 = pps.pps_scc_extension_params.pps_act_cr_qp_offset_plus3;
			const auto components = pps.pps_scc_extension_params.monochrome_palette_flag != 0U ? 1U : 3U;
			for (std::size_t component = 0; component < components; ++component) {
				for (std::size_t entry = 0; entry < std::min<std::size_t>(predictor_palette_size, 128U); ++entry) {
					picture.scc.predictor_palette_entries[component][entry] =
							pps.pps_scc_extension_params.pps_palette_predictor_initializers_present_flag
									? pps.pps_scc_extension_params.pps_palette_predictor_initializer[component][entry]
									: sps.sps_scc_extension_params.sps_palette_predictor_initializer[component][entry];
				}
			}
			(void) sequence;
			return picture;
		}

		static constexpr std::array<std::uint8_t, 64> kDefault_Scaling_List_Intra{
				16, 16, 16, 16, 17, 18, 21, 24, 16, 16, 16, 16, 17, 19, 22, 25, 16, 16, 17, 18,  20, 22,
				25, 29, 16, 16, 18, 21, 24, 27, 31, 36, 17, 17, 20, 24, 30, 35, 41, 47, 18, 19,  22, 27,
				35, 44, 54, 65, 21, 22, 25, 31, 41, 54, 70, 88, 24, 25, 29, 36, 47, 65, 88, 115,
		};

		static constexpr std::array<std::uint8_t, 64> kDefault_Scaling_List_Inter{
				16, 16, 16, 16, 17, 18, 20, 24, 16, 16, 16, 17, 18, 20, 24, 25, 16, 16, 17, 18, 20, 24,
				25, 28, 16, 17, 18, 20, 24, 25, 28, 33, 17, 18, 20, 24, 25, 28, 33, 41, 18, 20, 24, 25,
				28, 33, 41, 54, 20, 24, 25, 28, 33, 41, 54, 71, 24, 25, 28, 33, 41, 54, 71, 91,
		};

		void copy_scaling_list(std::span<std::uint8_t> destination, const std::uint8_t *source, std::uint8_t edge) {
			std::size_t scan_index = 0;
			for (std::uint8_t diagonal = 0; diagonal <= static_cast<std::uint8_t>(2U * (edge - 1U)); ++diagonal) {
				std::uint8_t y     = std::min<std::uint8_t>(diagonal, edge - 1U);
				const auto   min_y = static_cast<std::uint8_t>(diagonal < edge ? 0U : diagonal - (edge - 1U));
				for (;;) {
					const auto x                                        = static_cast<std::uint8_t>(diagonal - y);
					destination[static_cast<std::size_t>(y) * edge + x] = source[scan_index++];
					if (y == min_y) { break; }
					--y;
				}
			}
		}

		void copy_default_scaling_list(std::span<std::uint8_t>             destination,
									   const std::array<std::uint8_t, 64> &source) {
			std::memcpy(destination.data(), source.data(), destination.size());
		}

		[[nodiscard]] VAIQMatrixBufferHEVC make_default_iq_matrix() {
			VAIQMatrixBufferHEVC matrix{};
			for (std::size_t i = 0; i < 6U; ++i) {
				std::memset(matrix.ScalingList4x4[i], 16, sizeof(matrix.ScalingList4x4[i]));
				const auto &default_list = i < 3U ? kDefault_Scaling_List_Intra : kDefault_Scaling_List_Inter;
				copy_default_scaling_list(std::span<std::uint8_t>{matrix.ScalingList8x8[i]}, default_list);
				copy_default_scaling_list(std::span<std::uint8_t>{matrix.ScalingList16x16[i]}, default_list);
				if (i < 2U) {
					copy_default_scaling_list(std::span<std::uint8_t>{matrix.ScalingList32x32[i]},
											  i == 0U ? kDefault_Scaling_List_Intra : kDefault_Scaling_List_Inter);
				}
				matrix.ScalingListDC16x16[i] = 16U;
				if (i < 2U) { matrix.ScalingListDC32x32[i] = 16U; }
			}
			return matrix;
		}

		[[nodiscard]] VAIQMatrixBufferHEVC make_iq_matrix(const Scaling_Lists &list) {
			VAIQMatrixBufferHEVC matrix{};
			for (std::size_t i = 0; i < 6U; ++i) {
				copy_scaling_list(std::span<std::uint8_t>{matrix.ScalingList4x4[i]}, list.scaling_lists_4x4[i], 4U);
				copy_scaling_list(std::span<std::uint8_t>{matrix.ScalingList8x8[i]}, list.scaling_lists_8x8[i], 8U);
				copy_scaling_list(std::span<std::uint8_t>{matrix.ScalingList16x16[i]}, list.scaling_lists_16x16[i], 8U);
				if (i < 2U) {
					copy_scaling_list(std::span<std::uint8_t>{matrix.ScalingList32x32[i]}, list.scaling_lists_32x32[i],
									  8U);
				}
				matrix.ScalingListDC16x16[i] = static_cast<std::uint8_t>(list.scaling_list_dc_coef_minus8_16x16[i] + 8);
				if (i < 2U) {
					matrix.ScalingListDC32x32[i] =
							static_cast<std::uint8_t>(list.scaling_list_dc_coef_minus8_32x32[i] + 8);
				}
			}
			return matrix;
		}

		struct Reference_Target {
			std::int32_t  pic_order_cnt_     = 0;
			std::uint16_t pic_order_cnt_lsb_ = 0;
			bool          exact_poc_         = true;
			bool          long_term_         = false;
		};

		[[nodiscard]] std::optional<std::uint8_t>
		find_va_reference_slot(std::span<const Va_Decode_Reference> references, const Reference_Target &target) {
			for (const auto &reference: references) {
				if (reference.long_term_ != target.long_term_) { continue; }
				if (target.exact_poc_) {
					if (reference.pic_order_cnt_ != target.pic_order_cnt_) { continue; }
				} else if (reference.pic_order_lsb_ != target.pic_order_cnt_lsb_) {
					continue;
				}
				return reference.slot_;
			}
			return std::nullopt;
		}

		struct Reference_Sets {
			std::vector<Reference_Target> before_;
			std::vector<Reference_Target> after_;
			std::vector<Reference_Target> lt_;
		};

		[[nodiscard]] Reference_Sets derive_reference_sets(const Slice_Header &slice, std::int32_t current_poc) {
			Reference_Sets                   sets;
			const auto                      *pps = slice.pps;
			const auto                      *sps = pps == nullptr ? nullptr : pps->sps;
			const Short_Term_Reference_Set *rps = nullptr;
			if (sps != nullptr) {
				rps = slice.short_term_ref_pic_set_sps_flag != 0U
							  ? (slice.short_term_ref_pic_set_idx < sps->num_short_term_ref_pic_sets
										 ? &sps->short_term_ref_pic_set[slice.short_term_ref_pic_set_idx]
										 : nullptr)
							  : &slice.short_term_ref_pic_sets;
			}
			if (rps != nullptr) {
				for (std::uint8_t index = 0; index < rps->NumNegativePics; ++index) {
					if (rps->UsedByCurrPicS0[index] == 0U) { continue; }
					sets.before_.push_back(Reference_Target{
							.pic_order_cnt_     = current_poc + rps->DeltaPocS0[index],
							.pic_order_cnt_lsb_ = static_cast<std::uint16_t>(current_poc + rps->DeltaPocS0[index]),
							.exact_poc_         = true,
							.long_term_         = false,
					});
				}
				for (std::uint8_t index = 0; index < rps->NumPositivePics; ++index) {
					if (rps->UsedByCurrPicS1[index] == 0U) { continue; }
					sets.after_.push_back(Reference_Target{
							.pic_order_cnt_     = current_poc + rps->DeltaPocS1[index],
							.pic_order_cnt_lsb_ = static_cast<std::uint16_t>(current_poc + rps->DeltaPocS1[index]),
							.exact_poc_         = true,
							.long_term_         = false,
					});
				}
			}
			if (sps == nullptr || sps->long_term_ref_pics_present_flag == 0U) { return sets; }
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
				if (!used) { continue; }
				Reference_Target target{
						.pic_order_cnt_     = 0,
						.pic_order_cnt_lsb_ = static_cast<std::uint16_t>(lsb),
						.exact_poc_         = false,
						.long_term_         = true,
				};
				if (slice.delta_poc_msb_present_flag[index] != 0U) {
					previous_delta_poc_msb += slice.delta_poc_msb_cycle_lt[index];
					target.pic_order_cnt_ = current_poc -
											static_cast<std::int32_t>(previous_delta_poc_msb) * max_poc_lsb -
											(current_poc & (max_poc_lsb - 1)) + static_cast<std::int32_t>(lsb);
					target.exact_poc_     = true;
				}
				sets.lt_.push_back(target);
			}
			return sets;
		}

		[[nodiscard]] std::uint8_t active_ref_count(const Slice_Header &slice, std::uint8_t list) {
			if (slice.type == kSlice_I) { return 0U; }
			if (list == 0U) {
				return static_cast<std::uint8_t>(std::min<std::uint32_t>(slice.num_ref_idx_l0_active_minus1 + 1U, 15U));
			}
			if (slice.type != kSlice_B) { return 0U; }
			return static_cast<std::uint8_t>(std::min<std::uint32_t>(slice.num_ref_idx_l1_active_minus1 + 1U, 15U));
		}

		[[nodiscard]] std::vector<Reference_Target> build_ref_pic_list(const Slice_Header &slice,
																	   const Reference_Sets &sets, std::uint8_t list, std::int32_t current_poc) {
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
			if (slice.pps->pps_scc_extension_params.pps_curr_pic_ref_enabled_flag) {
				temp.push_back({.pic_order_cnt_ = current_poc, .pic_order_cnt_lsb_ = static_cast<std::uint16_t>(current_poc),
					.exact_poc_ = true, .long_term_ = true});
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

		[[nodiscard]] int16_t clip_int16(std::int32_t value, std::int32_t low, std::int32_t high) {
			return static_cast<int16_t>(std::min(std::max(value, low), high));
		}

		void fill_weight_table(const Slice_Header &header, VASliceParameterBufferHEVCExtension &parameters) {
			auto       &base       = parameters.base;
			const auto &wp         = header.pred_weight_table;
			const auto *pps        = header.pps;
			const auto *sps        = pps == nullptr ? nullptr : pps->sps;
			const bool  monochrome = sps != nullptr && sps->chroma_format_idc == 0U;
			if (header.type == kSlice_I ||
				(header.type == kSlice_P && (pps == nullptr || pps->weighted_pred_flag == 0U)) ||
				(header.type == kSlice_B && (pps == nullptr || pps->weighted_bipred_flag == 0U))) {
				return;
			}
			const bool is_extension_profile =
					sps != nullptr && (sps->sps_range_extension_flag != 0U || sps->sps_scc_extension_flag != 0U ||
									   sps->chroma_format_idc > 1U || (sps->bit_depth_luma_minus8 + 8U) > 10U);
			base.luma_log2_weight_denom         = wp.luma_log2_weight_denom;
			base.delta_chroma_log2_weight_denom = monochrome ? 0 : wp.delta_chroma_log2_weight_denom;
			const std::int32_t chroma_denom = static_cast<std::int32_t>(wp.luma_log2_weight_denom) + wp.delta_chroma_log2_weight_denom;
			for (std::size_t index = 0; index < 15U; ++index) {
				base.delta_luma_weight_l0[index]      = wp.delta_luma_weight_l0[index];
				base.delta_luma_weight_l1[index]      = wp.delta_luma_weight_l1[index];
				parameters.rext.luma_offset_l0[index] = wp.luma_offset_l0[index];
				parameters.rext.luma_offset_l1[index] = wp.luma_offset_l1[index];
				if (!is_extension_profile) {
					base.luma_offset_l0[index] = wp.luma_offset_l0[index];
					base.luma_offset_l1[index] = wp.luma_offset_l1[index];
				}
				for (std::size_t component = 0; component < 2U; ++component) {
					base.delta_chroma_weight_l0[index][component] = wp.delta_chroma_weight_l0[index][component];
					base.delta_chroma_weight_l1[index][component] = wp.delta_chroma_weight_l1[index][component];
					const std::int32_t weight_l0 = (1 << chroma_denom) + wp.delta_chroma_weight_l0[index][component];
					const std::int32_t weight_l1 = (1 << chroma_denom) + wp.delta_chroma_weight_l1[index][component];
					const std::int32_t offset_l0 = std::clamp<std::int32_t>(128 + wp.delta_chroma_offset_l0[index][component] -
																  ((128 * weight_l0) >> chroma_denom),
														  -128, 127);
					const std::int32_t offset_l1 = std::clamp<std::int32_t>(128 + wp.delta_chroma_offset_l1[index][component] -
																  ((128 * weight_l1) >> chroma_denom),
														  -128, 127);
					if (!is_extension_profile) {
						base.ChromaOffsetL0[index][component] = static_cast<int8_t>(offset_l0);
						base.ChromaOffsetL1[index][component] = static_cast<int8_t>(offset_l1);
					}
					parameters.rext.ChromaOffsetL0[index][component] = clip_int16(offset_l0, -32768, 32767);
					parameters.rext.ChromaOffsetL1[index][component] = clip_int16(offset_l1, -32768, 32767);
				}
			}
		}

		[[nodiscard]] VASliceParameterBufferHEVCExtension
		make_slice_parameters(const Parsed_Decode_Slice &slice, std::uint32_t slice_data_size, bool last_slice,
							  std::int32_t current_poc, std::span<const Va_Decode_Reference> references,
							  std::uint32_t subset_offset_index) {
			const auto                         &header = slice.header_;
			VASliceParameterBufferHEVCExtension parameters{};
			auto                               &base = parameters.base;
			base.slice_data_size                     = slice_data_size;
			base.slice_data_offset                   = 0;
			base.slice_data_flag                     = VA_SLICE_DATA_FLAG_ALL;
			base.slice_data_byte_offset              = slice_data_byte_offset(slice);
			base.slice_segment_address               = header.segment_address;
			base.collocated_ref_idx = header.temporal_mvp_enabled_flag != 0U ? header.collocated_ref_idx : 0xFF;
			base.num_ref_idx_l0_active_minus1 =
					active_ref_count(header, 0U) > 0U ? active_ref_count(header, 0U) - 1U : 0U;
			base.num_ref_idx_l1_active_minus1 =
					active_ref_count(header, 1U) > 0U ? active_ref_count(header, 1U) - 1U : 0U;
			base.slice_qp_delta                 = header.qp_delta;
			base.slice_cb_qp_offset             = header.cb_qp_offset;
			base.slice_cr_qp_offset             = header.cr_qp_offset;
			base.slice_beta_offset_div2         = header.beta_offset_div2;
			base.slice_tc_offset_div2           = header.tc_offset_div2;
			base.five_minus_max_num_merge_cand  = header.five_minus_max_num_merge_cand;
			base.num_entry_point_offsets        = static_cast<std::uint16_t>(header.num_entry_point_offsets);
			base.entry_offset_to_subset_array   = static_cast<std::uint16_t>(subset_offset_index);
			base.slice_data_num_emu_prevn_bytes = static_cast<std::uint16_t>(header.n_emulation_prevention_bytes);
			std::memset(base.RefPicList, 0xFF, sizeof(base.RefPicList));

			const auto sets = derive_reference_sets(header, current_poc);
			for (std::uint8_t list = 0; list < 2U; ++list) {
				const auto rpl = build_ref_pic_list(header, sets, list, current_poc);
				for (std::uint8_t index = 0; index < rpl.size() && index < 15U; ++index) {
					const auto slot = find_va_reference_slot(references, rpl[index]);
					if (slot) { base.RefPicList[list][index] = *slot; }
				}
			}

			base.LongSliceFlags.fields.LastSliceOfPic                        = last_slice;
			base.LongSliceFlags.fields.dependent_slice_segment_flag          = header.dependent_slice_segment_flag;
			base.LongSliceFlags.fields.slice_type                            = header.type;
			base.LongSliceFlags.fields.color_plane_id                        = header.colour_plane_id;
			base.LongSliceFlags.fields.slice_sao_luma_flag                   = header.sao_luma_flag;
			base.LongSliceFlags.fields.slice_sao_chroma_flag                 = header.sao_chroma_flag;
			base.LongSliceFlags.fields.mvd_l1_zero_flag                      = header.mvd_l1_zero_flag;
			base.LongSliceFlags.fields.cabac_init_flag                       = header.cabac_init_flag;
			base.LongSliceFlags.fields.slice_temporal_mvp_enabled_flag       = header.temporal_mvp_enabled_flag;
			base.LongSliceFlags.fields.slice_deblocking_filter_disabled_flag = header.deblocking_filter_disabled_flag;
			base.LongSliceFlags.fields.collocated_from_l0_flag               = header.collocated_from_l0_flag;
			base.LongSliceFlags.fields.slice_loop_filter_across_slices_enabled_flag =
					header.loop_filter_across_slices_enabled_flag;

			fill_weight_table(header, parameters);
			parameters.rext.slice_ext_flags.bits.cu_chroma_qp_offset_enabled_flag =
					header.cu_chroma_qp_offset_enabled_flag;
			parameters.rext.slice_ext_flags.bits.use_integer_mv_flag = header.use_integer_mv_flag;
			parameters.rext.slice_act_y_qp_offset                    = header.slice_act_y_qp_offset;
			parameters.rext.slice_act_cb_qp_offset                   = header.slice_act_cb_qp_offset;
			parameters.rext.slice_act_cr_qp_offset                   = header.slice_act_cr_qp_offset;
			return parameters;
		}

		[[nodiscard]] Result<void> create_va_buffer(VADisplay display, VAContextID context, VABufferType type,
													std::uint32_t element_size, std::uint32_t element_count, void *data,
													std::vector<VABufferID> &buffers, std::string_view step) {
			buffers.push_back(VA_INVALID_ID);
			const VAStatus status =
					vaCreateBuffer(display, context, type, element_size, element_count, data, &buffers.back());
			if (status != VA_STATUS_SUCCESS) {
				return std::unexpected(error(Media_Error_Code::VAAPI_SUBMISSION_FAILED,
											 std::string{"failed to create H.265 VA "} + std::string{step} + ": " +
													 vaErrorStr(status)));
			}
			return {};
		}

		[[nodiscard]] Result<void> submit_frame(VADisplay display, VAContextID context, VASurfaceID current_surface,
												const H265_Sequence_Info &sequence, std::uint32_t decode_frame_index,
												const Parsed_Decode_Frame            &frame,
												const decoder::Frame_Reference_State &state,
												std::span<const Va_Decode_Reference>  references,
												std::span<const std::uint8_t>         bitstream,
				std::uint64_t logical_bitstream_begin) {
			auto picture = make_picture_parameters(frame, sequence, current_surface, state, references);
			std::vector<VABufferID> control_buffers;
			std::vector<VABufferID> slice_buffers;
			const auto              destroy_buffers = [&] {
				for (const auto buffer: control_buffers) {
					if (buffer != VA_INVALID_ID) { (void) vaDestroyBuffer(display, buffer); }
				}
				for (const auto buffer: slice_buffers) {
					if (buffer != VA_INVALID_ID) { (void) vaDestroyBuffer(display, buffer); }
				}
			};

			const auto picture_size =
					uses_extension_picture_buffers(sequence) ? sizeof(picture) : sizeof(VAPictureParameterBufferHEVC);
			auto created = create_va_buffer(display, context, VAPictureParameterBufferType,
											static_cast<std::uint32_t>(picture_size), 1, &picture, control_buffers,
											"picture parameter buffer");
			if (!created) {
				destroy_buffers();
				return std::unexpected(created.error());
			}

			const auto                         &pps = *frame.slices_.front().header_.pps;
			const auto                         &sps = *pps.sps;
			std::optional<VAIQMatrixBufferHEVC> scaling_matrix;
			if (pps.scaling_list_data_present_flag != 0U) {
				scaling_matrix = make_iq_matrix(pps.scaling_list);
			} else if (sps.scaling_list_enabled_flag != 0U && sps.scaling_list_data_present_flag != 0U) {
				scaling_matrix = make_iq_matrix(sps.scaling_list);
			} else if (sps.scaling_list_enabled_flag != 0U) {
				scaling_matrix = make_default_iq_matrix();
			}
			if (scaling_matrix.has_value()) {
				created = create_va_buffer(display, context, VAIQMatrixBufferType, sizeof(*scaling_matrix), 1,
										   &*scaling_matrix, control_buffers, "IQ matrix buffer");
				if (!created) {
					destroy_buffers();
					return std::unexpected(created.error());
				}
			}

			const auto slice_size = uses_extension_picture_buffers(sequence)
											? sizeof(VASliceParameterBufferHEVCExtension)
											: sizeof(VASliceParameterBufferHEVC);
			std::vector<VASliceParameterBufferHEVCExtension> slice_params;
			slice_params.reserve(frame.slices_.size());
			std::vector<std::uint32_t> subset_offsets;
			std::uint32_t              subset_offset_count = 0;
			for (const auto &slice: frame.slices_) {
				if (slice.header_.num_entry_point_offsets > 0U && !slice.header_.entry_point_offsets) {
					destroy_buffers();
					return std::unexpected(
							error(Media_Error_Code::INVALID_H265_BITSTREAM,
								  "H.265 slice declares entry-point offsets without parsed offset data"));
				}
				if (slice.header_.num_entry_point_offsets >
					std::numeric_limits<std::uint32_t>::max() - subset_offset_count) {
					destroy_buffers();
					return std::unexpected(error(Media_Error_Code::INVALID_H265_BITSTREAM,
												 "H.265 slice entry-point offset table is too large"));
				}
				subset_offset_count += slice.header_.num_entry_point_offsets;
			}
			subset_offsets.reserve(subset_offset_count);
			for (std::size_t index = 0; index < frame.slices_.size(); ++index) {
				const auto &slice = frame.slices_[index];
				auto payload = decoder::va_slice_payload(slice, bitstream, logical_bitstream_begin);
				if (!payload) { destroy_buffers(); return std::unexpected(payload.error()); }
				const auto slice_buffer_size = static_cast<std::uint32_t>(payload->size());
				const auto  subset_offset_index = static_cast<std::uint32_t>(subset_offsets.size());
				for (std::uint32_t entry = 0; entry < slice.header_.num_entry_point_offsets; ++entry) {
					subset_offsets.push_back((*slice.header_.entry_point_offsets)[entry]);
				}
				slice_params.push_back(make_slice_parameters(slice, slice_buffer_size,
															 index + 1U == frame.slices_.size(), state.pic_order_cnt_,
															 references, subset_offset_index));
				created = create_va_buffer(display, context, VASliceParameterBufferType,
										   static_cast<std::uint32_t>(slice_size), 1, &slice_params.back(),
										   slice_buffers, "slice parameter buffer");
				if (!created) {
					destroy_buffers();
					return std::unexpected(created.error());
				}
				auto *slice_data = const_cast<std::uint8_t *>(payload->data());
				created = create_va_buffer(display, context, VASliceDataBufferType, slice_buffer_size, 1, slice_data,
										   slice_buffers, "slice data buffer");
				if (!created) {
					destroy_buffers();
					return std::unexpected(created.error());
				}
			}
			if (!subset_offsets.empty()) {
				const auto subset_bytes = subset_offsets.size() * sizeof(std::uint32_t);
				if (subset_bytes > std::numeric_limits<std::uint32_t>::max()) {
					destroy_buffers();
					return std::unexpected(error(Media_Error_Code::INVALID_H265_BITSTREAM,
												 "H.265 slice entry-point offset table is too large for VA"));
				}
				created = create_va_buffer(display, context, VASubsetsParameterBufferType,
										   static_cast<std::uint32_t>(subset_bytes), 1, subset_offsets.data(),
										   control_buffers, "subset entry-point buffer");
				if (!created) {
					destroy_buffers();
					return std::unexpected(created.error());
				}
			}

			std::string_view va_step = "begin picture";
			VAStatus         status  = vaBeginPicture(display, context, current_surface);
			if (status == VA_STATUS_SUCCESS) {
				va_step = "render picture parameters";
				status  = vaRenderPicture(display, context, control_buffers.data(),
										  static_cast<std::int32_t>(control_buffers.size()));
			}
			if (status == VA_STATUS_SUCCESS && !slice_buffers.empty()) {
				va_step = "render slice data";
				status =
						vaRenderPicture(display, context, slice_buffers.data(), static_cast<std::int32_t>(slice_buffers.size()));
			}
			if (status == VA_STATUS_SUCCESS) {
				va_step = "end picture";
				status  = vaEndPicture(display, context);
			}
			destroy_buffers();
			if (status != VA_STATUS_SUCCESS) {
				std::ostringstream message;
				message << "failed to submit H.265 VA decode frame at " << va_step << ": " << vaErrorStr(status)
						<< " frame=" << decode_frame_index << " slices=" << frame.slices_.size()
						<< " poc=" << state.pic_order_cnt_;
				for (const auto &reference: references) {
					message << " ref" << static_cast<std::uint32_t>(reference.slot_) << "_poc=" << reference.pic_order_cnt_
							<< "_flags=" << reference.va_flags_;
				}
				return std::unexpected(error(Media_Error_Code::VAAPI_SUBMISSION_FAILED, message.str()));
			}
			return {};
		}


		struct Decode_Surface {
			VADisplay display;
			VAConfigID config = VA_INVALID_ID;
			VAContextID context = VA_INVALID_ID;
			VASurfaceID surface = VA_INVALID_SURFACE;
			VASurfaceID alternate = VA_INVALID_SURFACE;
			~Decode_Surface() {
				if (context != VA_INVALID_ID) { (void) vaDestroyContext(display, context); }
				if (surface != VA_INVALID_SURFACE) { (void) vaDestroySurfaces(display, &surface, 1); }
				if (alternate != VA_INVALID_SURFACE) { (void) vaDestroySurfaces(display, &alternate, 1); }
				if (config != VA_INVALID_ID) { (void) vaDestroyConfig(display, config); }
			}
		};
		struct Decoder {
			std::string device;
			Frame_Layout layout;
			VAProfile profile;
			std::shared_ptr<Va_Display> display;
			Decode_Surface target;
			bool inter{};
			std::optional<std::uint16_t> previous_poc;
			std::uint64_t owner = new_surface_owner();

			[[nodiscard]] Result<void> init() {
				VAConfigAttrib attribute{.type = VAConfigAttribRTFormat, .value = layout.rt_format()};
				auto status = vaCreateConfig(target.display, profile, VAEntrypointVLD, &attribute, 1, &target.config);
				if (!status_ok(status)) {
					return std::unexpected(error(Media_Error_Code::UNSUPPORTED_DRIVER_FEATURE,
												 std::string("create H.265 decode config: ") + vaErrorStr(status)));
				}
				auto surface = create_surface(target.display, layout, VA_SURFACE_ATTRIB_USAGE_HINT_DECODER);
				if (!surface) { return std::unexpected(surface.error()); }
				target.surface = *surface;
				if (inter) {
					auto alternate = create_surface(target.display, layout, VA_SURFACE_ATTRIB_USAGE_HINT_DECODER);
					if (!alternate) { return std::unexpected(alternate.error()); }
					target.alternate = *alternate;
				}
				std::array surfaces{target.surface, target.alternate};
				status = vaCreateContext(target.display, target.config, layout.width, layout.height,
										 VA_PROGRESSIVE, surfaces.data(), inter ? 2 : 1, &target.context);
				if (!status_ok(status)) {
					return std::unexpected(error(Media_Error_Code::VAAPI_SUBMISSION_FAILED,
												 std::string("create H.265 decode context: ") + vaErrorStr(status)));
				}
				return {};
			}
		};
	} // namespace

	using Decoder_Cache = std::array<std::unique_ptr<Decoder>, 2>;
	struct Decoder_State {
		std::array<Decoder_Cache, 2> streams;
	};
	Decoder_State *create_decoder_state() { return new Decoder_State{}; }
	void destroy_decoder_state(Decoder_State *state) { delete state; }

	std::uint32_t decode_formats(const char *device, std::span<const Frame_Format> formats) {
		const auto display = Va_Display::open(device);
		if (!display || formats.size() > 32) { return 0; }
		const auto handle = (*display)->get();
		std::uint32_t supported = 0;
		for (std::size_t i = 0; i < formats.size(); ++i) {
			const auto [chroma, depth, scc] = formats[i];
			H265_Profile profile = H265_Profile::UNSUPPORTED_EXTENSION;
			switch (chroma) {
				case 420:
					profile = scc ? (depth == 8 ? H265_Profile::SCC_MAIN : H265_Profile::SCC_MAIN_10)
								  : (depth == 8 ? H265_Profile::MAIN : H265_Profile::MAIN_10);
					break;
				case 422:
					profile = H265_Profile::MAIN_422_10;
					break;
				case 444:
					profile = scc ? (depth == 8 ? H265_Profile::SCC_MAIN_444 : H265_Profile::SCC_MAIN_444_10)
								  : (depth == 8 ? H265_Profile::MAIN_444 : H265_Profile::MAIN_444_10);
					break;
				default:
					continue;
			}
			const Frame_Layout layout{.width = 0, .height = 0, .depth = depth, .chroma = chroma};
			VAConfigAttrib attribute{.type = VAConfigAttribRTFormat, .value = layout.rt_format()};
			VAConfigID config = VA_INVALID_ID;
			const auto va_profile = va_profile_for(profile);
			if (va_profile && status_ok(vaCreateConfig(handle, *va_profile, VAEntrypointVLD, &attribute, 1, &config))) {
				supported |= 1U << i;
				(void) vaDestroyConfig(handle, config);
			}
		}
		return supported;
	}

	Result<Decoded_Frame>
	decode_picture(const char *device, Frame_Layout layout, bool scc, std::span<const std::uint8_t> bitstream, bool alpha,
			   bool keep_surface, std::optional<std::uint16_t> stream_poc, Decoder_State *decoder_state,
			   bool require_surface) {
		const auto poc = stream_poc.value_or(0);
		const bool predicted = poc != 0;
		const auto preceding_poc = static_cast<std::uint16_t>(predicted ? poc - 1 : 0);
		const auto previous_poc = predicted ? std::optional{preceding_poc} : std::nullopt;
		auto parsed = decoder::Annex_B_Parser{}.parse(bitstream, previous_poc);
		if (!parsed) { return std::unexpected(parsed.error()); }
		const auto &sequence = parsed->sequence_;
		const auto expected_chroma = layout.chroma == 420 ? H265_Chroma_Format::YUV420 :
									 layout.chroma == 422 ? H265_Chroma_Format::YUV422 : H265_Chroma_Format::YUV444;
		if (parsed->requires_unsupported_) {
			return std::unexpected(error(Media_Error_Code::UNSUPPORTED_BITSTREAM_FEATURE,
										 parsed->unsupported_message_));
		}
		if (!parsed->has_sps_ || parsed->decode_frames_.size() != 1 || parsed->frame_states_.size() != 1 ||
			parsed->frame_metadata_.size() != 1 || sequence.max_width_ != layout.width ||
			sequence.max_height_ != layout.height || sequence.bit_depth_ != layout.depth ||
			sequence.chroma_bit_depth_ != layout.depth || sequence.chroma_format_ != expected_chroma ||
			sequence.screen_content_ != scc || sequence.color_.primaries_ != Color_Primaries::BT709 ||
			sequence.color_.transfer_ != Transfer_Function::SRGB || sequence.color_.matrix_ != Matrix_Coefficients::BT709 ||
			sequence.color_.range_ != Color_Range::FULL ||
			(layout.chroma == 420 && sequence.color_.chroma_sample_position_ != Chroma_Sample_Position::MIDPOINT)) {
			return std::unexpected(error(Media_Error_Code::INVALID_H265_BITSTREAM,
										 "access unit disagrees with negotiated picture or color metadata"));
		}
		const auto &frame = parsed->decode_frames_.front();
		const auto &state = parsed->frame_states_.front();
		const auto &metadata = parsed->frame_metadata_.front();
		if (!frame.parameters_ || frame.slices_.empty() ||
			!state.output_ || state.field_picture_ || metadata.visible_x_ != 0 || metadata.visible_y_ != 0 ||
			metadata.visible_width_ != layout.width || metadata.visible_height_ != layout.height) {
			return std::unexpected(error(Media_Error_Code::INVALID_H265_BITSTREAM,
										 "expected one complete progressive H.265 picture"));
		}
		if (predicted) {
			const auto &slice = frame.slices_.front();
			if (frame.slices_.size() != 1 || !slice.header_.first_slice_segment_in_pic_flag ||
				metadata.irap_ || state.non_reference_ || state.pic_order_cnt_ != poc ||
				metadata.primary_slice_type_ != H265_Slice_Type::P ||
				metadata.used_reference_mask_ != 1 || metadata.active_reference_mask_ != 1 ||
				metadata.unavailable_reference_mask_ != 0 || state.reference_long_term_[0] ||
				state.reference_pic_order_cnt_[0] != preceding_poc ||
				slice.nalu_.type != decoder::Nal_Unit_Type::TRAIL_R || slice.header_.type != kSlice_P) {
				return std::unexpected(error(Media_Error_Code::INVALID_H265_BITSTREAM,
											 "expected one H.265 P picture referring to its predecessor"));
			}
		} else if (!metadata.irap_ || metadata.primary_slice_type_ != H265_Slice_Type::I ||
			metadata.used_reference_mask_ != 0 ||
			std::ranges::any_of(frame.slices_, [](const Parsed_Decode_Slice &slice) {
				return slice.nalu_.type != decoder::Nal_Unit_Type::IDR_W_RADL &&
					   slice.nalu_.type != decoder::Nal_Unit_Type::IDR_N_LP;
			})) {
			return std::unexpected(error(Media_Error_Code::INVALID_H265_BITSTREAM,
										 "expected one complete independent H.265 IDR picture"));
		}

		auto validated = validate_decode_frame_shape(frame);
		if (!validated) { return std::unexpected(validated.error()); }

		auto profile = va_profile_for(sequence);
		if (!profile) {
			return std::unexpected(error(Media_Error_Code::UNSUPPORTED_BITSTREAM_FEATURE,
										 "no VA profile for H.265 picture"));
		}
		const auto decode_vulkan = [&] {
			return vulkan_video::decode_idr(device, layout, frame, bitstream, alpha)
				.transform([](auto words) { return Decoded_Frame{std::move(words)}; });
		};
		thread_local Decoder_State standalone{};
		auto &cache = (decoder_state ? *decoder_state : standalone).streams[alpha];
		auto &decoder = cache.front();
		const auto matches = [&](const std::unique_ptr<Decoder> &entry) {
			return entry && entry->device == device && entry->layout == layout && entry->profile == *profile &&
				entry->inter == stream_poc.has_value();
		};
		if (!matches(decoder) && matches(cache.back())) {
			std::swap(decoder, cache.back());
		}
		if (!matches(decoder)) {
			auto display = Va_Display::open(device);
			if (!display) {
				if (!require_surface && !scc && !stream_poc) { return decode_vulkan(); }
				return std::unexpected(display.error());
			}
			const auto native = (*display)->get();
			// Construct at its final address. The target is destroyed before the display.
			auto next = std::unique_ptr<Decoder>(new Decoder{
				.device = device, .layout = layout, .profile = *profile, .display = std::move(*display),
				.target = {.display = native},
				.inter = stream_poc.has_value(), .previous_poc = std::nullopt,
			});
			auto initialized = next->init();
			if (!initialized) {
				if (!require_surface && !scc && !stream_poc) { return decode_vulkan(); }
				return std::unexpected(initialized.error());
			}
			if (decoder) {
				cache.back() = std::move(decoder);
			}
			decoder = std::move(next);
		}
		auto &target = decoder->target;
		if (predicted && decoder->previous_poc != previous_poc) {
			return std::unexpected(error(Media_Error_Code::INVALID_H265_BITSTREAM,
										 "previous H.265 reference has not been decoded"));
		}
		const bool odd = poc % 2 != 0;
		const auto current = odd ? target.alternate : target.surface;
		const std::array references{
			Va_Decode_Reference{
				.slot_ = 0,
				.surface_ = odd ? target.surface : target.alternate,
				.pic_order_cnt_ = previous_poc.value_or(0),
				.pic_order_lsb_ = previous_poc.value_or(0),
				.va_flags_ = state.reference_va_flags_[0],
			},
		};
		const auto inputs = predicted ? std::span<const Va_Decode_Reference>(references) : std::span<const Va_Decode_Reference>();
		auto submitted = submit_frame(target.display, target.context, current, sequence,
									  poc, frame, state, inputs, bitstream, 0);
		if (!submitted) {
			decoder.reset();
			return std::unexpected(submitted.error());
		}
		decoder->previous_poc = stream_poc;
		if (keep_surface || require_surface) {
			return Decoded_Frame{Va_Surface{.display = target.display, .surface = current, .owner = decoder->owner, .lifetime = decoder->display}};
		}
		auto decoded = read_frame(target.display, current, layout);
		if (!decoded) {
			decoder.reset();
			return std::unexpected(std::move(decoded.error()));
		}
		return Decoded_Frame{std::move(*decoded)};
	}
} // namespace video_editor::h265
