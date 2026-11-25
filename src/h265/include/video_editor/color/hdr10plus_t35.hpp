#pragma once

#include "video_editor/color/hdr10plus_author.hpp"
#include "video_editor/core/numeric.hpp"

#include <cmath>
#include <cstdint>
#include <expected>
#include <limits>
#include <optional>
#include <span>
#include <string>
#include <vector>

namespace video_editor::color {
	struct App4_Decoded_Payload {
		std::uint32_t application_version = 0;
		App4_Statistics statistics;
		double target_nits = 0;
		std::optional<App4_Curve> curve;
		Hdr10_Plus preview;
	};

	// HEVC registered-user-data SEI separates the country byte from data.
	// AV1 T.35 metadata carries the same byte followed by this data.
	[[nodiscard]] inline std::expected<App4_Decoded_Payload, std::string>
	parse_app4_registered_t35(std::uint8_t country, std::span<const std::uint8_t> data) {
		constexpr std::array<std::uint8_t, 5> prefix{0, 0x3c, 0, 1, 4};
		if (country != 0xb5 || data.size() <= prefix.size() ||
			!std::equal(prefix.begin(), prefix.end(), data.begin())) {
			return std::unexpected("This registered payload is not ST 2094-40 Application #4");
		}
		const std::uint32_t version = data[prefix.size()];
		if (version > 1) {
			return std::unexpected("This ST 2094-40 application version is unsupported");
		}
		struct Reader {
			std::span<const std::uint8_t> bytes;
			std::size_t bit = 0;
			bool valid = true;
			std::uint32_t take(std::uint32_t width) {
				if (width > 32 || bit > bytes.size() * 8 || width > bytes.size() * 8 - bit) {
					valid = false;
					return 0;
				}
				std::uint32_t value = 0;
				for (std::uint32_t i = 0; i < width; ++i, ++bit) {
					value = (value << 1) | ((bytes[bit / 8] >> (7 - bit % 8)) & 1U);
				}
				return value;
			}
		};
		Reader reader{data.subspan(prefix.size() + 1)};
		if (reader.take(2) != 1) {
			return std::unexpected("This HDR10+ payload uses unsupported processing windows");
		}
		App4_Decoded_Payload result;
		result.application_version = version;
		result.target_nits = reader.take(27);
		if (reader.take(1) != 0) {
			return std::unexpected("This HDR10+ payload uses an unsupported target peak table");
		}
		for (std::uint32_t channel = 0; channel < 3; ++channel) {
			result.statistics.max_scl[channel] = reader.take(17) / 100000.;
		}
		result.statistics.average_max_rgb = reader.take(17) / 100000.;
		const std::uint32_t percentile_count = reader.take(4);
		if ((version == 1 && percentile_count != 9) || percentile_count == 0 || percentile_count > 15) {
			return std::unexpected("This HDR10+ payload has an unsupported percentile count");
		}
		double last_percentile = 0;
		for (std::uint32_t i = 0; i < percentile_count; ++i) {
			const std::uint32_t percentage = reader.take(7);
			if (version == 1 && percentage != App4_Statistics::percentages[i]) {
				return std::unexpected("This HDR10+ payload has an unsupported percentile index");
			}
			last_percentile = reader.take(17) / 100000.;
			if (i < result.statistics.percentiles.size()) {
				result.statistics.percentiles[i] = last_percentile;
			}
		}
		result.statistics.fraction_bright_pixels = reader.take(10) / 1000.;
		if (reader.take(1) != 0) {
			return std::unexpected("This HDR10+ payload uses an unsupported mastering peak table");
		}
		if (reader.take(1) != 0) {
			App4_Curve curve;
			curve.knee_x = reader.take(12) / 4095.;
			curve.knee_y = reader.take(12) / 4095.;
			const auto anchors = reader.take(4);
			if (anchors > (version == 1 ? 9U : 15U)) {
				return std::unexpected("This HDR10+ tone curve has too many anchors");
			}
			for (std::uint32_t i = 0; i < anchors; ++i) {
				curve.anchors.push_back(reader.take(10) / 1023.);
			}
			result.curve = std::move(curve);
		}
		if (reader.take(1) != 0) {
			return std::unexpected("This HDR10+ payload uses unsupported saturation mapping");
		}
		if (!reader.valid || reader.bytes.size() * 8 - reader.bit > 7) {
			return std::unexpected("This HDR10+ payload is truncated or has unexpected trailing data at bit " +
								   std::to_string(reader.bit) + " of " + std::to_string(reader.bytes.size() * 8));
		}
		if (reader.bit < reader.bytes.size() * 8) {
			if (reader.take(1) != 1) {
				return std::unexpected("This HDR10+ payload has no byte-alignment marker");
			}
			while (reader.bit < reader.bytes.size() * 8) {
				if (reader.take(1) != 0) {
					return std::unexpected("This HDR10+ payload has nonzero alignment bits");
				}
			}
		}
		// Playback preserves older files whose Version 1 reserved percentile
		// slots were populated. Authoring below enforces the stricter output
		// profile instead of rejecting an otherwise usable picture here.
		if (result.target_nits < 1 || result.target_nits > 10000) {
			return std::unexpected("This HDR10+ payload has an invalid target peak");
		}
		result.preview.target_nits_ = result.target_nits;
		result.preview.source_max_ = std::max({result.statistics.max_scl[0],
			result.statistics.max_scl[1], result.statistics.max_scl[2]});
		if (result.preview.source_max_ == 0) {
			result.preview.source_max_ = last_percentile;
		}
		if (result.curve && result.preview.source_max_ > 0) {
			result.preview.supported_ = true;
			result.preview.knee_x_ = result.curve->knee_x;
			result.preview.knee_y_ = result.curve->knee_y;
			result.preview.anchor_count_ = std::uint8_t(result.curve->anchors.size());
			std::copy(result.curve->anchors.begin(), result.curve->anchors.end(),
					  result.preview.anchors_.begin());
		}
		return result;
	}
	// Registered ITU-T T.35 payload for ST 2094-40 Application #4, Version 1.
	// The caller owns codec-specific SEI/OBU wrapping and output-PTS placement.
	[[nodiscard]] inline std::expected<std::vector<std::uint8_t>, std::string>
	app4_v1_t35_payload(const App4_Scene_Metadata &scene) {
		if (!scene.curve) {
			return std::unexpected("This scene has statistics only; accept a tone curve before exporting this profile");
		}
		if(!valid_app4_statistics(scene.statistics)) {
			return std::unexpected(
					"Could not export this scene's metadata. Its linear RGB statistics contain a nonfinite or "
					"out-of-range value.");
		}
		if (!scene.statistics.samples || scene.start_pts >= scene.end_pts) {
			return std::unexpected("Analyze a valid output scene before exporting its tone map");
		}
		if (!video_editor::core::is_finite(scene.target_nits) || scene.target_nits < 1 || scene.target_nits > 10000 ||
			std::round(scene.target_nits) != scene.target_nits) {
			return std::unexpected("This output profile needs a whole-number target peak from 1 to 10,000 cd/m²");
		}
		if(const auto valid=validate_encoded_app4_curve(*scene.curve);!valid) {
			return std::unexpected("Could not export this scene's tone curve. " + valid.error());
		}
		struct Bit_Writer {
			std::vector<std::uint8_t> bytes;
			std::uint32_t bit = 0;
			void put(std::uint32_t width, std::uint32_t value) {
				for (std::uint32_t shift = width; shift > 0; --shift) {
					if (bit == 0) {
						bytes.push_back(0);
					}
					bytes.back() |= std::uint8_t(((value >> (shift - 1)) & 1U) << (7 - bit));
					bit = (bit + 1) & 7;
				}
			}
		};
		auto fixed = [](double value, std::uint32_t scale, const char *field)
				-> std::expected<std::uint32_t, std::string> {
			if (!video_editor::core::is_finite(value) || value < 0 || value > 1) {
				return std::unexpected(std::string(field) + " is outside the supported range");
			}
			const double encoded = value * scale;
			const double rounded = std::round(encoded);
			if (std::abs(encoded - rounded) > 1e-6) {
				return std::unexpected(std::string(field) + " is not on its encoded step");
			}
			return std::uint32_t(rounded);
		};
		Bit_Writer writer;
		writer.put(8, 0xb5); // ITU-T country code
		writer.put(16, 0x003c); // terminal provider
		writer.put(16, 0x0001); // provider oriented code
		writer.put(8, 4); // Application #4
		writer.put(8, 1); // Version 1
		writer.put(2, 1); // Window 0 only
		writer.put(27, std::uint32_t(scene.target_nits));
		writer.put(1, 0); // no target actual-peak table
		for (std::uint32_t channel = 0; channel < 3; ++channel) {
			auto value = fixed(scene.statistics.max_scl[channel], 100000, "MaxSCL");
			if (!value) {
				return std::unexpected(value.error());
			}
			writer.put(17, *value);
		}
		auto average = fixed(scene.statistics.average_max_rgb, 100000, "AverageMaxRGB");
		if (!average) {
			return std::unexpected(average.error());
		}
		writer.put(17, *average);
		writer.put(4, 9);
		for (std::uint32_t i = 0; i < 9; ++i) {
			auto percentile = fixed(scene.statistics.percentiles[i], 100000, "DistributionMaxRGB");
			if (!percentile) {
				return std::unexpected(percentile.error());
			}
			writer.put(7, App4_Statistics::percentages[i]);
			writer.put(17, *percentile);
		}
		if (scene.statistics.percentiles[1] != 0 ||
			std::abs(scene.statistics.percentiles[2] - .00255) > 1e-8 ||
			scene.statistics.fraction_bright_pixels != 0) {
			return std::unexpected("Version 1 reserved scene statistics have changed; analyze this scene again");
		}
		writer.put(10, 0); // FractionBrightPixels not calculated
		writer.put(1, 0); // no mastering actual-peak table
		writer.put(1, 1); // tone curve present
		auto knee_x = fixed(scene.curve->knee_x, 4095, "Knee X");
		if (!knee_x) {
			return std::unexpected(knee_x.error());
		}
		auto knee_y = fixed(scene.curve->knee_y, 4095, "Knee Y");
		if (!knee_y) {
			return std::unexpected(knee_y.error());
		}
		writer.put(12, *knee_x);
		writer.put(12, *knee_y);
		writer.put(4, std::uint32_t(scene.curve->anchors.size()));
		for (double anchor : scene.curve->anchors) {
			auto value = fixed(anchor, 1023, "Bézier anchor");
			if (!value) {
				return std::unexpected(value.error());
			}
			writer.put(10, *value);
		}
		writer.put(1, 0); // no color-saturation mapping
		if (writer.bit != 0) {
			writer.put(1, 1); // byte_alignment marker; remaining bits stay zero
		}
		return writer.bytes;
	}

	// AV1 Metadata OBU. Place this immediately before the Frame Header OBU of
	// each displayed picture, including show_existing_frame; never attach it to
	// a hidden reference picture. This function only constructs the OBU.
	[[nodiscard]] inline std::expected<std::vector<std::uint8_t>, std::string>
	app4_v1_av1_metadata_obu(const App4_Scene_Metadata &scene) {
		auto t35 = app4_v1_t35_payload(scene);
		if (!t35) {
			return std::unexpected(t35.error());
		}
		std::vector<std::uint8_t> obu;
		obu.reserve(t35->size() + 8);
		obu.push_back(0x2a); // metadata OBU, has_size_field = 1
		auto size = std::uint64_t(t35->size()) + 1; // metadata_type = 4 (T.35)
		do {
			auto byte = std::uint8_t(size & 0x7fU);
			size >>= 7;
			obu.push_back(byte | (size ? 0x80U : 0U));
		} while (size);
		obu.push_back(4);
		obu.insert(obu.end(), t35->begin(), t35->end());
		return obu;
	}
} // namespace video_editor::color
