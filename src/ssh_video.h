#pragma once
#include "visual_gpu.h"
#include <cstddef>
#include <cstdint>

namespace video_editor::h265 {
	struct Frame_Format;
}

struct Video_Options {
	std::uint32_t width, height, coded_width, coded_height;
	std::uint32_t format, depth, chroma, mode;
	std::uint32_t tone_map;
	float exposure;
	std::uint32_t background, tone_points;
	float curve[16][2];
	std::uint32_t source_stride, source_offset, output_offset, reserved;
};
static_assert(sizeof(Video_Options) == 192);

struct Video_Shm_Source {
	int _fd;
	std::uint32_t _stride;
	std::uint64_t _offset;
};

enum class Video_Rate_Mode : std::uint32_t { Fixed_Qp, Vbr, Constant_Quality };

struct Video_Rate {
	Video_Rate_Mode _mode;
	std::uint32_t _frame_rate;
	std::uint32_t _target_bitrate;
	std::uint32_t _quality;
	std::uint32_t _automatic_bitrate;

	[[nodiscard]] bool operator==(const Video_Rate &) const = default;
};

struct Video_Encode_Target {
	const char *_device;
	void *_stream;
	bool _with_alpha;
	bool _compare_alpha;
};

struct Video_Bitstream {
	const std::uint8_t *_color;
	const std::uint8_t *_alpha;
	std::uint32_t _color_size, _alpha_size, _poc, _alpha_poc;
	bool _scc;
	bool _reuse_alpha;
};

struct Video_Display_Target {
	const Visual_Plane *_dma;
	const Video_Shm_Source *_shm;
};

extern "C" {
void *ssh_video_create_stream();
void *ssh_video_capture_stream();
void ssh_video_drop_stream(void *);
void *ssh_video_create_converter(const char *device);
void ssh_video_take_converter(void *);
void ssh_video_drop_converter(void *);
std::uint32_t ssh_video_encode_formats(const char *device, const video_editor::h265::Frame_Format *,
									 std::uint32_t count, Video_Rate_Mode mode);
std::uint32_t ssh_video_decode_formats(const char *device, const video_editor::h265::Frame_Format *, std::uint32_t count);
int ssh_video_prepare_encoder(const char *device, const Video_Options *, bool scc, const Video_Rate *, char *error, void *stream);
std::uint32_t ssh_video_capture_modifiers(const char *device, std::uint32_t format,
										 std::uint64_t *modifiers, std::uint32_t capacity);
int ssh_video_convert_snapshot(void *snapshot, const Video_Options *, const Visual_Color_Profile *,
							   void *stream, char *error);
int ssh_video_copy_snapshot(void *snapshot, const Video_Options *, const Visual_Plane *destination,
	std::uint32_t destination_width, char *error);
int ssh_video_convert_buffer(const Video_Options *, const Video_Shm_Source *, const Visual_Plane *,
	std::uint32_t plane_width, const Visual_Color_Profile *, bool implicit_sync, const Video_Encode_Target *, char *error);
int ssh_video_copy_dma_buf(const char *device, const Video_Options *, const Visual_Plane *source,
	const Visual_Plane *destination, std::uint32_t destination_width, bool implicit_sync, char *error);
void *ssh_video_encode(const char *device, const Video_Options *, bool scc, std::uint8_t qp,
					  const Video_Rate *, std::uint32_t stream_poc, char *error, void *stream);
void *wayland_recording_encoder_create(const char *device, std::uint32_t width,
	std::uint32_t height, std::uint32_t frame_rate, char *error);
void *wayland_recording_encoder_group(void *encoder, const std::uint32_t *const *frames,
	std::size_t count, char *error);
void wayland_recording_encoder_destroy(void *encoder);
const std::uint8_t *ssh_video_packet_data(const void *, std::uint32_t *size);
void ssh_video_packet_destroy(void *);
int ssh_video_decode(const char *device, const Video_Options *, void *stream,
	const Video_Bitstream *, const Video_Display_Target *, char *error);
void *ssh_video_decode_preview(const char *device, const Video_Options *, bool scc, const std::uint8_t *data,
	std::uint32_t size, std::uint32_t width, std::uint32_t height, char *error);
}
