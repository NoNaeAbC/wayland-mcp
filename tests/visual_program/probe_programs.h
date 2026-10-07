#pragma once

inline constexpr char SCAN_SOURCE[] = R"VGPU(
#version 450
// Generic pass 1 of a two-pass color-target probe.
// Input is linear BT.2020 RGBA16F. All coordinates are full-frame pixels.
layout(local_size_x = 16, local_size_y = 16, local_size_z = 1) in;

layout(set = 0, binding = 0, rgba16f) readonly uniform image2D currentFrame;
layout(set = 0, binding = 1, rgba16f) readonly uniform image2D previousFrame;

// Zeroed once before pass 1. Coordinate minima are stored inverted so zero is
// a valid empty initialization value for atomicMax.
layout(set = 0, binding = 4, std430) buffer Scratch {
	uint matchCount;
	uint inverseMinX;
	uint inverseMinY;
	uint maxX;
	uint maxY;
	uint changedCount;
	uint reserved0;
	uint reserved1;
}
scratch;

layout(set = 0, binding = 6, std430) readonly buffer Parameters {
	vec4  targetLow;  // Inclusive RGBA lower bound in linear BT.2020.
	vec4  targetHigh; // Inclusive RGBA upper bound in linear BT.2020.
	uvec4 roi;        // x, y, width, height in full-frame pixels.
	uint  minPixels;  // Minimum matching pixels for TARGET_PRESENT.
	uint  reserved0;
	uint  reserved1;
	uint  reserved2;
}
params;

layout(push_constant) uniform FrameInfo {
	uint frameWidth;
	uint frameHeight;
	uint sequence;
	uint historyValid;
}
frameInfo;

bool matchesTarget(vec4 rgba) {
	return all(greaterThanEqual(rgba, params.targetLow)) && all(lessThanEqual(rgba, params.targetHigh));
}

void main() {
	uvec2 inRoi = gl_GlobalInvocationID.xy;
	if (inRoi.x >= params.roi.z || inRoi.y >= params.roi.w) {
		return;
	}

	uvec2 pixel = params.roi.xy + inRoi;
	// Runtime validation must keep the ROI inside the bound images. Retain a
	// shader guard as defense in depth for malformed dispatch parameters.
	if (pixel.x >= frameInfo.frameWidth || pixel.y >= frameInfo.frameHeight) {
		return;
	}

	vec4 nowRgba    = imageLoad(currentFrame, ivec2(pixel));
	bool nowMatches = matchesTarget(nowRgba);
	if (nowMatches) {
		atomicAdd(scratch.matchCount, 1u);
		atomicMax(scratch.inverseMinX, frameInfo.frameWidth - 1u - pixel.x);
		atomicMax(scratch.inverseMinY, frameInfo.frameHeight - 1u - pixel.y);
		atomicMax(scratch.maxX, pixel.x);
		atomicMax(scratch.maxY, pixel.y);
	}

	if (frameInfo.historyValid != 0u) {
		bool wasMatching = matchesTarget(imageLoad(previousFrame, ivec2(pixel)));
		if (nowMatches != wasMatching) {
			atomicAdd(scratch.changedCount, 1u);
		}
	}
}
)VGPU";

inline constexpr char REDUCE_SOURCE[] = R"VGPU(
#version 450
// Generic pass 2: finalize the per-pixel reductions and emit an 8-word result.
layout(local_size_x = 1, local_size_y = 1, local_size_z = 1) in;

layout(set = 0, binding = 2, std430) readonly buffer StatePrevious { uint wasPresent; }
statePrevious;

layout(set = 0, binding = 3, std430) buffer StateNext { uint isPresent; }
stateNext;

layout(set = 0, binding = 4, std430) readonly buffer Scratch {
	uint matchCount;
	uint inverseMinX;
	uint inverseMinY;
	uint maxX;
	uint maxY;
	uint changedCount;
	uint reserved0;
	uint reserved1;
}
scratch;

layout(set = 0, binding = 5, std430) buffer Result {
	uint flags;
	uint minX;
	uint minY;
	uint maxX;
	uint maxY;
	uint sequence;
	uint reserved0;
	uint reserved1;
}
result;

layout(set = 0, binding = 6, std430) readonly buffer Parameters {
	vec4  targetLow;
	vec4  targetHigh;
	uvec4 roi;
	uint  minPixels;
	uint  reserved0;
	uint  reserved1;
	uint  reserved2;
}
params;

layout(push_constant) uniform FrameInfo {
	uint frameWidth;
	uint frameHeight;
	uint sequence;
	uint historyValid;
}
frameInfo;

