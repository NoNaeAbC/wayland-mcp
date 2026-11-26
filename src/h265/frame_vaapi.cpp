#include "h265/frame_vaapi.hpp"

#include <algorithm>
#include <array>
#include <cassert>
#include <atomic>
#include <cstring>
#include <drm/drm_fourcc.h>
#include <fcntl.h>
#include <memory>
#include <new>
#include <unistd.h>
#include <va/va.h>
#include <va/va_drmcommon.h>
#include <va/va_drm.h>
#include <unordered_map>
#include <string_view>

namespace video_editor::h265 {
	namespace {
		[[nodiscard]] Media_Error va_error(const char *operation, VAStatus status) {
			return Media_Error{.code_ = Media_Error_Code::VAAPI_SUBMISSION_FAILED,
							   .byte_offset_ = 0, .bit_offset_ = 0,
							   .message_ = std::string(operation) + ": " + vaErrorStr(status)};
		}

		struct Image {
			VADisplay display;
			VAImage image{};
			void *map = nullptr;
			~Image() {
				if (map) { (void) vaUnmapBuffer(display, image.buf); }
				if (image.image_id != VA_INVALID_ID) { (void) vaDestroyImage(display, image.image_id); }
			}

			[[nodiscard]] Result<void> init(Frame_Layout layout) {
				image.image_id = VA_INVALID_ID;
				const auto n = vaMaxNumImageFormats(display);
				if (n <= 0) { return std::unexpected(va_error("query image formats", VA_STATUS_ERROR_OPERATION_FAILED)); }
				std::vector<VAImageFormat> formats(static_cast<std::size_t>(n));
				int count = n;
				auto status = vaQueryImageFormats(display, formats.data(), &count);
				if (status != VA_STATUS_SUCCESS) { return std::unexpected(va_error("query image formats", status)); }
				for (int i = 0; i < count; ++i) {
					if (formats[i].fourcc == layout.fourcc()) {
						status = vaCreateImage(display, &formats[i], layout.width, layout.height, &image);
						if (status != VA_STATUS_SUCCESS) { return std::unexpected(va_error("create image", status)); }
						return {};
					}
				}
				return std::unexpected(va_error("required VA image format unavailable", VA_STATUS_ERROR_INVALID_IMAGE_FORMAT));
			}

			[[nodiscard]] Result<void> mapping() {
				const auto status = vaMapBuffer(display, image.buf, &map);
				if (status != VA_STATUS_SUCCESS) { return std::unexpected(va_error("map image", status)); }
				if (!map) {
					(void) vaUnmapBuffer(display, image.buf);
					return std::unexpected(va_error("map image returned no data", VA_STATUS_ERROR_OPERATION_FAILED));
				}
				return {};
			}

			[[nodiscard]] Result<void> unmap() {
				const auto status = vaUnmapBuffer(display, image.buf);
				map = nullptr;
				if (status != VA_STATUS_SUCCESS) { return std::unexpected(va_error("unmap image", status)); }
				return {};
			}

			[[nodiscard]] std::uint8_t *row(unsigned plane, unsigned y) const {
				return static_cast<std::uint8_t *>(map) + image.offsets[plane] + std::size_t(y) * image.pitches[plane];
			}
		};

		template<class T>
		[[nodiscard]] T load(const std::uint8_t *p) {
			T value{};
			std::memcpy(&value, p, sizeof(value));
			return value;
		}

		template<class T>
		void store(std::uint8_t *p, T value) { std::memcpy(p, &value, sizeof(value)); }

