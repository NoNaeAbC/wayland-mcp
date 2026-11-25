#pragma once

#include "video_editor/color/hdr10plus.hpp"
#include "video_editor/core/numeric.hpp"
#include "video_editor/math/polynomial_sign.hpp"

#include <algorithm>
#include <array>
#include <cmath>
#include <cstdint>
#include <expected>
#include <limits>
#include <optional>
#include <span>
#include <string>
#include <vector>

namespace video_editor::color {
	// Application #4, version 1, Window 0. Values are normalized linear RGB;
	// 1.0 denotes 10,000 cd/m2. This is a logical authoring record, not T.35.
	struct App4_Statistics {
		std::array<double, 3> max_scl{};
		double average_max_rgb = 0;
		static constexpr std::array<std::uint32_t, 9> percentages{1, 5, 10, 25, 50, 75, 90, 95, 99};
		std::array<double, 9> percentiles{};
		double fraction_bright_pixels = 0; // Zero means not calculated.
		std::uint64_t samples = 0;
	};

	[[nodiscard]] inline bool valid_app4_statistics(const App4_Statistics &statistics) {
		const auto normalized=[](double value){return video_editor::core::is_finite(value)&&value>=0&&value<=1;};
		return std::ranges::all_of(statistics.max_scl,normalized)&&normalized(statistics.average_max_rgb)&&
			std::ranges::all_of(statistics.percentiles,normalized)&&normalized(statistics.fraction_bright_pixels);
	}

	struct App4_Curve {
		double knee_x = 0;
		double knee_y = 0;
		std::vector<double> anchors;
	};

	// Validate the encoded control sequence itself, even when a degenerate knee
	// makes the rendered function constant or bypasses the curved interval.
	[[nodiscard]] inline std::expected<void,std::string> validate_encoded_app4_curve(const App4_Curve &curve) {
		if (curve.anchors.size() > 9) {
			return std::unexpected("The encoded tone curve has too many anchors for Application Version 1.");
		}
		for(const double knee:{curve.knee_x,curve.knee_y}) {
			if (!video_editor::core::is_finite(knee) || knee < 0 || knee > 1) {
				return std::unexpected("A tone-curve knee coordinate is outside [0, 1].");
			}
			if(std::abs(knee*4095-std::round(knee*4095))>1e-6) {
				return std::unexpected("A tone-curve knee coordinate is not on its encoded step.");
			}
		}
		std::array<std::int64_t,11> controls{};
		for(std::size_t i=0;i<curve.anchors.size();++i) {
			const double value=curve.anchors[i];
			if (!video_editor::core::is_finite(value) || value < 0 || value > 1) {
				return std::unexpected("A tone-curve anchor is outside [0, 1].");
			}
			const double encoded=value*1023,rounded=std::round(encoded);
			if (std::abs(encoded - rounded) > 1e-6) {
				return std::unexpected("A tone-curve anchor is not on its encoded step.");
			}
			controls[i+1]=std::int64_t(rounded);
		}
		controls[curve.anchors.size()+1]=1023;
		const auto monotone=math::nondecreasing_bernstein(std::span(controls).first(curve.anchors.size()+2));
		if (!monotone) {
			return std::unexpected("The encoded tone curve could not be checked.");
		}
		if (!*monotone) {
			return std::unexpected("The encoded Bézier function decreases inside [0, 1].");
		}
		return {};
	}

	struct App4_Scene_Metadata {
		std::int64_t start_pts = 0;
		std::int64_t end_pts = 0; // Exclusive, in the caller's output timebase.
		double target_nits = 0; // Authored target, never inferred from the monitor.
		App4_Statistics statistics;
		std::optional<App4_Curve> curve;
	};

	// Bounded memory regardless of scene length. Bucketing before nearest-rank
	// selection gives the same 0.00001-quantized percentile as quantizing the
	// selected value afterward, because rounding is monotone.
	class App4_Scene_Analyzer {
	public:
		explicit App4_Scene_Analyzer(std::int64_t start_pts, std::int64_t end_pts)
			: start_pts_(start_pts), end_pts_(end_pts), counts_(100001) {}

		[[nodiscard]] std::expected<void, std::string>
		add_frame(std::int64_t pts, std::uint32_t width, std::uint32_t height,
				  std::span<const std::array<float, 3>> linear_rgb) {
			if (start_pts_ >= end_pts_ || pts < start_pts_ || pts >= end_pts_ ||
				(last_pts_ && pts <= *last_pts_)) {
				return std::unexpected("Output frame time is outside this scene or out of order");
			}
			if (width == 0 || height == 0 || std::uint64_t(width) * height != linear_rgb.size()) {
				return std::unexpected("Output frame dimensions do not match its pixels");
			}
			if (linear_rgb.size() > std::numeric_limits<std::uint64_t>::max() - samples_) {
				return std::unexpected("Scene has too many pixels to analyze");
			}
			// Validate before changing accumulators, so a bad frame cannot leave a
			// partially measured scene behind.
			for (const auto &rgb : linear_rgb) {
				for (float value : rgb) {
					if (!video_editor::core::is_finite(value) || value < 0 || value > 1) {
						return std::unexpected("Output has a nonfinite or out-of-range HDR pixel");
					}
				}
			}
			for (const auto &rgb : linear_rgb) {
				for (std::uint32_t channel = 0; channel < 3; ++channel) {
					max_scl_[channel] = std::max(max_scl_[channel], double(rgb[channel]));
				}
				const auto maximum = std::max({rgb[0], rgb[1], rgb[2]});
				sum_max_rgb_ += static_cast<long double>(maximum);
				++counts_[static_cast<std::size_t>(std::llround(double(maximum) * 100000.))];
			}
			samples_ += linear_rgb.size();
			last_pts_ = pts;
			return {};
		}

