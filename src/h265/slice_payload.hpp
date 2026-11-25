#pragma once
#include "h265/decoder_internal.hpp"
#include <limits>

namespace video_editor::h265::decoder {
// Preserve the source's 64-bit coordinate system up to the actual VA payload
// boundary. Locate the borrowed NAL using its logical file position.
[[nodiscard]] inline Result<std::span<const std::uint8_t>> va_slice_payload(
        const Parsed_Decode_Slice &slice, std::span<const std::uint8_t> source,
        std::uint64_t source_begin) {
    const auto fail = [&](Media_Error_Code code, const char *message) {
        return std::unexpected(Media_Error{.code_ = code, .byte_offset_ = slice.logical_offset_, .message_ = message});
    };
    if (slice.logical_offset_ < source_begin || slice.logical_offset_ - source_begin > source.size()) {
		return fail(Media_Error_Code::INVALID_H265_BITSTREAM, "H.265 slice starts outside its mapped source");
	}
	const auto begin = slice.logical_offset_ - source_begin;
    if (slice.nalu_.bytes.size() > source.size() - begin) {
		return fail(Media_Error_Code::INVALID_H265_BITSTREAM, "H.265 slice payload extends beyond its mapped source");
	}
	std::uint64_t end = begin + slice.nalu_.bytes.size();
    while (source.size() - end >= 3 && source[end] == 0) {
		if (source[end + 1] == 0 && source[end + 2] == 1) {
			break;
		}
		++end;
    }
    if (end - begin > std::numeric_limits<std::uint32_t>::max()) {
		return fail(Media_Error_Code::UNSUPPORTED_DRIVER_FEATURE,
					"A single H.265 slice exceeds VA's 32-bit slice-data size field");
	}
	return source.subspan(begin, end - begin);
}
} // namespace video_editor::h265::decoder
