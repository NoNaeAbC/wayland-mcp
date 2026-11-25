#pragma once

#include "video_editor/encode/api.hpp"

#include <vulkan/vulkan.h>

#include <array>
#include <cstdint>
#include <expected>
#include <limits>
#include <memory>
#include <mutex>
#include <optional>
#include <span>
#include <string>
#include <string_view>
#include <type_traits>
#include <utility>
#include <variant>
#include <vector>

namespace video_editor::av1 {
	template<typename T>
	using Result = std::expected<T, struct Media_Error>;

	class Av1_Editor_Bitstream_Api;
	enum class Decode_Backend : std::uint8_t { UNKNOWN, VA, VULKAN_VIDEO };
	class Decode_Context {
	public:
		Decode_Context() = default;
		virtual ~Decode_Context() = default;
		[[nodiscard]] Decode_Backend backend() const noexcept { return backend_; }
	protected:
		explicit Decode_Context(Decode_Backend backend) : backend_(backend) {}
	private:
		Decode_Backend backend_ = Decode_Backend::UNKNOWN;
	};

	class Av1_Decode_Context : public Decode_Context {
	public:
		using Decode_Context::Decode_Context;
		~Av1_Decode_Context() override                       = default;
		virtual void release_source(struct Source_Id source) = 0;
	};

	class Encode_Session {
	public:
		virtual ~Encode_Session() = default;
	};

	enum class Media_Error_Code : std::uint8_t {
		INVALID_AV1_BITSTREAM,
		INVALID_VP8_BITSTREAM,
		INVALID_VP9_BITSTREAM,
		INVALID_H264_BITSTREAM,
		INVALID_H265_BITSTREAM,
		INVALID_H266_BITSTREAM,
		UNSUPPORTED_BITSTREAM_FEATURE,
		UNSUPPORTED_DRIVER_FEATURE,
		INVALID_REFERENCE_STATE,
		INVALID_ARGUMENT,
		VULKAN_INTEROP_FAILED,
		VAAPI_SUBMISSION_FAILED,
		DECODE_DEVICE_UNAVAILABLE,
		DECODER_INITIALIZATION_FAILED,
		UNSUPPORTED_DECODE_BACKEND,
		VIDEO_TRACK_INDEX_FAILED,
	};

	enum class Video_Chroma_Format : std::uint8_t { MONOCHROME = 0, YUV420 = 1, YUV422 = 2, YUV444 = 3 };

	struct Video_Sample_Layout {
		Video_Chroma_Format chroma_;
		std::uint8_t luma_bits_;
		std::uint8_t chroma_bits_;
	};

	struct Unsupported_Coded_Profile {
		std::string_view codec_;
		std::uint8_t idc_;
		std::string_view name_;
		std::optional<Video_Sample_Layout> layout_;
	};

	struct Unsupported_Decoded_Layout {
		std::string_view codec_;
		Video_Sample_Layout source_;
		Video_Chroma_Format required_chroma_;
	};

	struct Media_Error {
		Media_Error_Code code_        = Media_Error_Code::INVALID_ARGUMENT;
		std::uint64_t    byte_offset_ = 0;
		std::uint64_t    bit_offset_  = 0;
		std::string      message_;
		std::variant<std::monostate, Unsupported_Coded_Profile, Unsupported_Decoded_Layout> restriction_{};
	};

	enum class Av1_Profile : std::uint8_t { PROFILE_0, PROFILE_1, PROFILE_2 };
	enum class Chroma_Sample_Position : std::uint8_t { UNKNOWN, COSITED, MIDPOINT };
	enum class Color_Range : std::uint8_t { LIMITED, FULL };
	enum class Matrix_Coefficients : std::uint8_t {
		IDENTITY    = 0,
		BT709       = 1,
		BT_709      = BT709,
		UNSPECIFIED = 2,
		FCC         = 4,
		BT470_BG    = 5,
		BT601       = 6,
		BT_601      = BT601,
		SMPTE240    = 7,
		YCGCO       = 8,
		BT2020_NCL  = 9,
		BT_2020_NCL = BT2020_NCL,
		BT2020_CL   = 10,
		SMPTE2085   = 11,
		CHROMAT_NCL = 12,
		CHROMAT_CL  = 13,
		ICTCP       = 14,
		IPT_C2      = 15,
		YCGCO_RE    = 16,
		YCGCO_RO    = 17,
	};
	enum class Color_Primaries : std::uint8_t {
		BT709        = 1,
		BT_709       = BT709,
		UNSPECIFIED  = 2,
		BT470_M      = 4,
		BT470_BG     = 5,
		BT601_525    = 6,
		BT_601       = BT601_525,
		SMPTE240     = 7,
		GENERIC_FILM = 8,
		BT2020       = 9,
		BT_2020      = BT2020,
		XYZ          = 10,
		SMPTE431     = 11,
		SMPTE432     = 12,
		EBU3213      = 22,
	};
	enum class Transfer_Function : std::uint8_t {
		BT1886       = 1,
		BT_709       = BT1886,
		UNSPECIFIED  = 2,
		BT470_M      = 4,
		BT470_BG     = 5,
		SMPTE170M    = 6,
		SMPTE240M    = 7,
		LINEAR       = 8,
		LOG_100      = 9,
		LOG_316      = 10,
		IEC61966_2_4 = 11,
		BT1361       = 12,
		SRGB         = 13,
		BT2020_10BIT = 14,
		BT2020_12BIT = 15,
		PQ           = 16,
		SMPTE_2084   = PQ,
		ST428        = 17,
		HLG          = 18,
	};

	[[nodiscard]] constexpr Color_Primaries color_primaries_from_h273_code(std::uint8_t value) noexcept {
		switch (value) {
			case 1U:
				return Color_Primaries::BT709;
			case 2U:
				return Color_Primaries::UNSPECIFIED;
			case 4U:
				return Color_Primaries::BT470_M;
			case 5U:
				return Color_Primaries::BT470_BG;
			case 6U:
				return Color_Primaries::BT601_525;
			case 7U:
				return Color_Primaries::SMPTE240;
			case 8U:
				return Color_Primaries::GENERIC_FILM;
			case 9U:
				return Color_Primaries::BT2020;
			case 10U:
				return Color_Primaries::XYZ;
			case 11U:
				return Color_Primaries::SMPTE431;
			case 12U:
				return Color_Primaries::SMPTE432;
			case 22U:
				return Color_Primaries::EBU3213;
			default:
				return Color_Primaries::UNSPECIFIED;
		}
	}

	[[nodiscard]] constexpr Transfer_Function transfer_function_from_h273_code(std::uint8_t value) noexcept {
		switch (value) {
			case 1U:
				return Transfer_Function::BT1886;
			case 2U:
				return Transfer_Function::UNSPECIFIED;
			case 4U:
				return Transfer_Function::BT470_M;
			case 5U:
				return Transfer_Function::BT470_BG;
			case 6U:
				return Transfer_Function::SMPTE170M;
			case 7U:
				return Transfer_Function::SMPTE240M;
			case 8U:
				return Transfer_Function::LINEAR;
			case 9U:
				return Transfer_Function::LOG_100;
			case 10U:
				return Transfer_Function::LOG_316;
			case 11U:
				return Transfer_Function::IEC61966_2_4;
			case 12U:
				return Transfer_Function::BT1361;
			case 13U:
				return Transfer_Function::SRGB;
			case 14U:
				return Transfer_Function::BT2020_10BIT;
			case 15U:
				return Transfer_Function::BT2020_12BIT;
			case 16U:
				return Transfer_Function::PQ;
			case 17U:
				return Transfer_Function::ST428;
			case 18U:
				return Transfer_Function::HLG;
			default:
				return Transfer_Function::UNSPECIFIED;
		}
	}

