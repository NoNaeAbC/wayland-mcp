#pragma once

#include <cstdint>
#include <cstring>
#include <optional>
#include <span>

namespace video_editor::media {
struct Annex_B_Start_Code {
    std::uint64_t begin_;
    std::uint64_t payload_;
};

// Positions always belong to the complete borrowed bitstream. This searches
// syntax boundaries in the original mapping; it does not split input buffers.
[[nodiscard]] inline std::optional<Annex_B_Start_Code> find_annex_b_start_code(
        std::span<const std::uint8_t> bytes, std::uint64_t from = 0) {
	if (from > bytes.size() || bytes.size() - from < 3) {
		return std::nullopt;
	}
	auto cursor = from + 2;
    while (cursor < bytes.size()) {
        const auto *one = static_cast<const std::uint8_t *>(
                std::memchr(bytes.data() + cursor, 1, bytes.size() - cursor));
		if (!one) {
			return std::nullopt;
		}
		const auto at = static_cast<std::uint64_t>(one - bytes.data());
        if (bytes[at - 1] == 0 && bytes[at - 2] == 0) {
            auto begin = at - 2;
			if (begin > from && bytes[begin - 1] == 0) {
				--begin;
			}
			return Annex_B_Start_Code{begin, at + 1};
        }
        cursor = at + 1;
    }
	return std::nullopt;
}
} // namespace video_editor::media
