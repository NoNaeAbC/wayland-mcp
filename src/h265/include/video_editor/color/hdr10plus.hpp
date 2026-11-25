#pragma once

#include <algorithm>
#include <array>
#include <cmath>
#include <cstdint>

namespace video_editor::color {
	// ST 2094-40 application 4. Values use the standard's normalized linear
	// domain (1 = 10000 cd/m2), except the explicitly named target luminance.
	// Unsupported metadata remains attached to its picture so presentation can
	// report it instead of silently treating that picture as static HDR10.
	struct Hdr10_Plus {
		bool                   supported_    = false;
		double                 target_nits_  = 0;
		double                 source_max_   = 0;
		double                 knee_x_       = 0;
		double                 knee_y_       = 0;
		std::uint8_t           anchor_count_ = 0;
		std::array<double, 15> anchors_{};
		friend bool            operator==(const Hdr10_Plus &, const Hdr10_Plus &) = default;
	};

	// Elevate to a fixed degree for a single reusable GPU program. Degree
	// elevation preserves the Bezier polynomial exactly, including zero anchors.
	inline std::array<double, 17> hdr10plus_controls(const Hdr10_Plus &metadata) {
		std::array<double, 17> controls{};
		std::uint32_t               degree = metadata.anchor_count_ + 1;
		for (std::uint32_t i = 1; i < degree; ++i) {
			controls[i] = metadata.anchors_[i - 1];
		}
		controls[degree] = 1;
		while (degree < 16) {
			auto next        = controls;
			next[degree + 1] = 1;
			for (std::uint32_t i = 1; i <= degree; ++i) {
				const double weight = double(i) / double(degree + 1);
				next[i]             = weight * controls[i - 1] + (1 - weight) * controls[i];
			}
			controls = next;
			++degree;
		}
		return controls;
	}

	inline double hdr10plus_curve(const Hdr10_Plus &metadata, double input) {
		const double s = std::clamp(input, 0.0, 1.0);
		if (s < metadata.knee_x_) {
			return s * metadata.knee_y_ / metadata.knee_x_;
		}
		if (metadata.knee_x_ >= 1) {
			return metadata.knee_y_;
		}
		const double t        = (s - metadata.knee_x_) / (1 - metadata.knee_x_);
		auto         controls = hdr10plus_controls(metadata);
		for (std::uint32_t count = 16; count > 0; --count) {
			for (std::uint32_t i = 0; i < count; ++i) {
				controls[i] = (1 - t) * controls[i] + t * controls[i + 1];
			}
		}
		return metadata.knee_y_ + (1 - metadata.knee_y_) * controls[0];
	}
} // namespace video_editor::color