	[[nodiscard]] constexpr Matrix_Coefficients matrix_coefficients_from_h273_code(std::uint8_t value) noexcept {
		switch (value) {
			case 0U:
				return Matrix_Coefficients::IDENTITY;
			case 1U:
				return Matrix_Coefficients::BT709;
			case 2U:
				return Matrix_Coefficients::UNSPECIFIED;
			case 4U:
				return Matrix_Coefficients::FCC;
			case 5U:
				return Matrix_Coefficients::BT470_BG;
			case 6U:
				return Matrix_Coefficients::BT601;
			case 7U:
				return Matrix_Coefficients::SMPTE240;
			case 8U:
				return Matrix_Coefficients::YCGCO;
			case 9U:
				return Matrix_Coefficients::BT2020_NCL;
			case 10U:
				return Matrix_Coefficients::BT2020_CL;
			case 11U:
				return Matrix_Coefficients::SMPTE2085;
			case 12U:
				return Matrix_Coefficients::CHROMAT_NCL;
			case 13U:
				return Matrix_Coefficients::CHROMAT_CL;
			case 14U:
				return Matrix_Coefficients::ICTCP;
			case 15U:
				return Matrix_Coefficients::IPT_C2;
			case 16U:
				return Matrix_Coefficients::YCGCO_RE;
			case 17U:
				return Matrix_Coefficients::YCGCO_RO;
			default:
				return Matrix_Coefficients::UNSPECIFIED;
		}
	}

	[[nodiscard]] constexpr std::uint8_t h273_code(Color_Primaries primaries) noexcept {
		return static_cast<std::uint8_t>(primaries);
	}

	[[nodiscard]] constexpr std::uint8_t h273_code(Transfer_Function transfer) noexcept {
		return static_cast<std::uint8_t>(transfer);
	}

	[[nodiscard]] constexpr std::uint8_t h273_code(Matrix_Coefficients matrix) noexcept {
		return static_cast<std::uint8_t>(matrix);
	}

	struct Color_Chromaticity {
		double x_ = 0.0;
		double y_ = 0.0;
	};

	struct Mastering_Display_Color_Volume {
		Color_Chromaticity red_;
		Color_Chromaticity green_;
		Color_Chromaticity blue_;
		Color_Chromaticity white_;
		double             min_luminance_ = 0.0;
		double             max_luminance_ = 0.0;
	};

	struct Content_Light_Level {
		std::uint16_t max_content_light_level_       = 0;
		std::uint16_t max_frame_average_light_level_ = 0;
	};

	// TODO(color-management): Color metadata is a boundary contract, not an
	// editor working-format contract. Preserve bitstream/container color metadata
	// through decode indexing and test round-tripping through encode/decode. The
	// editor works in linear BT.2020 RGBA16F; encoding must let callers explicitly
	// specify primaries, transfer, matrix coefficients, range, chroma siting,
	// mastering display metadata, and content light metadata instead of inferring
	// them from NV12, P010, bit depth, or profile.
	struct Av1_Color_Info {
		std::uint8_t           bit_depth_              = 8;
		bool                   monochrome_             = false;
		bool                   subsampling_x_          = true;
		bool                   subsampling_y_          = true;
		Chroma_Sample_Position chroma_sample_position_ = Chroma_Sample_Position::UNKNOWN;
		Color_Range            range_                  = Color_Range::LIMITED;
		Matrix_Coefficients    matrix_                 = Matrix_Coefficients::BT709;
		Color_Primaries        primaries_              = Color_Primaries::BT709;
		Transfer_Function      transfer_               = Transfer_Function::BT1886;
		std::optional<Mastering_Display_Color_Volume> mastering_display_ = std::nullopt;
		std::optional<Content_Light_Level>             content_light_     = std::nullopt;
	};

	using Color_Encoding = Av1_Color_Info;

	struct Av1_Sequence_Info {
		Av1_Profile    profile_                    = Av1_Profile::PROFILE_0;
		std::uint32_t  max_width_                  = 0;
		std::uint32_t  max_height_                 = 0;
		std::uint8_t   operating_point_count_      = 0;
		bool           decoder_model_info_present_ = false;
		bool           film_grain_present_         = false;
		Av1_Color_Info color_;
	};

	struct Encoded_Av1_Bitstream {
		std::vector<std::uint8_t> bytes_;
	};

	enum class Unsupported_Reason : std::uint8_t {
		DRIVER,
		PROFILE,
		BIT_DEPTH,
		CHROMA_LAYOUT,
		DIMENSIONS,
		TOOL,
	};

	enum class Preflight_Status : std::uint8_t { SUPPORTED, UNSUPPORTED };

	struct Unsupported_Diagnostic {
		Unsupported_Reason reason_      = Unsupported_Reason::DRIVER;
		std::uint64_t      byte_offset_ = 0;
		std::string        message_;
	};

	struct Preflight_Report {
		Preflight_Status                      status_ = Preflight_Status::SUPPORTED;
		Av1_Sequence_Info                     sequence_;
		std::optional<Unsupported_Diagnostic> unsupported_;
	};

	struct Decode_Capabilities {
		bool          profile0_420_8bit_  = false;
		bool          profile0_420_10bit_ = false;
		std::uint32_t max_width_          = 0;
		std::uint32_t max_height_         = 0;
		std::uint32_t max_tile_count_     = 0;
		// Empty means the implementation does not impose a fixed active-source limit.
		std::optional<std::uint32_t> max_active_sources_per_context_;
	};

	struct Encode_Runtime_Capabilities {
		bool streaming_output_sink_   = false;
		bool async_source_submission_ = false;
		// Nonblocking CPU-side timeline counter poll for acquire plus host
		// timeline signal for release. This preserves the async producer model,
		// but it is not a native sync-file/timeline-fd bridge.
		bool host_mediated_timeline_acquire_release_ = false;
		// True only when acquire/release can be represented as native GPU/DRM
		// synchronization objects instead of host counter polling/signaling.
		bool native_timeline_acquire_release_ = false;
		// True when Begin_Encode_Request::native_sync_bridge_ can be used to
		// lower acquire/release timeline points through caller-provided
		// sync_file bridge operations.
		bool caller_provided_native_sync_bridge_ = false;
		// Generic "caller may use Vulkan timeline points" bit. Check
		// native_timeline_acquire_release_ before assuming fence/sync-file
		// ownership can cross API boundaries without host mediation.
		bool          gpu_timeline_acquire_release_ = false;
		std::uint32_t max_in_flight_source_frames_  = 1;
	};

