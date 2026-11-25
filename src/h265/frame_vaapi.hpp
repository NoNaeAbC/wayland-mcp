#pragma once

#include <cstdint>
#include <filesystem>
#include <optional>
#include <memory>
#include <span>
#include <variant>
#include <vector>
#include <va/va.h>
#include "video_editor/h265/api.hpp"

namespace video_editor::h265 {
	class Va_Display {
	public:
		Va_Display(const Va_Display &) = delete;
		Va_Display &operator=(const Va_Display &) = delete;
		~Va_Display();

		[[nodiscard]] static Result<std::shared_ptr<Va_Display>> open(const std::filesystem::path &render_node);
		[[nodiscard]] VADisplay get() const { return _display; }

	private:
		Va_Display() = default;
		int _fd = -1;
		VADisplay _display{};
	};

	struct Frame_Layout {
		std::uint32_t width;
		std::uint32_t height;
		std::uint32_t depth;
		std::uint32_t chroma;
		friend bool operator==(Frame_Layout, Frame_Layout) = default;
		[[nodiscard]] std::uint32_t fourcc() const;
		[[nodiscard]] std::uint32_t rt_format() const;
	};
	struct Frame_Format {
		std::uint32_t chroma;
		std::uint32_t depth;
		std::uint32_t screen_content;
	};

	[[nodiscard]] std::uint64_t new_surface_owner();

	struct Va_Surface {
		VADisplay display;
		VASurfaceID surface;
		std::uint64_t owner;
		std::shared_ptr<void> lifetime{};
	};
	// The decoded surface is borrowed until the next decode of that stream on this thread.
	using Decoded_Frame = std::variant<std::vector<std::uint32_t>, Va_Surface>;

	[[nodiscard]] std::uint32_t decode_formats(const char *device, std::span<const Frame_Format> formats);

	// Each word stores Cb:Y:Cr in consecutive 10-bit fields, regardless of depth.
	[[nodiscard]] Result<VASurfaceID> create_surface(VADisplay display, Frame_Layout layout, unsigned usage);
	[[nodiscard]] Result<void> upload_frame(VADisplay display, VASurfaceID surface, Frame_Layout layout,
										   std::span<const std::uint32_t> words);
	[[nodiscard]] Result<std::vector<std::uint32_t>>
	read_frame(VADisplay display, VASurfaceID surface, Frame_Layout layout);
	[[nodiscard]] Result<std::vector<std::uint32_t>>
	read_reconstruction(VADisplay display, VASurfaceID surface, Frame_Layout layout, bool legacy_block_sizes);
	struct Decoder_State;
	[[nodiscard]] Decoder_State *create_decoder_state();
	void destroy_decoder_state(Decoder_State *);
	[[nodiscard]] Result<Decoded_Frame>
	decode_picture(const char *device, Frame_Layout layout, bool scc, std::span<const std::uint8_t> bitstream,
			   bool alpha = false, bool keep_surface = false,
			   std::optional<std::uint16_t> stream_poc = std::nullopt, Decoder_State *state = nullptr,
			   bool require_surface = false);
}