		// Pack contiguous rows in ordinary RAM before copying to the VA mapping.
		// NV12 and P010 have the same geometry and differ in sample storage.
		template<class Sample, bool upload, class Words>
		[[nodiscard]] Result<void> transfer_420(const Image &image, Frame_Layout layout, Words words) {
			constexpr unsigned shift = sizeof(Sample) == 1 ? 0 : 6;
			const auto width = layout.width;
			assert(width != 0 && width % 2 == 0);
			const auto row_bytes = std::size_t(width) * sizeof(Sample);
			auto *const luma = new (std::nothrow) Sample[std::size_t(width) * 2]{};
			if (!luma) {
				return std::unexpected(va_error("allocate VA image row storage", VA_STATUS_ERROR_ALLOCATION_FAILED));
			}
			const std::unique_ptr<Sample[]> scratch(luma);
			auto *const uv = luma + width;
			for (unsigned y = 0; y < layout.height; ++y) {
				auto row = words.subspan(std::size_t(y) * width, width);
				if constexpr (upload) {
					for (unsigned x = 0; x < width; ++x) {
						luma[x] = static_cast<Sample>(((row[x] >> 10) & 1023) << shift);
					}
					std::memcpy(image.row(0, y), luma, row_bytes);
					if ((y & 1u) == 0) {
						for (unsigned x = 0; x < width; x += 2) {
							const auto word = row[x];
							uv[x] = static_cast<Sample>((word & 1023) << shift);
							uv[x + 1] = static_cast<Sample>(((word >> 20) & 1023) << shift);
						}
						std::memcpy(image.row(1, y / 2), uv, row_bytes);
					}
				} else {
					std::memcpy(luma, image.row(0, y), row_bytes);
					if ((y & 1u) == 0) {
						std::memcpy(uv, image.row(1, y / 2), row_bytes);
					}
					for (unsigned x = 0; x < width; x += 2) {
						const auto cb = std::uint32_t(uv[x]) >> shift;
						const auto cr = std::uint32_t(uv[x + 1]) >> shift;
						const auto chroma = cb | (cr << 20);
						row[x] = chroma | ((std::uint32_t(luma[x]) >> shift) << 10);
						row[x + 1] = chroma | ((std::uint32_t(luma[x + 1]) >> shift) << 10);
					}
				}
			}
			return {};
		}

		// Only rearrange already-quantized codes for VA; Vulkan computes color and chroma.
		template<bool upload, class Words>
		[[nodiscard]] Result<void> transfer(const Image &image, Frame_Layout layout, Words words) {
			if (layout.chroma == 420) {
				return layout.depth == 10 ? transfer_420<std::uint16_t, upload>(image, layout, words)
										  : transfer_420<std::uint8_t, upload>(image, layout, words);
			}
			for (unsigned y = 0; y < layout.height; ++y) {
				auto *line = image.row(0, y);
				for (unsigned x = 0; x < layout.width; ++x) {
					auto &word = words[std::size_t(y) * layout.width + x];
					std::uint32_t cb = 0, luma = 0, cr = 0;
					if constexpr (upload) {
						cb = word & 1023;
						luma = (word >> 10) & 1023;
						cr = (word >> 20) & 1023;
					}
					if (layout.chroma == 444) {
						auto *p = line + x * 4;
						if constexpr (upload) {
							const std::uint32_t packed = layout.depth == 10
								? word | 0xc0000000U : cr | (cb << 8) | (luma << 16) | 0xff000000U;
							store(p, packed);
						} else {
							const auto packed = load<std::uint32_t>(p);
							cb = layout.depth == 10 ? packed & 1023 : (packed >> 8) & 255;
							luma = layout.depth == 10 ? (packed >> 10) & 1023 : (packed >> 16) & 255;
							cr = layout.depth == 10 ? (packed >> 20) & 1023 : packed & 255;
						}
					} else if (layout.chroma == 422) {
						auto *p = line + (x & ~1U) * 4;
						auto *l = p + (x & 1U) * 4;
						if constexpr (upload) {
							store(l, static_cast<std::uint16_t>(luma << 6));
							if (!(x & 1U)) {
								store(p + 2, static_cast<std::uint16_t>(cb << 6));
								store(p + 6, static_cast<std::uint16_t>(cr << 6));
							}
						} else {
							luma = load<std::uint16_t>(l) >> 6;
							cb = load<std::uint16_t>(p + 2) >> 6;
							cr = load<std::uint16_t>(p + 6) >> 6;
						}
					}
					if constexpr (!upload) { word = cb | (luma << 10) | (cr << 20); }
				}
			}
			return {};
		}
		[[nodiscard]] bool valid_layout(Frame_Layout layout) {
			return layout.width != 0 && layout.height != 0 && layout.width % 2 == 0 && layout.height % 2 == 0 &&
				(layout.depth == 8 || layout.depth == 10) &&
				(layout.chroma == 420 || layout.chroma == 444 || (layout.chroma == 422 && layout.depth == 10));
		}

		[[nodiscard]] bool fits_image(const VAImage &image, Frame_Layout layout, bool legacy_p010 = false) {
			const bool format_matches = image.format.fourcc == layout.fourcc() ||
					(legacy_p010 && image.format.fourcc == VA_FOURCC_NV12);
			if (!format_matches ||
				image.num_planes != (layout.chroma == 420 ? 2U : 1U)) {
				return false;
			}
			const auto bytes = layout.width * (layout.chroma == 420 ? (layout.depth == 8 ? 1U : 2U) : 4U);
			for (unsigned plane = 0; plane < image.num_planes; ++plane) {
				const auto rows = plane == 0 ? layout.height : layout.height / 2;
				if (image.pitches[plane] < bytes ||
					std::size_t(image.offsets[plane]) + std::size_t(image.pitches[plane]) * (rows - 1) + bytes > image.data_size) {
					return false;
				}
			}
			return true;
		}

	}