	struct Encode_Tool_Capability_Snapshot {
		bool          use_128x128_superblock_       = false;
		bool          filter_intra_                 = false;
		bool          intra_edge_filter_            = false;
		bool          interintra_compound_          = false;
		bool          masked_compound_              = false;
		bool          warped_motion_                = false;
		bool          palette_mode_                 = false;
		bool          dual_filter_                  = false;
		bool          joint_compound_               = false;
		bool          ref_frame_mvs_                = false;
		bool          superres_                     = false;
		bool          restoration_                  = false;
		bool          intrabc_                      = false;
		bool          cdef_channel_strength_        = false;
		bool          segmentation_feature_map_     = false;
		std::uint8_t  interpolation_filter_mask_    = 0;
		std::uint8_t  segmentation_feature_mask_    = 0;
		std::uint16_t segmentation_block_size_mask_ = 0;
		std::uint8_t  tile_size_bytes_              = 0;
		std::uint8_t  obu_size_bytes_               = 0;
		std::uint8_t  tx_mode_support_              = 0;
	};

	struct Encode_Capability_Snapshot {
		bool                            available_                         = false;
		bool                            profile0_420_8bit_                 = false;
		bool                            profile0_420_10bit_                = false;
		bool                            driver_source_nv12_                = false;
		bool                            driver_source_p010_                = false;
		bool                            implemented_source_nv12_           = false;
		bool                            implemented_source_p010_           = false;
		std::uint32_t                   rate_control_mask_                 = 0;
		std::uint32_t                   packed_header_mask_                = 0;
		std::uint32_t                   max_width_                         = 0;
		std::uint32_t                   max_height_                        = 0;
		bool                            tile_support_                      = false;
		std::uint32_t                   driver_max_tile_count_             = 1;
		std::uint32_t                   max_tile_count_                    = 1;
		std::uint32_t                   max_tile_cols_                     = 1;
		std::uint32_t                   max_tile_rows_                     = 1;
		std::uint32_t                   max_reference_l0_                  = 0;
		std::uint32_t                   max_reference_l1_                  = 0;
		std::uint8_t                    quality_level_count_               = 0;
		bool                            max_frame_size_                    = false;
		bool                            max_frame_size_multi_pass_         = false;
		std::uint8_t                    max_roi_regions_                   = 0;
		bool                            roi_priority_                      = false;
		bool                            roi_qp_delta_                      = false;
		std::uint8_t                    max_temporal_layers_               = 0;
		bool                            temporal_layer_bitrate_control_    = false;
		bool                            processing_rate_encode_advertised_ = false;
		bool                            processing_rate_encode_            = false;
		std::uint32_t                   intra_refresh_mask_                = 0;
		bool                            skip_frame_                        = false;
		std::uint32_t                   max_dirty_rectangles_              = 0;
		std::uint32_t                   parallel_rate_control_layers_      = 0;
		bool                            frame_size_tolerance_              = false;
		bool                            custom_rounding_control_           = false;
		bool                            per_block_delta_qp_                = false;
		std::uint8_t                    delta_qp_log2_block_size_          = 0;
		std::uint8_t                    delta_qp_size_bytes_               = 0;
		bool                            prediction_direction_advertised_   = false;
		std::uint32_t                   prediction_direction_mask_         = 0;
		std::uint8_t                    max_concurrent_frames_             = 0;
		bool                            mixed_quality_level_               = false;
		std::uint16_t                   max_context_priority_              = 0;
		Encode_Tool_Capability_Snapshot tools_;
	};

	struct Source_Id {
		std::uint64_t value_                           = 0;
		friend bool   operator==(Source_Id, Source_Id) = default;
	};

	struct Frame_Id {
		std::uint64_t value_                         = 0;
		friend bool   operator==(Frame_Id, Frame_Id) = default;
	};

	struct Frame_Key {
		std::uint64_t coded_order_   = 0;
		std::uint64_t display_order_ = 0;
		std::uint64_t encode_order_  = 0;
	};

	enum class Av1_Frame_Type : std::uint8_t { KEY, INTER, INTRA_ONLY, SWITCH };

	inline constexpr std::uint32_t kInvalid_Av1_Frame_Index = std::numeric_limits<std::uint32_t>::max();

	struct Encoded_Byte_Interval {
		std::uint64_t begin_ = 0;
		std::uint64_t size_  = 0;
	};

	struct Av1_Frame_Decode_Metadata {
		Encoded_Byte_Interval        bitstream_;
		std::array<std::uint32_t, 8> reference_frame_index_{
				kInvalid_Av1_Frame_Index, kInvalid_Av1_Frame_Index, kInvalid_Av1_Frame_Index, kInvalid_Av1_Frame_Index,
				kInvalid_Av1_Frame_Index, kInvalid_Av1_Frame_Index, kInvalid_Av1_Frame_Index, kInvalid_Av1_Frame_Index};
		std::uint8_t   used_reference_slot_mask_    = 0;
		std::uint8_t   refresh_reference_slot_mask_ = 0;
		std::uint8_t   existing_reference_slot_     = 0;
		Av1_Frame_Type frame_type_                  = Av1_Frame_Type::INTER;
		bool           show_existing_frame_         = false;
	};

	struct Av1_Presentation_Record {
		std::uint32_t decoded_frame_index_ = 0;
	};

	class Av1_Bitstream_Index {
	public:
		Av1_Bitstream_Index() = default;
		Av1_Bitstream_Index(Source_Id source, Av1_Sequence_Info sequence,
							std::vector<Av1_Frame_Decode_Metadata> decode_frames,
							std::vector<Av1_Presentation_Record>   presentation)
			: source_(source), sequence_(std::move(sequence)), decode_frames_(std::move(decode_frames)),
			  presentation_(std::move(presentation)) {}

		[[nodiscard]] Source_Id                source() const noexcept { return source_; }
		[[nodiscard]] const Av1_Sequence_Info &sequence() const noexcept { return sequence_; }
		[[nodiscard]] std::size_t              decoded_picture_count() const noexcept { return decode_frames_.size(); }
		[[nodiscard]] std::size_t presentation_frame_count() const noexcept { return presentation_.size(); }
		[[nodiscard]] Frame_Id    decoded_picture_for_presentation(std::size_t presentation_frame) const noexcept {
			return Frame_Id{.value_ = presentation_[presentation_frame].decoded_frame_index_};
		}
		[[nodiscard]] const Av1_Frame_Decode_Metadata &unsafe_decode_metadata(Frame_Id frame) const noexcept {
			return decode_frames_[static_cast<std::size_t>(frame.value_)];
		}

	private:
		Source_Id                              source_;
		Av1_Sequence_Info                      sequence_;
		std::vector<Av1_Frame_Decode_Metadata> decode_frames_;
		std::vector<Av1_Presentation_Record>   presentation_;
	};

	struct Vulkan_Device_Context {
		VkInstance       instance_        = VK_NULL_HANDLE;
		VkPhysicalDevice physical_device_ = VK_NULL_HANDLE;
		VkDevice         device_          = VK_NULL_HANDLE;
		std::uint32_t    compute_queue_family_ = 0;
		VkQueue          compute_queue_        = VK_NULL_HANDLE;
		std::shared_ptr<std::mutex> compute_queue_mutex_;
		std::uint32_t    queue_family_    = 0;
		VkQueue          queue_           = VK_NULL_HANDLE;
		VkVideoCodecOperationFlagsKHR video_codec_operations_ = 0U;
		std::shared_ptr<std::mutex> queue_mutex_;
	};

	enum class Vulkan_Video_Decode_Codec : std::uint8_t { NONE, AV1, H264, H265, VP9 };

