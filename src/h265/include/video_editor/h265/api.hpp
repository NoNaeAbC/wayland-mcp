#pragma once

#include "video_editor/av1/api.hpp"
#include "video_editor/color/hdr10plus.hpp"
#include "video_editor/color/dynamic_hdr.hpp"
#include "video_editor/encode/api.hpp"

#include <array>
#include <cstdint>
#include <expected>
#include <limits>
#include <memory>
#include <optional>
#include <span>
#include <string>
#include <utility>
#include <vector>

namespace video_editor::h265 {
	using av1::Chroma_Sample_Position;
	using av1::Color_Encoding;
	using av1::Color_Primaries;
	using av1::Color_Range;
	using av1::Encoded_Byte_Interval;
	using av1::Matrix_Coefficients;
	using av1::Media_Error;
	using av1::Media_Error_Code;
	using av1::Result;
	using av1::Transfer_Function;

	struct Frame_Id {
		std::uint64_t value_                         = 0;
		friend bool   operator==(Frame_Id, Frame_Id) = default;
	};

	inline constexpr std::uint32_t kInvalid_H265_Frame_Index = std::numeric_limits<std::uint32_t>::max();

	enum class H265_Profile : std::uint8_t {
		MAIN,
		MAIN_10,
		MAIN_12,
		MAIN_STILL_PICTURE,
		MAIN_422_10,
		MAIN_422_12,
		MAIN_444,
		MAIN_444_10,
		MAIN_444_12,
		SCC_MAIN,
		SCC_MAIN_10,
		SCC_MAIN_444,
		SCC_MAIN_444_10,
		UNSUPPORTED_EXTENSION,
	};
	enum class H265_Chroma_Format : std::uint8_t { MONOCHROME = 0, YUV420 = 1, YUV422 = 2, YUV444 = 3 };
	enum class H265_Slice_Type : std::uint8_t { B = 0, P = 1, I = 2 };

	struct H265_Sequence_Info {
		H265_Profile       profile_                       = H265_Profile::MAIN;
		std::uint32_t      max_width_                     = 0;
		std::uint32_t      max_height_                    = 0;
		std::uint8_t       bit_depth_                     = 8;
		std::uint8_t       chroma_bit_depth_              = 8;
		H265_Chroma_Format chroma_format_                 = H265_Chroma_Format::YUV420;
		bool               range_extension_               = false;
		bool               screen_content_                = false;
		bool               high_precision_offsets_        = false;
		bool               extended_precision_processing_ = false;
		Color_Encoding     color_;
	};

	struct H265_Frame_Decode_Metadata {
		color::Dynamic_HDR dynamic_hdr_ = {};
		std::optional<color::Hdr10_Plus> hdr10_plus_ = std::nullopt;
		Encoded_Byte_Interval         bitstream_;
		std::array<std::uint32_t, 15> reference_frame_index_{
				kInvalid_H265_Frame_Index, kInvalid_H265_Frame_Index, kInvalid_H265_Frame_Index,
				kInvalid_H265_Frame_Index, kInvalid_H265_Frame_Index, kInvalid_H265_Frame_Index,
				kInvalid_H265_Frame_Index, kInvalid_H265_Frame_Index, kInvalid_H265_Frame_Index,
				kInvalid_H265_Frame_Index, kInvalid_H265_Frame_Index, kInvalid_H265_Frame_Index,
				kInvalid_H265_Frame_Index, kInvalid_H265_Frame_Index, kInvalid_H265_Frame_Index};
		std::uint16_t   used_reference_mask_        = 0;
		// Subset of used_reference_mask_ which the current picture actually
		// addresses through RefPicSetStCurrBefore/After or RefPicSetLtCurr.
		// Other used slots describe retained virtual-DPB occupancy.
		std::uint16_t   active_reference_mask_      = 0;
		std::uint16_t   unavailable_reference_mask_ = 0;
		H265_Slice_Type primary_slice_type_         = H265_Slice_Type::I;
		bool            irap_                       = false;
		std::uint32_t   visible_x_                  = 0;
		std::uint32_t   visible_y_                  = 0;
		std::uint32_t   visible_width_              = 0;
		std::uint32_t   visible_height_             = 0;
	};

	struct H265_Rate_Control {
		std::uint32_t target_bitrate_       = 0;
		std::uint32_t maximum_bitrate_      = 0;
		std::uint32_t target_percentage_    = 100;
		std::uint32_t cpb_size_bits_        = 0;
		std::uint8_t  initial_qp_            = 26;
		std::uint8_t  minimum_qp_            = 1;
		std::uint8_t  maximum_qp_            = 51;
		std::uint8_t  driver_quality_level_  = 0;
	};

	struct H265_Encode_Plan {
		encode::Resolved_Stream_Plan stream_;
		Color_Encoding               output_color_;
		H265_Rate_Control            rate_control_;
		std::int8_t                  chroma_qp_offset_  = 0;
		std::uint32_t                intra_period_      = 30;
		std::uint32_t                intra_idr_period_  = 30;
		std::uint32_t                ip_period_         = 1;
		std::uint8_t                 _decoded_picture_buffer_size = 0; // zero uses the mode default
		std::uint8_t                 level_idc_         = 120;
		bool                         high_tier_         = false;
		std::uint8_t                 picture_order_count_lsb_bits_ = 16;
		std::uint8_t                 slice_count_       = 1;
		bool                         sample_adaptive_offset_ = true;
		bool                         temporal_mvp_           = true;
		bool                         strong_intra_smoothing_ = true;
		bool                         transform_skip_         = false;
		bool                         constrained_intra_prediction_ = false;
		bool                         palette_mode_           = false;
		bool                         intra_block_copy_       = false;
	};

	struct H265_Encode_Picture {
		Frame_Id                 frame_;
		H265_Slice_Type          type_ = H265_Slice_Type::P;
		std::int32_t             picture_order_count_ = 0;
		bool                     idr_                  = false;
		bool                     reference_            = true;
		std::vector<Frame_Id>    references_l0_;
		std::vector<Frame_Id>    references_l1_;
		std::vector<Frame_Id>    _retained_references{}; // in the RPS, unused by this picture
		std::optional<std::uint8_t> cqp_;
		std::uint8_t              _pyramid_level = 0;
		bool                     read_reconstruction_ = true;
	};

	struct H265_Encoded_Picture {
		Frame_Id                  frame_;
		std::vector<std::uint8_t> annex_b_;
		std::vector<std::uint32_t> reconstruction_;
	};

	class H265_Encode_Context {
	public:
		virtual ~H265_Encode_Context() = default;
		[[nodiscard]] virtual Result<H265_Encoded_Picture>
		encode(std::span<const std::uint32_t> pixels, const H265_Encode_Picture &picture) = 0;
		[[nodiscard]] virtual Result<void> release_reference(Frame_Id frame) = 0;
	};

} // namespace video_editor::h265