	Va_Display::~Va_Display() {
		if (_display) { (void) vaTerminate(_display); }
		if (_fd >= 0) { (void) close(_fd); }
	}

	Result<std::shared_ptr<Va_Display>> Va_Display::open(const std::filesystem::path &render_node) {
		thread_local std::unordered_map<std::filesystem::path, std::shared_ptr<Va_Display>> displays;
		if (const auto found = displays.find(render_node); found != displays.end()) { return found->second; }
		auto next = std::shared_ptr<Va_Display>(new Va_Display{});
		next->_fd = ::open(render_node.c_str(), O_RDWR | O_CLOEXEC);
		if (next->_fd < 0) { return std::unexpected(va_error("open VA-API render node", VA_STATUS_ERROR_INVALID_DISPLAY)); }
		next->_display = vaGetDisplayDRM(next->_fd);
		if (!next->_display) { return std::unexpected(va_error("create VA display", VA_STATUS_ERROR_INVALID_DISPLAY)); }
		int major{}, minor{};
		const auto status = vaInitialize(next->_display, &major, &minor);
		if (status != VA_STATUS_SUCCESS) { return std::unexpected(va_error("initialize VA display", status)); }
		displays.emplace(render_node, next);
		return next;
	}

	std::uint64_t new_surface_owner() {
		static std::atomic<std::uint64_t> next{1};
		return next.fetch_add(1, std::memory_order_relaxed);
	}

	std::uint32_t Frame_Layout::fourcc() const {
		switch (chroma) {
			case 420: return depth == 10 ? VA_FOURCC_P010 : VA_FOURCC_NV12;
			case 422: return VA_FOURCC_Y210;
			case 444: return depth == 10 ? VA_FOURCC_Y410 : VA_FOURCC_AYUV;
		}
		return 0;
	}

	std::uint32_t Frame_Layout::rt_format() const {
		switch (chroma) {
			case 420: return depth == 10 ? VA_RT_FORMAT_YUV420_10 : VA_RT_FORMAT_YUV420;
			case 422: return VA_RT_FORMAT_YUV422_10;
			case 444: return depth == 10 ? VA_RT_FORMAT_YUV444_10 : VA_RT_FORMAT_YUV444;
		}
		return 0;
	}

	Result<VASurfaceID> create_surface(VADisplay display, Frame_Layout layout, unsigned usage) {
		if (!valid_layout(layout)) {
			return std::unexpected(va_error("unsupported video surface layout", VA_STATUS_ERROR_INVALID_IMAGE_FORMAT));
		}
		std::array attributes{
			VASurfaceAttrib{.type = VASurfaceAttribPixelFormat, .flags = VA_SURFACE_ATTRIB_SETTABLE,
							.value = {.type = VAGenericValueTypeInteger, .value = {.i = static_cast<int>(layout.fourcc())}}},
			VASurfaceAttrib{.type = VASurfaceAttribUsageHint, .flags = VA_SURFACE_ATTRIB_SETTABLE,
							.value = {.type = VAGenericValueTypeInteger, .value = {.i = static_cast<int>(usage)}}},
		};
		VASurfaceID surface = VA_INVALID_SURFACE;
		const auto status = vaCreateSurfaces(display, layout.rt_format(), layout.width, layout.height,
											 &surface, 1, attributes.data(), attributes.size());
		if (status != VA_STATUS_SUCCESS) { return std::unexpected(va_error("allocate video surface", status)); }
		return surface;
	}