	// Stable value-only description used by frame allocation. Codec-specific
	// pNext structures are rebuilt locally when querying formats or creating an
	// image; no pointer-bearing Vulkan structure escapes its owning stack frame.
	struct Vulkan_Video_Decode_Profile {
		Vulkan_Video_Decode_Codec codec_ = Vulkan_Video_Decode_Codec::NONE;
		std::int32_t std_profile_idc_ = 0;
		VkVideoChromaSubsamplingFlagBitsKHR chroma_subsampling_ =
				VK_VIDEO_CHROMA_SUBSAMPLING_INVALID_KHR;
		VkVideoComponentBitDepthFlagBitsKHR luma_bit_depth_ =
				VK_VIDEO_COMPONENT_BIT_DEPTH_INVALID_KHR;
		VkVideoComponentBitDepthFlagBitsKHR chroma_bit_depth_ =
				VK_VIDEO_COMPONENT_BIT_DEPTH_INVALID_KHR;
		bool film_grain_support_ = false;
		VkVideoDecodeH264PictureLayoutFlagBitsKHR h264_picture_layout_ =
				VK_VIDEO_DECODE_H264_PICTURE_LAYOUT_PROGRESSIVE_KHR;
		friend bool operator==(const Vulkan_Video_Decode_Profile &,
				const Vulkan_Video_Decode_Profile &) = default;
	};

	struct Vulkan_Image_Plane {
		VkImage            image_            = VK_NULL_HANDLE;
		VkFormat           format_           = VK_FORMAT_UNDEFINED;
		VkImageAspectFlags aspect_           = 0;
		std::uint32_t      width_            = 0;
		std::uint32_t      height_           = 0;
		std::uint32_t      sample_stride_    = 0;
		std::uint32_t      drm_object_index_ = 0;
		std::uint64_t      drm_offset_       = 0;
		std::uint32_t      drm_pitch_        = 0;
	};

	struct Vulkan_Buffer_Plane {
		VkBuffer      buffer_           = VK_NULL_HANDLE;
		VkFormat      format_           = VK_FORMAT_UNDEFINED;
		VkDeviceSize  offset_           = 0;
		VkDeviceSize  size_             = 0;
		std::uint32_t width_            = 0;
		std::uint32_t height_           = 0;
		std::uint32_t sample_stride_    = 0;
		std::uint32_t drm_object_index_ = 0;
		std::uint64_t drm_offset_       = 0;
		std::uint32_t drm_pitch_        = 0;
	};

	struct Dma_Buf_Object {
		std::int32_t           fd_                  = -1;
		std::uint64_t size_                = 0;
		std::uint64_t drm_format_modifier_ = 0;
	};

	struct Vulkan_Timeline_Semaphore_Point {
		VkDevice      device_    = VK_NULL_HANDLE;
		VkSemaphore   semaphore_ = VK_NULL_HANDLE;
		std::uint64_t value_     = 0;
	};

	// Native decode synchronization is necessarily on the VkDevice borrowed by
	// the decode context.  The caller owns the semaphore and its lifetime; the
	// decoder may only put this handle/value pair in a queue submission.
	struct Vulkan_Semaphore_Point {
		VkSemaphore   semaphore_ = VK_NULL_HANDLE;
		std::uint64_t value_     = 0;
	};

	struct Exported_Dmabuf_Image {
		std::uint32_t                   width_      = 0;
		std::uint32_t                   height_     = 0;
		VkFormat                        format_     = VK_FORMAT_UNDEFINED;
		std::uint32_t                   drm_fourcc_ = 0;
		std::vector<Dma_Buf_Object>     memory_objects_;
		std::vector<Vulkan_Image_Plane> image_planes_;
		Vulkan_Timeline_Semaphore_Point vulkan_timeline_;
	};

	struct Drm_Syncobj_Timeline {
		std::int32_t           drm_fd_ = -1;
		std::uint32_t handle_ = 0;
	};

	struct Drm_Syncobj_Timeline_Point {
		Drm_Syncobj_Timeline timeline_;
		std::uint64_t        point_ = 0;
	};

	struct Vulkan_Decode_Target {
		Exported_Dmabuf_Image      image_;
		Drm_Syncobj_Timeline_Point safe_to_write_;
		Drm_Syncobj_Timeline_Point decode_complete_;
		Vulkan_Semaphore_Point vulkan_safe_to_write_;
		Vulkan_Semaphore_Point vulkan_decode_complete_;
	};

	struct Vulkan_Encode_Source {
		Exported_Dmabuf_Image           image_;
		Vulkan_Timeline_Semaphore_Point acquire_for_read_;
		Vulkan_Timeline_Semaphore_Point release_after_read_;
	};

	// One-shot Linux sync_file fd used only at external bridge boundaries.
	// Public editor/player synchronization should use timeline objects; sync
	// files appear here because Vulkan external semaphores and dma-buf
	// reservation-object ioctls use sync-file fds as their interop currency.
	struct Native_Sync_File {
		std::int32_t fd_ = -1;
	};

	class Encode_Native_Sync_Bridge {
	public:
		virtual ~Encode_Native_Sync_Bridge() = default;

		// Transfers ownership of a sync_file fd that becomes readable when
		// point is reached. The encoder imports it into the source dma-buf
		// because the VA/dma-buf boundary takes sync-file acquire fences.
		[[nodiscard]] virtual Result<Native_Sync_File>
		export_acquire_sync_file(Vulkan_Timeline_Semaphore_Point point) = 0;

		// Takes ownership of read_complete.fd_ and arranges for point to be
		// signaled after that sync_file signals. This is the caller-owned
		// Vulkan/DRM bridge; the VA submitter does not own a Vulkan queue or
		// a native Vulkan-to-DRM-syncobj export primitive.
		[[nodiscard]] virtual Result<void> signal_release_from_sync_file(Vulkan_Timeline_Semaphore_Point point,
																		 Native_Sync_File read_complete) = 0;
	};

	struct Vulkan_Frame_Transaction {
		Frame_Key            frame_;
		Vulkan_Decode_Target decode_target_;
		Vulkan_Encode_Source encode_source_;
	};

	struct Decode_Reference_Binding {
		std::uint8_t reference_slot_ = 0;
		Frame_Id     frame_;
	};

	struct Decode_Frame_Facts {
		Frame_Id              frame_;
		Encoded_Byte_Interval bitstream_;
		std::uint32_t         width_            = 0;
		std::uint32_t         height_           = 0;
		std::uint32_t         visible_x_        = 0;
		std::uint32_t         visible_y_        = 0;
		std::uint32_t         visible_width_    = 0;
		std::uint32_t         visible_height_   = 0;
		VkFormat              requested_format_ = VK_FORMAT_UNDEFINED;
		Vulkan_Video_Decode_Profile video_profile_;
		// Optional storage identity when multiple VA/DRM layouts share one
		// Vulkan format (for example P010 and P016).
		std::uint32_t                requested_drm_fourcc_ = 0;
		std::array<std::uint32_t, 8> reference_frame_index_{
				kInvalid_Av1_Frame_Index, kInvalid_Av1_Frame_Index, kInvalid_Av1_Frame_Index, kInvalid_Av1_Frame_Index,
				kInvalid_Av1_Frame_Index, kInvalid_Av1_Frame_Index, kInvalid_Av1_Frame_Index, kInvalid_Av1_Frame_Index};
		std::vector<Frame_Id>                 dependencies_;
		std::vector<Decode_Reference_Binding> reference_bindings_;
		std::uint8_t                          used_reference_slot_mask_    = 0;
		std::uint8_t                          refresh_reference_slot_mask_ = 0;
		// Hard lower bound for a cache scheduler to make progress without
		// overwriting active codec references. This is not a latency,
		// throughput, or memory-policy recommendation; the cache should choose
		// its own target pipeline depth above this bound.
		std::uint32_t  minimum_cache_frames_    = 1;
		std::uint8_t   existing_reference_slot_ = 0;
		Av1_Frame_Type frame_type_              = Av1_Frame_Type::INTER;
		bool           show_existing_frame_     = false;
	};

