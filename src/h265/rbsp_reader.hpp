#pragma once

#include "h265/syntax_parser.hpp"
#include "video_editor/core/try_result.hpp"

#include <algorithm>
#include <limits>

namespace video_editor::h265::decoder {
	// Read EBSP in place. Physical positions include emulation-prevention bytes,
	// while syntax lengths measure the RBSP after those bytes are removed.
	class Rbsp_Reader {
	public:
		explicit Rbsp_Reader(std::span<const std::uint8_t> bytes) : bytes_(bytes) {}
		[[nodiscard]] std::uint64_t position() const { return byte_ * 8 - remaining_; }
		[[nodiscard]] std::uint64_t syntax_position() const { return position() - escapes_ * 8; }
		[[nodiscard]] std::uint64_t escapes() const { return escapes_; }

		[[nodiscard]] std::unexpected<Syntax_Error> fail(const char *detail, bool unsupported = false) const {
			return std::unexpected(Syntax_Error{position(), detail, unsupported});
		}

		[[nodiscard]] Syntax_Result<std::uint32_t> bits(unsigned count) {
			std::uint32_t value = 0;
			while (count != 0) {
				if (remaining_ == 0) {
					if (byte_ == bytes_.size()) {
						return fail("truncated RBSP");
					}
					current_ = bytes_[byte_++];
					if (zero_run_ == 2 && current_ == 3) {
						if (byte_ == bytes_.size() || bytes_[byte_] > 3) {
							return fail("invalid emulation-prevention byte");
						}
						++escapes_;
						zero_run_ = 0;
						current_  = bytes_[byte_++];
					}
					zero_run_  = current_ == 0 ? std::min(zero_run_ + 1, 2U) : 0;
					remaining_ = 8;
				}
				const auto take = std::min(count, remaining_);
				remaining_ -= take;
				value = (value << take) | ((current_ >> remaining_) & ((1U << take) - 1U));
				count -= take;
			}
			return value;
		}

		[[nodiscard]] Syntax_Result<void> skip(std::uint64_t count) {
			while (count > 32) {
				VIDEO_EDITOR_TRY(bits(32));
				count -= 32;
			}
			VIDEO_EDITOR_TRY(bits(static_cast<unsigned>(count)));
			return {};
		}

		[[nodiscard]] Syntax_Result<std::uint32_t> ue(std::uint32_t maximum = UINT32_MAX) {
			unsigned zeros = 0;
			while (!VIDEO_EDITOR_TRY(bits(1))) {
				if (++zeros > 32) {
					return fail("unsigned Exp-Golomb value exceeds 32 bits");
				}
			}
			const auto suffix = VIDEO_EDITOR_TRY(bits(zeros));
			const auto value  = ((std::uint64_t{1} << zeros) - 1) + suffix;
			if (value > maximum) {
				return fail("unsigned Exp-Golomb value is out of range");
			}
			return static_cast<std::uint32_t>(value);
		}

		[[nodiscard]] Syntax_Result<std::int32_t> se(std::int32_t minimum, std::int32_t maximum) {
			const auto code = VIDEO_EDITOR_TRY(ue(UINT32_MAX - 1));
			const auto value =
					(code & 1U) ? static_cast<std::int32_t>((code + 1) / 2) : -static_cast<std::int32_t>(code / 2);
			if (value < minimum || value > maximum) {
				return fail("signed Exp-Golomb value is out of range");
			}
			return value;
		}

		[[nodiscard]] Syntax_Result<void> align() {
			if (VIDEO_EDITOR_TRY(bits(1)) != 1 || VIDEO_EDITOR_TRY(bits(remaining_)) != 0) {
				return fail("invalid RBSP alignment");
			}
			return {};
		}

	private:
		std::span<const std::uint8_t> bytes_;
		std::uint64_t                 byte_ = 0, escapes_ = 0;
		unsigned                      remaining_ = 0, zero_run_ = 0;
		std::uint8_t                  current_ = 0;
	};
} // namespace video_editor::h265::decoder