	Result<void> upload_frame(VADisplay display, VASurfaceID surface, Frame_Layout layout,
							  std::span<const std::uint32_t> words) {
		if (!valid_layout(layout) || words.size() != std::size_t(layout.width) * layout.height) {
			return std::unexpected(va_error("invalid upload extent", VA_STATUS_ERROR_INVALID_PARAMETER));
		}
		Image image{.display = display};
		image.image.image_id = VA_INVALID_ID;
		const char *vendor = vaQueryVendorString(display);
		// iHD's NV12 vaPutImage can return success with corrupted rows when
		// uploading a partial tile height. Map the synchronized surface directly.
		const bool direct = layout.chroma == 420 && layout.depth == 8 && layout.height % 32 != 0 &&
			vendor && std::string_view(vendor).starts_with("Intel iHD");
		if (direct) {
			const auto ready = vaSyncSurface(display, surface);
			if (ready != VA_STATUS_SUCCESS) { return std::unexpected(va_error("synchronize upload surface", ready)); }
			const auto derived = vaDeriveImage(display, surface, &image.image);
			if (derived != VA_STATUS_SUCCESS) { return std::unexpected(va_error("derive upload surface", derived)); }
		} else if (auto result = image.init(layout); !result) {
			return result;
		}
		if (!fits_image(image.image, layout)) {
			return std::unexpected(va_error("unexpected upload image layout", VA_STATUS_ERROR_INVALID_IMAGE_FORMAT));
		}
		if (auto result = image.mapping(); !result) { return result; }
		if (auto result = transfer<true>(image, layout, words); !result) { return result; }
		if (auto result = image.unmap(); !result) { return result; }
		if (direct) { return {}; }
		const auto status = vaPutImage(display, surface, image.image.image_id, 0, 0, layout.width, layout.height,
									   0, 0, layout.width, layout.height);
		if (status != VA_STATUS_SUCCESS) { return std::unexpected(va_error("upload video frame", status)); }
		return {};
	}