	struct Decode_Reference_Frame {
		std::uint8_t               reference_slot_ = 0;
		Frame_Id                   frame_;
		Exported_Dmabuf_Image      image_;
		Drm_Syncobj_Timeline_Point acquire_for_read_;
		Drm_Syncobj_Timeline_Point release_after_read_;
		Vulkan_Semaphore_Point vulkan_acquire_for_read_;
		Vulkan_Semaphore_Point vulkan_release_after_read_;
	};

	struct Decode_Frame_Request {
		Source_Id                               source_;
		Frame_Id                                frame_;
		std::span<const Decode_Reference_Frame> references_;
		Vulkan_Decode_Target                    output_;
	};

	struct Decode_Frame_Result {
		Frame_Id frame_;
	};

	// A logical elementary-stream interval backed directly by caller-owned mapped
	// storage. The owner must keep the mapping alive until release_source().
	struct Borrowed_Bitstream_Extent {
		std::uint64_t                 logical_begin_ = 0;
		std::span<const std::uint8_t> bytes_;
	};
	struct Compressed_Source_Storage {
		std::uint64_t owned_payload_bytes_ = 0;
		std::uint64_t borrowed_payload_bytes_ = 0;
		std::uint64_t borrowed_extent_count_ = 0;
	};

	enum class Rate_Control_Mode : std::uint8_t { CQP, ICQ, VBR, CBR, QVBR };
	enum class Encode_Interpolation_Filter : std::uint8_t {
		EIGHT_TAP,
		EIGHT_TAP_SMOOTH,
		EIGHT_TAP_SHARP,
		BILINEAR,
		SWITCHABLE,
	};
	enum class Encode_Transform_Mode : std::uint8_t { LARGEST, SELECT };
	enum class Encode_Reference_Mode : std::uint8_t { SINGLE, COMPOUND, SELECT };
	enum class Encode_Presentation : std::uint8_t { SHOW, HIDDEN_NOT_SHOWABLE, HIDDEN_SHOWABLE };
	enum class Encode_Restoration_Type : std::uint8_t { NONE, WIENER, SGRPROJ, SWITCHABLE };
	enum class Encode_Global_Motion_Type : std::uint8_t { IDENTITY, TRANSLATION, ROTZOOM, AFFINE };
	enum class Encode_Output_Backpressure : std::uint8_t { ACCEPTED, TRY_AGAIN_LATER };
	enum class Encode_Submission_Status : std::uint8_t { ACCEPTED };
	enum class Encode_Drain_Status : std::uint8_t { IDLE, MADE_PROGRESS, COMPLETE };

	struct Encode_Rate_Control_Common {
		std::uint32_t               max_frame_size_bits_ = 0;
		bool                        disable_frame_skip_  = true;
		std::optional<std::uint8_t> driver_quality_level_;
	};

	struct Encode_Qindex_Range {
		std::uint32_t min_qindex_ = 1;
		std::uint32_t max_qindex_ = 255;
	};

	struct Encode_Cqp_Rate_Control {
		Encode_Rate_Control_Common common_;
	};

	struct Encode_Icq_Rate_Control {
		std::uint32_t              quality_ = 128;
		Encode_Qindex_Range        qindex_range_;
		Encode_Rate_Control_Common common_;
	};

	struct Encode_Vbr_Rate_Control {
		std::uint32_t              target_kbps_       = 0;
		std::uint32_t              max_kbps_          = 0;
		std::uint32_t              target_percentage_ = 66;
		std::uint32_t              initial_qindex_    = 128;
		Encode_Qindex_Range        qindex_range_;
		std::uint32_t              cpb_size_kbits_ = 0;
		Encode_Rate_Control_Common common_;
	};

	struct Encode_Cbr_Rate_Control {
		std::uint32_t              target_kbps_    = 0;
		std::uint32_t              initial_qindex_ = 128;
		Encode_Qindex_Range        qindex_range_;
		std::uint32_t              cpb_size_kbits_ = 0;
		Encode_Rate_Control_Common common_;
	};

	struct Encode_Qvbr_Rate_Control {
		std::uint32_t              target_kbps_       = 0;
		std::uint32_t              max_kbps_          = 0;
		std::uint32_t              target_percentage_ = 66;
		std::uint32_t              quality_           = 128;
		Encode_Qindex_Range        qindex_range_;
		std::uint32_t              cpb_size_kbits_ = 0;
		Encode_Rate_Control_Common common_;
	};

	class Encode_Rate_Control {
	public:
		using Settings = std::variant<Encode_Cqp_Rate_Control, Encode_Icq_Rate_Control, Encode_Vbr_Rate_Control,
									  Encode_Cbr_Rate_Control, Encode_Qvbr_Rate_Control>;

		Encode_Rate_Control() = default;

		[[nodiscard]] static Encode_Rate_Control cqp(Encode_Cqp_Rate_Control control = {}) {
			return Encode_Rate_Control{control};
		}
		[[nodiscard]] static Encode_Rate_Control icq(Encode_Icq_Rate_Control control = {}) {
			return Encode_Rate_Control{control};
		}
		[[nodiscard]] static Encode_Rate_Control vbr(Encode_Vbr_Rate_Control control) {
			return Encode_Rate_Control{control};
		}
		[[nodiscard]] static Encode_Rate_Control cbr(Encode_Cbr_Rate_Control control) {
			return Encode_Rate_Control{control};
		}
		[[nodiscard]] static Encode_Rate_Control qvbr(Encode_Qvbr_Rate_Control control) {
			return Encode_Rate_Control{control};
		}