		[[nodiscard]] std::expected<App4_Statistics, std::string> finish() const {
			if (!samples_) {
				return std::unexpected("This output scene has no analyzed frames");
			}
			App4_Statistics result;
			result.samples = samples_;
			for (std::uint32_t channel = 0; channel < 3; ++channel) {
				result.max_scl[channel] = quantize(max_scl_[channel], 100000);
			}
			result.average_max_rgb = quantize(static_cast<double>(sum_max_rgb_ / samples_), 100000);
			// ST 2094-40:2020 8.5.4: Version 1's 5 and 10 slots are
			// reserved sentinels, not measured percentiles. Its 99 slot uses
			// the 99.98th percentile.
			constexpr std::array<std::uint32_t, 9> basis_points{100, 0, 0, 2500, 5000, 7500, 9000, 9500,
													  9998};
			result.percentiles[1] = 0;
			result.percentiles[2] = .00255;
			for (std::uint32_t i : {0U, 3U, 4U, 5U, 6U, 7U, 8U}) {
				// ceil(samples * p / 10000), with no overflow at uint64 limits.
				const auto whole = samples_ / 10000;
				const auto rest = samples_ % 10000;
				const auto rank = whole * basis_points[i] + (rest * basis_points[i] + 9999) / 10000;
				std::uint64_t cumulative = 0;
				for (std::size_t value = 0; value < counts_.size(); ++value) {
					cumulative += counts_[value];
					if (cumulative >= rank) {
						result.percentiles[i] = double(value) / 100000.;
						break;
					}
				}
			}
			return result;
		}

	private:
		static double quantize(double value, double denominator) {
			return std::round(value * denominator) / denominator;
		}
		std::int64_t start_pts_, end_pts_;
		std::optional<std::int64_t> last_pts_;
		std::array<double, 3> max_scl_{};
		long double sum_max_rgb_ = 0;
		std::uint64_t samples_ = 0;
		std::vector<std::uint64_t> counts_;
	};

	[[nodiscard]] inline std::expected<App4_Scene_Metadata, std::string>
	author_app4_scene(std::int64_t start_pts, std::int64_t end_pts, double target_nits,
					 App4_Statistics statistics, std::optional<App4_Curve> curve) {
		if (start_pts >= end_pts || !statistics.samples) {
			return std::unexpected("The output scene has no valid analyzed interval");
		}
		if (!video_editor::core::is_finite(target_nits) || target_nits < 1 || target_nits > 10000) {
			return std::unexpected("Choose an HDR target peak between 1 and 10,000 cd/m2");
		}
		if(!valid_app4_statistics(statistics)) {
			return std::unexpected(
					"Could not accept this scene's metadata. Its linear RGB statistics contain a nonfinite or "
					"out-of-range value.");
		}
		if (curve) {
			if (curve->anchors.size() > 9 || !video_editor::core::is_finite(curve->knee_x) || !video_editor::core::is_finite(curve->knee_y) ||
				curve->knee_x < 0 || curve->knee_x > 1 || curve->knee_y < 0 || curve->knee_y > 1) {
				return std::unexpected("This scene's tone curve has an invalid knee or too many anchors");
			}
			for (double anchor : curve->anchors) {
				if (!video_editor::core::is_finite(anchor) || anchor < 0 || anchor > 1) {
					return std::unexpected("This scene's tone curve has an invalid anchor");
				}
			}
			curve->knee_x = std::round(curve->knee_x * 4095.) / 4095.;
			curve->knee_y = std::round(curve->knee_y * 4095.) / 4095.;
			for (double &anchor: curve->anchors) {
				anchor = std::round(anchor * 1023.) / 1023.;
			}
			if(const auto valid=validate_encoded_app4_curve(*curve);!valid) {
				return std::unexpected("Could not accept this scene's tone curve. " + valid.error());
			}
		}
		return App4_Scene_Metadata{start_pts, end_pts, target_nits, std::move(statistics), std::move(curve)};
	}

	[[nodiscard]] inline std::expected<Hdr10_Plus, std::string>
	app4_preview(const App4_Scene_Metadata &scene) {
		if (!scene.curve) {
			return std::unexpected("This scene has statistics but no authored tone curve");
		}
		if(const auto valid=validate_encoded_app4_curve(*scene.curve);!valid) {
			return std::unexpected("Could not preview this scene's tone curve. " + valid.error());
		}
		if(!valid_app4_statistics(scene.statistics)) {
			return std::unexpected(
					"Could not preview this scene's tone curve. Its linear RGB statistics contain a nonfinite or "
					"out-of-range value.");
		}
		if(!video_editor::core::is_finite(scene.target_nits)||scene.target_nits<1||scene.target_nits>10000) {
			return std::unexpected(
					"Could not preview this scene's tone curve. Its authored HDR target is outside 1 to 10,000 cd/m².");
		}
		Hdr10_Plus preview;
		preview.supported_ = true;
		preview.target_nits_ = scene.target_nits;
		preview.source_max_ = std::max({scene.statistics.max_scl[0], scene.statistics.max_scl[1],
											 scene.statistics.max_scl[2]});
		if (preview.source_max_ == 0) {
			preview.source_max_ = scene.statistics.percentiles[8];
		}
		if (preview.source_max_ <= 0) {
			return std::unexpected("This scene has no measurable HDR source peak");
		}
		preview.knee_x_ = scene.curve->knee_x;
		preview.knee_y_ = scene.curve->knee_y;
		preview.anchor_count_ = static_cast<std::uint8_t>(scene.curve->anchors.size());
		std::copy(scene.curve->anchors.begin(), scene.curve->anchors.end(), preview.anchors_.begin());
		return preview;
	}
} // namespace video_editor::color