const uint FLAG_PRESENT       = 1u << 0;
const uint FLAG_ENTERED       = 1u << 1;
const uint FLAG_EXITED        = 1u << 2;
const uint FLAG_MASK_CHANGED  = 1u << 3;
const uint FLAG_HISTORY_VALID = 1u << 4;

void main() {
	bool present = scratch.matchCount >= max(params.minPixels, 1u);
	bool valid   = frameInfo.historyValid != 0u;
	uint flags   = 0u;

	if (present) {
		flags |= FLAG_PRESENT;
	}
	if (valid) {
		flags |= FLAG_HISTORY_VALID;
		if (present && statePrevious.wasPresent == 0u) {
			flags |= FLAG_ENTERED;
		}
		if (!present && statePrevious.wasPresent != 0u) {
			flags |= FLAG_EXITED;
		}
		if (scratch.changedCount != 0u) {
			flags |= FLAG_MASK_CHANGED;
		}
	}

	// Write every result word on every dispatch. The result buffer can be
	// zeroed by the framework, but correctness does not rely on stale flags or
	// stale coordinates being cleared there.
	result.flags     = flags;
	result.minX      = present ? frameInfo.frameWidth - 1u - scratch.inverseMinX : 0u;
	result.minY      = present ? frameInfo.frameHeight - 1u - scratch.inverseMinY : 0u;
	result.maxX      = present ? scratch.maxX : 0u;
	result.maxY      = present ? scratch.maxY : 0u;
	result.sequence  = frameInfo.sequence;
	result.reserved0 = 0u;
	result.reserved1 = 0u;

	// State ping-pong is swapped only after pass 2 completes.
	stateNext.isPresent = uint(present);
}
)VGPU";

inline constexpr char REDUCE_FEEDBACK_SOURCE[] = R"VGPU(
#version 450
// Pass 2 variant for automatic result feedback. Binding 2 is the previous
// 32-byte result/state record; binding 5 is the current result record. The
// framework copies result to the next ping-pong state buffer after completion.
layout(local_size_x = 1, local_size_y = 1, local_size_z = 1) in;

layout(set = 0, binding = 2, std430) readonly buffer PreviousResult {
	uint flags;
	uint minX;
	uint minY;
	uint maxX;
	uint maxY;
	uint sequence;
	uint reserved0;
	uint reserved1;
}
previousResult;

layout(set = 0, binding = 4, std430) readonly buffer Scratch {
	uint matchCount;
	uint inverseMinX;
	uint inverseMinY;
	uint maxX;
	uint maxY;
	uint changedCount;
	uint reserved0;
	uint reserved1;
}
scratch;

layout(set = 0, binding = 5, std430) buffer Result {
	uint flags;
	uint minX;
	uint minY;
	uint maxX;
	uint maxY;
	uint sequence;
	uint reserved0;
	uint reserved1;
}
result;

layout(set = 0, binding = 6, std430) readonly buffer Parameters {
	vec4  targetLow;
	vec4  targetHigh;
	uvec4 roi;
	uint  minPixels;
	uint  reserved0;
	uint  reserved1;
	uint  reserved2;
}
params;

layout(push_constant) uniform FrameInfo {
	uint frameWidth;
	uint frameHeight;
	uint sequence;
	uint historyValid;
}
frameInfo;

const uint FLAG_PRESENT       = 1u << 0;
const uint FLAG_ENTERED       = 1u << 1;
const uint FLAG_EXITED        = 1u << 2;
const uint FLAG_MASK_CHANGED  = 1u << 3;
const uint FLAG_HISTORY_VALID = 1u << 4;

void main() {
	bool present    = scratch.matchCount >= max(params.minPixels, 1u);
	bool valid      = frameInfo.historyValid != 0u;
	bool wasPresent = (previousResult.flags & FLAG_PRESENT) != 0u;
	uint flags      = 0u;

	if (present) {
		flags |= FLAG_PRESENT;
	}
	if (valid) {
		flags |= FLAG_HISTORY_VALID;
		if (present && !wasPresent) {
			flags |= FLAG_ENTERED;
		}
		if (!present && wasPresent) {
			flags |= FLAG_EXITED;
		}
		if (scratch.changedCount != 0u) {
			flags |= FLAG_MASK_CHANGED;
		}
	}

	// Every field is overwritten. Result ping-pongs as state; the framework
	// copies this output into the next feedback slot only after completion.
	result.flags     = flags;
	result.minX      = present ? frameInfo.frameWidth - 1u - scratch.inverseMinX : 0u;
	result.minY      = present ? frameInfo.frameHeight - 1u - scratch.inverseMinY : 0u;
	result.maxX      = present ? scratch.maxX : 0u;
	result.maxY      = present ? scratch.maxY : 0u;
	result.sequence  = frameInfo.sequence;
	result.reserved0 = 0u;
	result.reserved1 = 0u;
}
)VGPU";