		[[nodiscard]] const Settings   &settings() const { return settings_; }
		[[nodiscard]] Rate_Control_Mode mode() const {
			return std::visit(
					[](const auto &control) {
						using Control = std::decay_t<decltype(control)>;
						if constexpr (std::is_same_v<Control, Encode_Cqp_Rate_Control>) {
							return Rate_Control_Mode::CQP;
						} else if constexpr (std::is_same_v<Control, Encode_Icq_Rate_Control>) {
							return Rate_Control_Mode::ICQ;
						} else if constexpr (std::is_same_v<Control, Encode_Vbr_Rate_Control>) {
							return Rate_Control_Mode::VBR;
						} else if constexpr (std::is_same_v<Control, Encode_Cbr_Rate_Control>) {
							return Rate_Control_Mode::CBR;
						} else {
							return Rate_Control_Mode::QVBR;
						}
					},
					settings_);
		}
		[[nodiscard]] std::optional<std::uint32_t> driver_initial_qindex_or_quality() const {
			return std::visit(
					[](const auto &control) -> std::optional<std::uint32_t> {
						using Control = std::decay_t<decltype(control)>;
						if constexpr (std::is_same_v<Control, Encode_Cqp_Rate_Control>) {
							return std::nullopt;
						} else if constexpr (std::is_same_v<Control, Encode_Vbr_Rate_Control> ||
											 std::is_same_v<Control, Encode_Cbr_Rate_Control>) {
							return control.initial_qindex_;
						} else {
							return control.quality_;
						}
					},
					settings_);
		}
		[[nodiscard]] Encode_Qindex_Range qindex_range() const {
			return std::visit(
					[](const auto &control) -> Encode_Qindex_Range {
						using Control = std::decay_t<decltype(control)>;
						if constexpr (std::is_same_v<Control, Encode_Cqp_Rate_Control>) {
							return {};
						} else {
							return control.qindex_range_;
						}
					},
					settings_);
		}
		[[nodiscard]] std::uint32_t min_qindex() const { return qindex_range().min_qindex_; }
		[[nodiscard]] std::uint32_t max_qindex() const { return qindex_range().max_qindex_; }
		[[nodiscard]] std::uint32_t target_kbps() const {
			return std::visit(
					[](const auto &control) -> std::uint32_t {
						using Control = std::decay_t<decltype(control)>;
						if constexpr (std::is_same_v<Control, Encode_Vbr_Rate_Control> ||
									  std::is_same_v<Control, Encode_Cbr_Rate_Control> ||
									  std::is_same_v<Control, Encode_Qvbr_Rate_Control>) {
							return control.target_kbps_;
						} else {
							return 0U;
						}
					},
					settings_);
		}
		[[nodiscard]] std::uint32_t max_kbps() const {
			return std::visit(
					[](const auto &control) -> std::uint32_t {
						using Control = std::decay_t<decltype(control)>;
						if constexpr (std::is_same_v<Control, Encode_Vbr_Rate_Control> ||
									  std::is_same_v<Control, Encode_Qvbr_Rate_Control>) {
							return control.max_kbps_;
						} else if constexpr (std::is_same_v<Control, Encode_Cbr_Rate_Control>) {
							return control.target_kbps_;
						} else {
							return 0U;
						}
					},
					settings_);
		}
		[[nodiscard]] std::uint32_t target_percentage() const {
			return std::visit(
					[](const auto &control) -> std::uint32_t {
						using Control = std::decay_t<decltype(control)>;
						if constexpr (std::is_same_v<Control, Encode_Vbr_Rate_Control> ||
									  std::is_same_v<Control, Encode_Qvbr_Rate_Control>) {
							return control.target_percentage_;
						} else {
							return 100U;
						}
					},
					settings_);
		}
		[[nodiscard]] std::uint32_t cpb_size_kbits() const {
			return std::visit(
					[](const auto &control) -> std::uint32_t {
						using Control = std::decay_t<decltype(control)>;
						if constexpr (std::is_same_v<Control, Encode_Cqp_Rate_Control> ||
									  std::is_same_v<Control, Encode_Icq_Rate_Control>) {
							return 0U;
						} else {
							return control.cpb_size_kbits_;
						}
					},
					settings_);
		}
		[[nodiscard]] const Encode_Rate_Control_Common &common() const {
			return std::visit([](const auto &control) -> const Encode_Rate_Control_Common & { return control.common_; },
							  settings_);
		}
		[[nodiscard]] std::uint32_t               max_frame_size_bits() const { return common().max_frame_size_bits_; }
		[[nodiscard]] bool                        disable_frame_skip() const { return common().disable_frame_skip_; }
		[[nodiscard]] std::optional<std::uint8_t> driver_quality_level() const {
			return common().driver_quality_level_;
		}

	private:
		explicit Encode_Rate_Control(Settings settings) : settings_{std::move(settings)} {}

		Settings settings_{Encode_Cqp_Rate_Control{}};
	};

	struct Encode_Tile_Policy {
		std::uint32_t columns_                = 1;
		std::uint32_t rows_                   = 1;
		std::uint32_t tile_groups_            = 1;
		std::uint16_t context_update_tile_id_ = 0;
	};

	struct Encode_Cdef_Parameters {
		std::uint8_t                damping_minus_3_ = 0;
		std::uint8_t                bits_            = 0;
		std::array<std::uint8_t, 8> y_strengths_{};
		std::array<std::uint8_t, 8> uv_strengths_{};
	};

	struct Encode_Loop_Filter_Parameters {
		std::array<std::uint8_t, 2> filter_level_{};
		std::uint8_t                filter_level_u_         = 0;
		std::uint8_t                filter_level_v_         = 0;
		std::uint8_t                sharpness_level_        = 0;
		bool                        mode_ref_delta_enabled_ = false;
		bool                        mode_ref_delta_update_  = false;
		std::array<std::int8_t, 8>  ref_deltas_{};
		std::array<std::int8_t, 2>  mode_deltas_{};
	};

	struct Encode_Quantization_Parameters {
		std::int8_t  y_dc_delta_q_ = 0;
		std::int8_t  u_dc_delta_q_ = 0;
		std::int8_t  u_ac_delta_q_ = 0;
		std::int8_t  v_dc_delta_q_ = 0;
		std::int8_t  v_ac_delta_q_ = 0;
		bool         use_qmatrix_  = false;
		std::uint8_t qm_y_         = 0;
		std::uint8_t qm_u_         = 0;
		std::uint8_t qm_v_         = 0;
	};

	struct Encode_Block_Delta_Policy {
		bool         delta_q_present_   = false;
		std::uint8_t delta_q_res_log2_  = 0;
		bool         delta_lf_present_  = false;
		std::uint8_t delta_lf_res_log2_ = 0;
		bool         delta_lf_multi_    = false;
	};

	struct Encode_Restoration_Parameters {
		Encode_Restoration_Type y_             = Encode_Restoration_Type::NONE;
		Encode_Restoration_Type cb_            = Encode_Restoration_Type::NONE;
		Encode_Restoration_Type cr_            = Encode_Restoration_Type::NONE;
		std::uint8_t            lr_unit_shift_ = 0;
		std::uint8_t            lr_uv_shift_   = 0;
	};

	struct Encode_Superres_Parameters {
		std::uint8_t denominator_ = 8;
	};

	struct Encode_Global_Motion_Parameters {
		Encode_Global_Motion_Type   type_ = Encode_Global_Motion_Type::IDENTITY;
		std::array<std::int32_t, 8> matrix_{};
		bool                        invalid_ = false;
	};

	struct Encode_Tool_Policy {
		bool                                           use_128x128_superblock_     = false;
		bool                                           enable_filter_intra_        = false;
		bool                                           enable_intra_edge_filter_   = false;
		bool                                           enable_interintra_compound_ = false;
		bool                                           enable_masked_compound_     = false;
		bool                                           enable_warped_motion_       = false;
		bool                                           allow_warped_motion_        = false;
		bool                                           enable_dual_filter_         = false;
		bool                                           enable_joint_compound_      = false;
		bool                                           enable_ref_frame_mvs_       = false;
		bool                                           enable_superres_            = false;
		std::optional<Encode_Superres_Parameters>      superres_parameters_;
		bool                                           enable_cdef_ = true;
		std::optional<Encode_Cdef_Parameters>          cdef_parameters_;
		bool                                           enable_restoration_ = true;
		std::optional<Encode_Restoration_Parameters>   restoration_parameters_;
		Encode_Interpolation_Filter                    interpolation_filter_ = Encode_Interpolation_Filter::EIGHT_TAP;
		Encode_Transform_Mode                          transform_mode_       = Encode_Transform_Mode::SELECT;
		std::optional<Encode_Loop_Filter_Parameters>   loop_filter_parameters_;
		std::optional<Encode_Quantization_Parameters>  quantization_parameters_;
		Encode_Block_Delta_Policy                      block_delta_;
		std::array<Encode_Global_Motion_Parameters, 7> global_motion_{};
		bool                                           allow_screen_content_tools_          = false;
		bool                                           force_integer_mv_                    = false;
		bool                                           allow_high_precision_mv_             = false;
		bool                                           disable_cdf_update_                  = false;
		bool                                           disable_frame_end_update_cdf_        = false;
		bool                                           reduced_tx_set_                      = false;
		bool                                           allow_intrabc_                       = false;
		bool                                           palette_mode_                        = false;
		bool                                           use_raw_source_for_motion_reference_ = false;
	};

