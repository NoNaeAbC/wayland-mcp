#pragma once
#include <cstdint>
#include <expected>
#include <span>

namespace video_editor::math {
	enum class Polynomial_Sign_Error { DEGREE_LIMIT, ANALYSIS_FAILED };
	// Exact integer coefficients in ascending powers; zero is nonnegative.
	// Bounds include both endpoints. At most 33 coefficients are admitted, independent of
	// coefficient magnitude. No numerical samples or root tolerances are used.
	[[nodiscard]] std::expected<bool, Polynomial_Sign_Error>
	nonnegative_on_unit_interval(std::span<const std::int64_t> coefficients);
	// Integer Bernstein ordinates with one common positive unit. This checks
	// the derivative on the whole unit interval, admitting isolated stationary
	// points and nonordered controls when the mathematical function is monotone.
	[[nodiscard]] std::expected<bool, Polynomial_Sign_Error>
	nondecreasing_bernstein(std::span<const std::int64_t> controls);
} // namespace video_editor::math
