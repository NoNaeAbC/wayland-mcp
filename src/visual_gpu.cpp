// C ABI entry points for Rust. Implementations and probes share native types.
#include "visual_gpu.h"
#include "visual_gpu_internal.h"
#include "visual_program_gpu.h"
#include "visual_result.h"
#include <cstdint>
#include <expected>
#include <memory>
#include <span>

extern "C" void *visual_snapshot_create(void *c, uint32_t w, uint32_t h, uint32_t fmt, char *error) {
	auto snapshot = std::make_unique<Cached_Snapshot_Native>(static_cast<Context *>(c), w, h, fmt);
	if (auto status = snapshot->init(); !status) {
		error_copy(error, status.error());
		return nullptr;
	}
	return snapshot.release();
}
extern "C" void visual_snapshot_destroy(void *p) { delete static_cast<Cached_Snapshot_Native *>(p); }
extern "C" int visual_snapshot_capture(void *p, uint64_t key, const Visual_Plane *planes, uint32_t count, char *error) {
	auto &snapshot          = *static_cast<Cached_Snapshot_Native *>(p);
	snapshot._raw._captured = false;
	return ffi_status(
			snapshot._raw.capture_buffer(key, snapshot._width, snapshot._height, snapshot._format, planes, count, true),
			error);
}
extern "C" int visual_snapshot_read(void *p, uint8_t *output, uint32_t bytes, char *error) {
	return ffi_status(static_cast<Cached_Snapshot_Native *>(p)->_raw.read_snapshot(output, bytes), error);
}
extern "C" void *visual_context_create(uint32_t major, uint32_t minor, char *error) {
	auto context = std::make_unique<Context>();
	if (auto status = context->init(major, minor); !status) {
		error_copy(error, status.error());
		return nullptr;
	}
	return context.release();
}
extern "C" void visual_context_destroy(void *c) { delete static_cast<Context *>(c); }
extern "C" int  visual_snapshot_raw(void *context, uint32_t w, uint32_t h, uint32_t fmt, const Visual_Plane *planes,
									uint32_t count, uint8_t *raw, uint32_t output_bytes, char *error) {
	auto const capture = [&]() -> visual::Status {
		uint32_t const bytes_per_pixel = (fmt == 0x48344258u || fmt == 0x48344241u) ? 8 : 4;
		if (!w || !h || uint64_t(w) * h > 8'388'608 || !raw || output_bytes != uint64_t(w) * h * bytes_per_pixel) {
			return std::unexpected("invalid screenshot extent/output");
		}
		auto *device = static_cast<Context *>(context);
		if (!device || !planes || !count || count > 4) {
			return std::unexpected("invalid screenshot source");
		}
		Program snapshot(device);
		VISUAL_TRY(snapshot.init_raw(w, h, bytes_per_pixel));
		Imported image(device);
		VISUAL_TRY(image.init(0, w, h, fmt, std::span(planes, count), true));
		VISUAL_TRY(snapshot.capture(&image, 0));
		return snapshot.read_snapshot(raw, output_bytes);
	};
	return ffi_status(capture(), error);
}
extern "C" void *visual_program_create(void *c, const Visual_Rule *rules, const Visual_Rect *rects, uint32_t count,
									   char *error) {
	if (!c || !rules || !rects || !count || count > 16) {
		error_copy(error, "invalid visual rules");
		return nullptr;
	}
	auto program = std::make_unique<Program>(static_cast<Context *>(c));
	if (auto status = program->init(std::span(rules, count), std::span(rects, count)); !status) {
		error_copy(error, status.error());
		return nullptr;
	}
	return program.release();
}
extern "C" void visual_program_destroy(void *p) { delete static_cast<Program *>(p); }
extern "C" int  visual_process(void *p, uint64_t key, uint32_t w, uint32_t h, uint32_t fmt, const Visual_Plane *planes,
							   uint32_t count, Visual_Event *output, char *error) {
	return ffi_result(static_cast<Program *>(p)->process(key, w, h, fmt, planes, count, output), error);
}
extern "C" int visual_capture(void *p, uint64_t key, uint32_t w, uint32_t h, uint32_t fmt, const Visual_Plane *planes,
							  uint32_t count, char *error) {
	return ffi_status(static_cast<Program *>(p)->capture_buffer(key, w, h, fmt, planes, count), error);
}
extern "C" int visual_analyze(void *p, Visual_Event *output, char *error) {
	return ffi_result(static_cast<Program *>(p)->analyze(output), error);
}
extern "C" int visual_set_color(void *p, const Visual_Color_Profile *color, char *) {
	static_cast<Program *>(p)->set_color(*color);
	return 0;
}
extern "C" int visual_test(void *p, uint32_t pattern, Visual_Event *output, char *error) {
	return ffi_result(static_cast<Program *>(p)->run(nullptr, pattern, output), error);
}

extern "C" void *visual_runtime_create(void *ctx, const Visual_Program_Config *config, const Visual_Shader_Pass *passes,
									   const uint8_t *parameters, char *error) {
	if (!ctx || !config || !passes || (config->_parameter_bytes && !parameters)) {
		error_copy(error, "invalid visual program arguments");
		return nullptr;
	}
	auto program = std::make_unique<Runtime_Program>(static_cast<Context *>(ctx));
	if (auto status = program->init(*config, std::span(passes, config->_pass_count),
									std::span(parameters, config->_parameter_bytes));
		!status) {
		error_copy(error, status.error());
		return nullptr;
	}
	return program.release();
}
extern "C" void visual_runtime_destroy(void *p) { delete static_cast<Runtime_Program *>(p); }
extern "C" int  visual_runtime_capture(void *p, uint64_t key, uint32_t width, uint32_t height, uint32_t format,
									   const Visual_Plane *planes, uint32_t count, char *error) {
	auto *program = static_cast<Runtime_Program *>(p);
	if (width != program->_cfg._width || height != program->_cfg._height) {
		error_copy(error, "program geometry changed; resubscribe");
		return -1;
	}
	return ffi_status(program->capture_buffer(key, width, height, format, planes, count), error);
}
extern "C" int visual_runtime_analyze(void *p, uint8_t *result, char *error) {
	return ffi_result(static_cast<Runtime_Program *>(p)->analyze_result(result), error);
}
extern "C" int visual_runtime_set_color(void *p, const Visual_Color_Profile *color, char *) {
	static_cast<Runtime_Program *>(p)->set_color(*color);
	return 0;
}