	struct Encode_Bitstream_Packaging_Policy {
		bool use_frame_obu_ = false;
	};

	struct Encode_Temporal_Layer_Rate_Control {
		std::uint32_t target_kbps_   = 0;
		std::uint32_t max_kbps_      = 0;
		std::uint32_t framerate_num_ = 0;
		std::uint32_t framerate_den_ = 1;
	};

	struct Encode_Temporal_Layer_Policy {
		std::uint8_t                                      layer_count_ = 0;
		std::uint32_t                                     periodicity_ = 0;
		std::array<std::uint8_t, 32>                      layer_id_{};
		std::array<Encode_Temporal_Layer_Rate_Control, 8> rate_control_{};
	};

	struct Encode_Determinism_Policy {
		bool fail_on_silent_degrade_ = false;
		bool exact_frame_count_      = true;
	};

	struct Encode_Execution_Policy {
		bool          require_gpu_timeline_synchronization_ = false;
		std::uint32_t max_in_flight_source_frames_          = 1;
	};

	struct Segmentation_Map {
		Vulkan_Buffer_Plane plane_;
		std::uint32_t       segment_count_ = 0;
	};

	enum class Encode_Segment_Id_Block_Size : std::uint8_t {
		BLOCK_16X16 = 0,
		BLOCK_32X32 = 1,
		BLOCK_64X64 = 2,
		BLOCK_8X8   = 3,
	};

	struct Encode_Segmentation_Policy {
		bool                                       enabled_               = false;
		bool                                       app_provides_map_      = false;
		bool                                       temporal_update_       = false;
		std::uint32_t                              segment_count_         = 0;
		Encode_Segment_Id_Block_Size               segment_id_block_size_ = Encode_Segment_Id_Block_Size::BLOCK_16X16;
		std::array<std::uint8_t, 8>                feature_mask_{};
		std::array<std::array<std::int16_t, 8>, 8> feature_data_{};
		std::optional<Segmentation_Map>            segment_map_;
	};

	enum class Encode_Roi_Value_Mode : std::uint8_t { QP_DELTA, PRIORITY };

	struct Encode_Roi_Policy {
		bool                  enabled_      = false;
		Encode_Roi_Value_Mode value_mode_   = Encode_Roi_Value_Mode::QP_DELTA;
		std::int8_t           min_delta_qp_ = -31;
		std::int8_t           max_delta_qp_ = 31;
	};

	struct Av1_Encode_Policy {
		Av1_Color_Info                    output_color_;
		std::uint8_t                      seq_profile_   = 0;
		std::uint8_t                      seq_level_idx_ = 0;
		bool                              seq_tier_high_ = false;
		bool                              still_picture_ = false;
		Encode_Rate_Control               rate_control_;
		Encode_Tile_Policy                tiles_;
		Encode_Tool_Policy                tools_;
		Encode_Bitstream_Packaging_Policy packaging_;
		Encode_Temporal_Layer_Policy      temporal_layers_;
		Encode_Segmentation_Policy        segmentation_;
		Encode_Roi_Policy                 roi_;
		Encode_Determinism_Policy         determinism_;
		Encode_Execution_Policy           execution_;
	};

	enum class Unsupported_Feature : std::uint8_t {
		ENCODE_10BIT,
		CHROMA_LAYOUT,
		RATE_CONTROL_MODE,
		TILE_COUNT,
		CDEF,
		RESTORATION,
		AV1_TOOL,
		SCREEN_CONTENT_TOOLS,
		SEGMENTATION_MAP,
		REFERENCE_STRUCTURE,
		TEMPORAL_LAYERS,
		DRIVER_QUALITY_LEVEL,
		SYNCHRONIZATION,
	};

	struct Lowering_Diagnostic {
		Unsupported_Feature feature_ = Unsupported_Feature::RATE_CONTROL_MODE;
		std::string         message_;
	};

	struct Encode_Lowering_Report {
		bool                             exact_ = false;
		Av1_Encode_Policy                effective_policy_;
		std::vector<Lowering_Diagnostic> unsupported_;
	};

	struct Encode_Processing_Rate {
		bool          available_                    = false;
		std::uint32_t processing_rate_blocks_per_s_ = 0;
		std::string   diagnostic_;
	};

	struct Encode_Report {
		std::uint64_t encoded_frames_                  = 0;
		bool          exact_policy_                    = false;
		std::uint32_t max_simultaneous_va_submissions_ = 0;
	};

	struct Encoded_Output {
		Encoded_Av1_Bitstream bitstream_;
		Encode_Report         report_;
	};

	struct Encode_Frame_Constraints {
		Frame_Id             frame_;
		std::uint64_t        display_order_        = 0;
		bool                 force_keyframe_       = false;
		bool                 independent_boundary_ = false;
		std::optional<float> intra_score_;
		std::optional<float> complexity_score_;
	};

	struct Encode_Roi_Region {
		std::uint32_t x_      = 0;
		std::uint32_t y_      = 0;
		std::uint32_t width_  = 0;
		std::uint32_t height_ = 0;
		std::int8_t   value_  = 0;
	};

	struct Encode_Source_Submission {
		Frame_Id                           frame_;
		std::uint64_t                      display_order_ = 0;
		Vulkan_Encode_Source               source_;
		std::span<const std::uint8_t>      segmentation_map_;
		std::span<const Encode_Roi_Region> roi_regions_;
	};

	struct Encoded_Temporal_Unit {
		Frame_Id                      frame_;
		// True only when this temporal unit is independently decodable. Container
		// sinks use this semantic fact to build their random-access sample tables.
		bool                          random_access_ = false;
		std::span<const std::uint8_t> bytes_;
	};

	class Encoded_Output_Sink {
	public:
		virtual ~Encoded_Output_Sink() = default;
		[[nodiscard]] virtual Result<Encode_Output_Backpressure>
		write_temporal_unit(Encoded_Temporal_Unit temporal_unit) = 0;
	};

	struct Encode_Stream_Info {
		std::uint32_t                width_         = 0;
		std::uint32_t                height_        = 0;
		std::uint32_t                framerate_num_ = 0;
		std::uint32_t                framerate_den_ = 1;
		std::optional<std::uint64_t> frame_count_;
		VkFormat                     source_format_     = VK_FORMAT_UNDEFINED;
		std::uint32_t                source_drm_fourcc_ = 0;
	};

	struct Encode_Va_Sequence_Parameters {
		std::uint8_t  order_hint_bits_;
		std::uint32_t intra_period_;
		std::uint32_t ip_period_;
		bool          hierarchical_;
		std::uint32_t reconstructed_reference_slots_;
	};

