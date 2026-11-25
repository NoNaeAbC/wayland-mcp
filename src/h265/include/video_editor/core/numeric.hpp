#pragma once

#include <bit>
#include <array>
#include <concepts>
#include <cstdint>
#include <limits>

namespace video_editor::core {
	enum class Floating_Class { FINITE, INFINITE, NAN_VALUE };
	struct Floating_Representation {
		Floating_Class classification;
		bool negative;
	};

	// Inspect storage bits, not floating-point comparisons or compiler builtins.
	// This validates external values even when finite-math assumptions are active.
	template<std::floating_point T>
	[[nodiscard]] constexpr Floating_Representation inspect_floating_point(T value) noexcept {
		static_assert(std::numeric_limits<T>::radix == 2);
		if constexpr (sizeof(T) == 4 && std::numeric_limits<T>::digits == 24 && std::numeric_limits<T>::max_exponent == 128) {
			const auto bits = std::bit_cast<std::uint32_t>(value);
			const auto magnitude = bits & 0x7fffffffU;
			return {magnitude < 0x7f800000U ? Floating_Class::FINITE :
				magnitude == 0x7f800000U ? Floating_Class::INFINITE : Floating_Class::NAN_VALUE,
				(bits >> 31) != 0};
		} else if constexpr (sizeof(T) == 8 && std::numeric_limits<T>::digits == 53 && std::numeric_limits<T>::max_exponent == 1024) {
			const auto bits = std::bit_cast<std::uint64_t>(value);
			const auto magnitude = bits & 0x7fffffffffffffffULL;
			return {magnitude < 0x7ff0000000000000ULL ? Floating_Class::FINITE :
				magnitude == 0x7ff0000000000000ULL ? Floating_Class::INFINITE : Floating_Class::NAN_VALUE,
				(bits >> 63) != 0};
		} else if constexpr (std::numeric_limits<T>::digits == 64 && std::numeric_limits<T>::max_exponent == 16384 &&
				std::endian::native == std::endian::little && sizeof(T) >= 10) {
			const auto bits = std::bit_cast<std::array<unsigned char, sizeof(T)>>(value);
			const auto exponent = unsigned(bits[8]) | (unsigned(bits[9] & 0x7f) << 8);
			std::uint64_t significand = 0;
			for (unsigned i = 0; i < 8; ++i) {
				significand |= std::uint64_t(bits[i]) << (8 * i);
			}
			const auto kind = exponent == 0x7fff ?
				(significand == 0x8000000000000000ULL ? Floating_Class::INFINITE : Floating_Class::NAN_VALUE) :
				(exponent != 0 && (significand >> 63) == 0 ? Floating_Class::NAN_VALUE : Floating_Class::FINITE);
			return {kind, (bits[9] & 0x80) != 0};
		} else if constexpr (sizeof(T) == 16 && std::numeric_limits<T>::digits == 113 && std::numeric_limits<T>::max_exponent == 16384) {
			const auto bits = std::bit_cast<std::array<std::uint64_t, 2>>(value);
			const auto high = bits[std::endian::native == std::endian::little ? 1 : 0];
			const auto low = bits[std::endian::native == std::endian::little ? 0 : 1];
			const auto exponent = (high >> 48) & 0x7fff;
			return {exponent != 0x7fff ? Floating_Class::FINITE :
				((high & 0x0000ffffffffffffULL) == 0 && low == 0 ? Floating_Class::INFINITE : Floating_Class::NAN_VALUE),
				(high >> 63) != 0};
		} else {
			static_assert(sizeof(T) == 0, "Unsupported floating-point storage representation");
		}
	}

	template<std::floating_point T>
	[[nodiscard]] constexpr bool is_finite(T value) noexcept {
		return inspect_floating_point(value).classification == Floating_Class::FINITE;
	}
	template<std::integral T>
	[[nodiscard]] constexpr bool is_finite(T) noexcept { return true; }
	[[nodiscard]] constexpr bool is_normal(float value) noexcept {
		const auto exponent = std::bit_cast<std::uint32_t>(value) & 0x7f800000U;
		return exponent != 0 && exponent != 0x7f800000U;
	}
	[[nodiscard]] constexpr bool is_zero(float value) noexcept {
		return (std::bit_cast<std::uint32_t>(value) & 0x7fffffffU) == 0;
	}
	template<std::floating_point T>
	[[nodiscard]] constexpr bool is_nan(T value) noexcept {
		return inspect_floating_point(value).classification == Floating_Class::NAN_VALUE;
	}
	template<std::floating_point T>
	[[nodiscard]] constexpr bool has_negative_sign(T value) noexcept {
		return inspect_floating_point(value).negative;
	}
	// Convert a signed 64-bit integer to IEEE-754 binary32 by rounding its exact
	// magnitude to nearest, ties to even. The computation is entirely integer
	// based; in particular it does not depend on the host floating-point mode.
	[[nodiscard]] constexpr float round_i64_to_fp32(std::int64_t value) noexcept {
		static_assert(std::numeric_limits<float>::is_iec559 && std::numeric_limits<float>::radix == 2 &&
					  std::numeric_limits<float>::digits == 24 && std::numeric_limits<float>::max_exponent == 128 &&
					  sizeof(float) == sizeof(std::uint32_t));

		const bool negative  = value < 0;
		const auto bits      = static_cast<std::uint64_t>(value);
		const auto magnitude = negative ? std::uint64_t{0} - bits : bits;
		if (magnitude == 0) {
			return std::bit_cast<float>(std::uint32_t{0});
		}

		auto          exponent = static_cast<unsigned>(std::bit_width(magnitude) - 1U);
		std::uint64_t significand;
		if (exponent <= 23U) {
			significand = magnitude << (23U - exponent);
		} else {
			const auto shift     = exponent - 23U;
			significand          = magnitude >> shift;
			const auto remainder = magnitude & ((std::uint64_t{1} << shift) - 1U);
			const auto halfway   = std::uint64_t{1} << (shift - 1U);
			const bool round_up  = remainder > halfway || (remainder == halfway && (significand & 1U) != 0);
			if (round_up && ++significand == (std::uint64_t{1} << 24U)) {
				significand >>= 1U;
				++exponent;
			}
		}

		const auto sign     = negative ? 0x80000000U : 0U;
		const auto exp_bits = (exponent + 127U) << 23U;
		const auto frac     = static_cast<std::uint32_t>(significand) & 0x007fffffU;
		return std::bit_cast<float>(sign | exp_bits | frac);
	}
} // namespace video_editor::core
