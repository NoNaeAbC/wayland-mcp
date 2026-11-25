#pragma once

#include "h265/frame_vaapi.hpp"

namespace video_editor::h265::decoder {
	struct Parsed_Decode_Frame;
}

namespace video_editor::h265::vulkan_video {
	struct Encode_Rate {
		std::uint32_t _frame_rate;
		std::uint32_t _target_bitrate;
		bool _automatic_bitrate = false;
	};

	[[nodiscard]] std::uint32_t encode_formats(const char *device, std::span<const Frame_Format> formats, bool vbr);
	[[nodiscard]] std::uint32_t decode_formats(const char *device, std::span<const Frame_Format> formats);
	[[nodiscard]] Result<std::unique_ptr<H265_Encode_Context>>
	create_encoder(const char *device, Frame_Layout layout, Encode_Rate rate);

	// The caller validates the full bitstream and verifies the returned reconstruction.
	[[nodiscard]] Result<std::vector<std::uint32_t>>
	decode_idr(const char *device, Frame_Layout layout, const decoder::Parsed_Decode_Frame &picture,
			   std::span<const std::uint8_t> bitstream, bool alpha = false);
}