	Result<std::vector<std::uint32_t>> read_frame(VADisplay display, VASurfaceID surface, Frame_Layout layout) {
		if (!valid_layout(layout)) {
			return std::unexpected(va_error("invalid download extent", VA_STATUS_ERROR_INVALID_PARAMETER));
		}
		auto status = vaSyncSurface(display, surface);
		if (status != VA_STATUS_SUCCESS) { return std::unexpected(va_error("complete video surface", status)); }
		Image image{.display = display};
		if (auto result = image.init(layout); !result) { return std::unexpected(result.error()); }
		if (!fits_image(image.image, layout)) {
			return std::unexpected(va_error("unexpected download image layout", VA_STATUS_ERROR_INVALID_IMAGE_FORMAT));
		}
		status = vaGetImage(display, surface, 0, 0, layout.width, layout.height, image.image.image_id);
		if (status != VA_STATUS_SUCCESS) { return std::unexpected(va_error("read video surface", status)); }
		if (auto result = image.mapping(); !result) { return std::unexpected(result.error()); }
		std::vector<std::uint32_t> words(std::size_t(layout.width) * layout.height);
		if (auto result = transfer<false>(image, layout, std::span(words)); !result) {
			return std::unexpected(std::move(result.error()));
		}
		if (auto result = image.unmap(); !result) { return std::unexpected(result.error()); }
		return words;
	}
	Result<std::vector<std::uint32_t>>
	read_reconstruction(VADisplay display, VASurfaceID surface, Frame_Layout layout, bool legacy_block_sizes) {
		const char *vendor = vaQueryVendorString(display);
		if (!vendor || !std::string_view(vendor).starts_with("Intel iHD") ||
			(layout.chroma == 420 && layout.depth == 8)) {
			return read_frame(display, surface, layout);
		}

		// iHD HEVC writes the HCP variant reconstruction layout to encode
		// surfaces. Y410/AYUV allocations hold three stacked planes, with the
		// hardware pitch divided by two/four. P010/Y210 use interleaved UV.
		// For 10-bit samples, the two low bits occupy a byte after the MSB row.
		auto status = vaSyncSurface(display, surface);
		if (status != VA_STATUS_SUCCESS) { return std::unexpected(va_error("complete reconstruction", status)); }
		Image image{.display = display};
		image.image.image_id = VA_INVALID_ID;
		status = vaDeriveImage(display, surface, &image.image);
		if (status != VA_STATUS_SUCCESS) { return std::unexpected(va_error("derive reconstruction", status)); }
		if (legacy_block_sizes && layout.chroma == 420 && layout.depth == 10) {
			// Legacy iHD writes P010 samples but can label the derived image NV12.
			// vaGetImage applies a variant conversion, so read the P010 planes directly.
			if (!fits_image(image.image, layout, true)) {
				return std::unexpected(va_error("unexpected P010 reconstruction layout", VA_STATUS_ERROR_INVALID_IMAGE_FORMAT));
			}
			if (auto result = image.mapping(); !result) { return std::unexpected(result.error()); }
			std::vector<std::uint32_t> words(std::size_t(layout.width) * layout.height);
			if (auto result = transfer<false>(image, layout, std::span(words)); !result) {
				return std::unexpected(std::move(result.error()));
			}
			if (auto result = image.unmap(); !result) { return std::unexpected(result.error()); }
			return words;
		}
		const auto &native = image.image;
		const auto divisor = layout.chroma == 444 ? (layout.depth == 10 ? 2U : 4U) : 1U;
		const std::size_t y_pitch = native.pitches[0] / divisor;
		const std::size_t uv_pitch = layout.chroma == 420 ? native.pitches[1] : y_pitch;
		const std::size_t y_offset = native.offsets[0];
		const std::size_t u_offset = layout.chroma == 420
			? native.offsets[1] : y_offset + y_pitch * layout.height;
		const std::size_t v_offset = u_offset + y_pitch * layout.height;
		const auto row_bytes = std::size_t(layout.width) * (layout.depth == 10 ? 2 : 1);
		const auto uv_rows = layout.chroma == 420 ? layout.height / 2 : layout.height;
		if (native.num_planes != (layout.chroma == 420 ? 2U : 1U) ||
			native.pitches[0] % divisor != 0 || y_pitch < row_bytes || uv_pitch < row_bytes ||
			(layout.chroma == 444 ? v_offset + uv_pitch * uv_rows : u_offset + uv_pitch * uv_rows) > native.data_size) {
			return std::unexpected(va_error("unexpected iHD reconstruction layout", VA_STATUS_ERROR_INVALID_IMAGE_FORMAT));
		}
		unsigned tile_height = 1;
		if (divisor > 1U) {
			VADRMPRIMESurfaceDescriptor descriptor{};
			status = vaExportSurfaceHandle(display, surface, VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
										   VA_EXPORT_SURFACE_READ_ONLY, &descriptor);
			if (status != VA_STATUS_SUCCESS) { return std::unexpected(va_error("export reconstruction layout", status)); }
			const auto modifier = descriptor.objects[0].drm_format_modifier;
			for (unsigned i = 0; i < descriptor.num_objects; ++i) { (void) close(descriptor.objects[i].fd); }
			if (modifier == I915_FORMAT_MOD_4_TILED || modifier == I915_FORMAT_MOD_Y_TILED) {
				tile_height = 32;
			} else if (modifier != DRM_FORMAT_MOD_LINEAR) {
				return std::unexpected(va_error("unsupported iHD reconstruction modifier", VA_STATUS_ERROR_INVALID_IMAGE_FORMAT));
			}
		}
		if (auto result = image.mapping(); !result) { return std::unexpected(result.error()); }
		const auto *base = static_cast<const std::uint8_t *>(image.map);
		std::vector<std::uint32_t> words(std::size_t(layout.width) * layout.height);
		const auto unpack = [layout](const std::uint8_t *line, unsigned x) -> std::uint32_t {
			if (layout.depth == 8) { return line[x]; }
			return (static_cast<std::uint32_t>(line[x]) << 2) | (line[layout.width + x] & 3U);
		};
		const auto plane_row_offset = [&](std::size_t offset, unsigned row, std::size_t pitch) {
			const auto tile_row = (offset / pitch + row) / tile_height;
			const auto within_tile = (offset / pitch + row) % tile_height;
			return (offset % pitch) + pitch *
				((tile_row / divisor * tile_height + within_tile) * divisor + tile_row % divisor);
		};
		for (unsigned y = 0; y < layout.height; ++y) {
			const auto luma_row = plane_row_offset(y_offset, y, y_pitch);
			const auto chroma_row = plane_row_offset(u_offset, layout.chroma == 420 ? y / 2 : y, uv_pitch);
			const auto cr_row = layout.chroma == 444 ? plane_row_offset(v_offset, y, uv_pitch) : chroma_row;
			if (std::max({luma_row, chroma_row, cr_row}) + row_bytes > native.data_size) {
				return std::unexpected(va_error("iHD reconstruction row exceeds image", VA_STATUS_ERROR_INVALID_IMAGE_FORMAT));
			}
			const auto *luma = base + luma_row;
			const auto *chroma = base + chroma_row;
			const auto *cr_plane = layout.chroma == 444 ? base + cr_row : nullptr;
			for (unsigned x = 0; x < layout.width; ++x) {
				const auto cb = unpack(chroma, layout.chroma == 444 ? x : (x & ~1U));
				const auto cr = cr_plane ? unpack(cr_plane, x) : unpack(chroma, (x & ~1U) + 1U);
				words[std::size_t(y) * layout.width + x] = cb | (unpack(luma, x) << 10) | (cr << 20);
			}
		}
		if (auto result = image.unmap(); !result) { return std::unexpected(result.error()); }
		return words;
	}

}
