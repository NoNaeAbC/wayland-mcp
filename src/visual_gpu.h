#pragma once
#include <cstdint>
#include <type_traits>
// Plain layouts and fixed arrays are the Rust/GLSL ABI boundary. Internal
// ownership and buffer views use C++ containers and spans.
struct Visual_Rule {
	uint32_t _offset, _width, _height, _kind;
	float    _threshold;
	uint32_t _minimum, _above, _debounce, _cooldown;
};
struct Visual_Rect {
	uint32_t _x, _y, _width, _height;
};
struct Visual_Plane {
	int      _fd;
	uint32_t _offset, _stride;
	uint64_t _modifier;
};
struct Visual_Event {
	uint32_t _rule, _active, _frame, _reserved;
};
struct Visual_Shader_Pass {
	const char *_source;
	uint32_t    _source_size;
	uint32_t    _groups[3];
};
struct Visual_Color_Profile {
	uint32_t _transfer, _alpha_mode;
	float    _power, _minimum, _maximum, _reference;
	uint32_t _reserved[2];
	float    _matrix[3][4], _weights[4];
};
static_assert(sizeof(Visual_Color_Profile) == 96);
struct Visual_Program_Config {
	uint32_t _width, _height, _result_bytes, _state_bytes, _scratch_bytes, _parameter_bytes;
	uint32_t _previous_frame, _feedback, _alpha_mode, _pass_count;
};
// Renaming native fields must preserve the Rust/GLSL binary layouts.
static_assert(sizeof(Visual_Rule) == 36 && alignof(Visual_Rule) == 4);
static_assert(sizeof(Visual_Rect) == 16 && alignof(Visual_Rect) == 4);
static_assert(sizeof(Visual_Plane) == 24 && alignof(Visual_Plane) == 8);
static_assert(sizeof(Visual_Event) == 16 && alignof(Visual_Event) == 4);
static_assert(sizeof(Visual_Shader_Pass) == 24 && alignof(Visual_Shader_Pass) == 8);
static_assert(sizeof(Visual_Program_Config) == 40 && alignof(Visual_Program_Config) == 4);
static_assert(std::is_standard_layout_v<Visual_Color_Profile> && std::is_trivially_copyable_v<Visual_Color_Profile>);

extern "C" {
void *visual_context_create(uint32_t major, uint32_t minor, char *error);
void  visual_context_destroy(void *);
void *visual_snapshot_create(void *, uint32_t width, uint32_t height, uint32_t format, char *error);
void  visual_snapshot_destroy(void *);
int   visual_snapshot_capture(void *, uint64_t key, const Visual_Plane *, uint32_t plane_count, char *error);
int   visual_snapshot_read(void *, uint8_t *output, uint32_t output_bytes, char *error);
// Explicit one-shot MCP screenshot; not used by the event-only frame loop.
int   visual_snapshot_raw(void *, uint32_t width, uint32_t height, uint32_t format, const Visual_Plane *,
						  uint32_t plane_count, uint8_t *output, uint32_t output_bytes, char *error);
void *visual_program_create(void *, const Visual_Rule *, const Visual_Rect *, uint32_t, char *);
void  visual_program_destroy(void *);
int   visual_process(void *, uint64_t key, uint32_t width, uint32_t height, uint32_t format, const Visual_Plane *,
					 uint32_t plane_count, Visual_Event *, char *);
int   visual_capture(void *, uint64_t key, uint32_t width, uint32_t height, uint32_t format, const Visual_Plane *,
					 uint32_t plane_count, char *);
int   visual_analyze(void *, Visual_Event *, char *);
int   visual_set_color(void *, const Visual_Color_Profile *, char *);
void *visual_runtime_create(void *, const Visual_Program_Config *, const Visual_Shader_Pass *,
							const uint8_t *parameters, char *);
void  visual_runtime_destroy(void *);
int visual_runtime_capture(void *, uint64_t key, uint32_t width, uint32_t height, uint32_t format, const Visual_Plane *,
						   uint32_t plane_count, char *);
int visual_runtime_analyze(void *, uint8_t *result, char *);
int visual_runtime_set_color(void *, const Visual_Color_Profile *, char *);
int visual_test(void *, uint32_t pattern, Visual_Event *, char *);
}