	struct Encode_Processing_Rate_Query {
		Av1_Encode_Policy             policy_;
		Encode_Stream_Info            stream_;
		Encode_Va_Sequence_Parameters va_sequence_;
	};

	struct Begin_Encode_Request {
		Av1_Encode_Policy             policy_;
		Encode_Stream_Info            stream_;
		Encode_Va_Sequence_Parameters va_sequence_;
		Encoded_Output_Sink          *output_sink_        = nullptr;
		Encode_Native_Sync_Bridge    *native_sync_bridge_ = nullptr;
	};

	struct Encode_Cqp_Picture_Rate_Control {
		std::uint8_t base_qindex_;
	};

	struct Encode_Driver_Picture_Rate_Control {};

	class Encode_Picture_Rate_Control {
	public:
		using Settings = std::variant<Encode_Cqp_Picture_Rate_Control, Encode_Driver_Picture_Rate_Control>;

		Encode_Picture_Rate_Control() = delete;

		[[nodiscard]] static Encode_Picture_Rate_Control cqp(Encode_Cqp_Picture_Rate_Control control) {
			return Encode_Picture_Rate_Control{control};
		}
		[[nodiscard]] static Encode_Picture_Rate_Control driver() {
			return Encode_Picture_Rate_Control{Encode_Driver_Picture_Rate_Control{}};
		}

		[[nodiscard]] const Settings &settings() const { return settings_; }
		[[nodiscard]] bool is_cqp() const { return std::holds_alternative<Encode_Cqp_Picture_Rate_Control>(settings_); }
		[[nodiscard]] std::uint8_t cqp_base_qindex() const {
			return std::get<Encode_Cqp_Picture_Rate_Control>(settings_).base_qindex_;
		}

	private:
		explicit Encode_Picture_Rate_Control(Settings settings) : settings_{std::move(settings)} {}

		Settings settings_;
	};

	struct Encode_Picture {
		// Concrete AV1 picture and VA submission fields supplied by the caller.
		Encode_Picture(Encode_Picture_Rate_Control rate_control, Encode_Reference_Mode reference_mode)
			: reference_mode_{reference_mode}, rate_control_{std::move(rate_control)} {}

		Frame_Id                    output_frame_;
		std::uint64_t               source_index_         = 0;
		std::uint64_t               source_display_order_ = 0;
		std::uint64_t               encode_order_         = 0;
		Av1_Frame_Type              frame_type_           = Av1_Frame_Type::INTER;
		std::uint8_t                order_hint_           = 0;
		std::uint8_t                primary_ref_frame_    = 7;
		bool                        error_resilient_mode_ = false;
		Encode_Presentation         presentation_;
		// Optional complete AV1 HDR10+ metadata OBU for this displayed picture.
		// Owned here because VA submission and output assembly are asynchronous.
		std::vector<std::uint8_t>   hdr10plus_metadata_obu_;
		std::uint8_t                refresh_reference_slot_mask_ = 0;
		std::uint8_t                used_reference_slot_mask_    = 0;
		std::array<std::uint8_t, 8> reference_slot_order_hint_{};
		std::array<std::uint8_t, 7> named_reference_slot_{0xffU, 0xffU, 0xffU, 0xffU, 0xffU, 0xffU, 0xffU};
		std::array<std::uint8_t, 7> l0_reference_order_{};
		std::array<std::uint8_t, 7> l1_reference_order_{};
		std::uint8_t                l0_reference_count_ = 0;
		std::uint8_t                l1_reference_count_ = 0;
		Encode_Reference_Mode       reference_mode_;
		std::uint8_t                hierarchical_level_plus1_ = 0;
		bool                        long_term_reference_      = false;
		bool                        skip_mode_present_        = false;
		std::uint8_t                number_skip_frames_       = 0;
		std::int32_t                skip_frames_reduced_size_ = 0;
		Encode_Picture_Rate_Control rate_control_;
	};

	struct Encode_Show_Existing {
		Frame_Id      output_frame_;
		std::uint64_t output_display_order_    = 0;
		std::uint8_t  existing_reference_slot_ = 0;
		std::vector<std::uint8_t> hdr10plus_metadata_obu_;
	};

	class Async_Encode_Session : public Encode_Session {
	public:
		~Async_Encode_Session() override = default;
		[[nodiscard]] virtual Result<Encode_Submission_Status> submit_picture(Encode_Source_Submission submission,
																			  Encode_Picture           picture)   = 0;
		[[nodiscard]] virtual Result<Encode_Submission_Status> submit_show_existing(Encode_Show_Existing picture) = 0;
		[[nodiscard]] virtual Result<Encode_Drain_Status>      drain_ready_output()                               = 0;
		[[nodiscard]] virtual Result<Encode_Report>            finish()                                           = 0;
	};

	class Av1_Editor_Bitstream_Api {
	public:
		virtual ~Av1_Editor_Bitstream_Api() = default;

		[[nodiscard]] virtual Decode_Capabilities         decode_capabilities() const         = 0;
		[[nodiscard]] virtual Encode_Runtime_Capabilities encode_runtime_capabilities() const = 0;
		[[nodiscard]] virtual Encode_Capability_Snapshot  encode_capabilities() const         = 0;
		[[nodiscard]] virtual encode::Encode_Plan_Restrictions encode_plan_restrictions() const = 0;
		[[nodiscard]] virtual Result<std::unique_ptr<Av1_Decode_Context>>
												  create_decode_context(const Av1_Sequence_Info &sequence)  = 0;
		[[nodiscard]] virtual Result<std::unique_ptr<Av1_Decode_Context>>
		create_vulkan_decode_context(const Av1_Sequence_Info &sequence, Vulkan_Device_Context device) = 0;
		[[nodiscard]] virtual Result<Preflight_Report>    preflight_decode(std::span<const std::uint8_t> bitstream) = 0;
		[[nodiscard]] virtual Result<Av1_Bitstream_Index> index_bitstream(std::span<const std::uint8_t> bitstream)  = 0;
		[[nodiscard]] virtual Result<Av1_Bitstream_Index>
		index_borrowed_bitstream(std::span<const Borrowed_Bitstream_Extent> extents) = 0;
		[[nodiscard]] virtual Result<void> bind_borrowed_bitstream(
				Source_Id source, std::span<const Borrowed_Bitstream_Extent> extents) = 0;
		[[nodiscard]] virtual Result<Compressed_Source_Storage> compressed_source_storage(Source_Id source) const = 0;
		[[nodiscard]] virtual Result<void>                release_source(Source_Id source)                          = 0;
		[[nodiscard]] virtual Result<Decode_Frame_Facts>  query_decode_frame(Source_Id source, Frame_Id frame)      = 0;
		[[nodiscard]] virtual Result<Decode_Frame_Result> decode_frame(Av1_Decode_Context  &context,
																	   Decode_Frame_Request request)                = 0;
		[[nodiscard]] virtual Result<Encode_Lowering_Report> lower_encode_policy(Av1_Encode_Policy policy)          = 0;
		[[nodiscard]] virtual Result<Encode_Processing_Rate>
		query_encode_processing_rate(Encode_Processing_Rate_Query query) = 0;
		[[nodiscard]] virtual Result<std::unique_ptr<Async_Encode_Session>>
		begin_encode(Begin_Encode_Request request) = 0;
	};

	[[nodiscard]] std::unique_ptr<Av1_Editor_Bitstream_Api> open_av1_editor_bitstream_api();
} // namespace video_editor::av1
