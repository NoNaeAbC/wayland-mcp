#pragma once

#include <vulkan/vulkan.h>

#include <cstdint>
#include <string>
#include <vector>

namespace video_editor::encode {
	enum class Codec : std::uint8_t { AV1, H265 };
	enum class Chroma_Layout : std::uint8_t { YUV420, YUV422, YUV444 };
	enum class Rate_Control_Mode : std::uint8_t { CQP, ICQ, VBR, CBR, QVBR, VCM, TCBRC };
	enum class Restriction_Source : std::uint8_t {
		CODEC_SYNTAX,
		DEVICE_CAPABILITY,
		BACKEND_IMPLEMENTATION,
		FRAME_CONVERSION,
	};

	struct Encode_Mode_Id {
		std::uint32_t value_                               = 0;
		friend bool   operator==(Encode_Mode_Id, Encode_Mode_Id) = default;
	};

	struct Dimension_Restrictions {
		std::uint32_t minimum_width_    = 1;
		std::uint32_t minimum_height_   = 1;
		std::uint32_t maximum_width_    = 0;
		std::uint32_t maximum_height_   = 0;
		std::uint32_t width_alignment_  = 1;
		std::uint32_t height_alignment_ = 1;
	};

	struct Rate_Control_Restrictions {
		std::vector<Rate_Control_Mode> modes_;
	};

	struct Codec_Level_Restriction {
		std::uint8_t  level_value_                = 0;
		std::uint64_t maximum_picture_samples_    = 0;
		std::uint64_t maximum_sample_rate_         = 0;
		std::uint64_t maximum_main_tier_bitrate_   = 0;
		std::uint64_t maximum_high_tier_bitrate_   = 0; // zero means high tier is forbidden
	};

	// One row is one exact coded representation and feature family. Conditional
	// relationships are represented by separate rows, so consumers do not need
	// an encoder-private constraint evaluator.
	struct Encode_Mode_Restrictions {
		Encode_Mode_Id             id_;
		Codec                      codec_        = Codec::AV1;
		std::uint8_t               bit_depth_    = 8;
		Chroma_Layout              chroma_       = Chroma_Layout::YUV420;
		bool                       screen_content_ = false;
		Dimension_Restrictions     dimensions_;
		Rate_Control_Restrictions  rate_control_;
		std::vector<Codec_Level_Restriction> levels_;
		std::uint32_t              max_reference_l0_ = 0;
		std::uint32_t              max_reference_l1_ = 0;
		std::uint32_t              max_slices_       = 1;
		std::uint32_t              coding_tree_block_size_ = 0;
		bool                       slices_equal_ctu_rows_   = false;
		bool                       device_slice_max_size_   = false;
		bool                       tiles_             = false;
		bool                       roi_qp_delta_      = false;
		bool                       intra_refresh_rows_    = false;
		bool                       intra_refresh_columns_ = false;

		// Raw device capabilities are separate from backend-selectable plan
		// features. A planner must only select the fields above; the device
		// fields explain why a future backend revision may expose more choices.
		bool                       device_tiles_                  = false;
		bool                       device_roi_qp_delta_           = false;
		bool                       device_intra_refresh_rows_     = false;
		bool                       device_intra_refresh_columns_  = false;

		// Empty means that the device mode exists but the current public import
		// path cannot feed it. This is deliberately distinct from hardware support.
		std::vector<VkFormat> implemented_source_formats_;
		std::string           device_mode_name_;
	};

	struct Encode_Plan_Restrictions {
		// Plans are valid only against the exact snapshot from which they were
		// selected. An API instance may reject a plan after device/backend change.
		std::uint64_t                         identity_ = 0;
		std::vector<Encode_Mode_Restrictions> modes_;
	};

	struct Resolved_Stream_Plan {
		std::uint64_t     restrictions_identity_ = 0;
		Encode_Mode_Id    mode_;
		std::uint32_t     width_         = 0;
		std::uint32_t     height_        = 0;
		std::uint32_t     framerate_num_ = 0;
		std::uint32_t     framerate_den_ = 1;
		VkFormat          source_format_ = VK_FORMAT_UNDEFINED;
		Rate_Control_Mode rate_control_  = Rate_Control_Mode::CQP;
	};

	[[nodiscard]] inline const Encode_Mode_Restrictions *find_mode(
			const Encode_Plan_Restrictions &restrictions, Encode_Mode_Id id) noexcept {
		for (const auto &mode: restrictions.modes_) {
			if (mode.id_ == id) { return &mode; }
		}
		return nullptr;
	}

	[[nodiscard]] inline bool contains_rate_control(
			const Rate_Control_Restrictions &restrictions, Rate_Control_Mode mode) noexcept {
		for (const auto candidate: restrictions.modes_) {
			if (candidate == mode) { return true; }
		}
		return false;
	}
} // namespace video_editor::encode
