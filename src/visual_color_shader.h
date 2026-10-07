#pragma once
// Shared GPU decoding for every advertised RGB DMA-BUF format and Wayland
// parametric color profile. COLOR_BINDING is private to the trusted pipeline.
inline constexpr char VISUAL_COLOR_GLSL[] = R"GLSL(
layout(set = 0, binding = COLOR_BINDING, std430) readonly buffer SourceColor {
	uint  transfer;
	uint  alphaMode;
	float power;
	float minimum;
	float maximum;
	float reference;
	uvec2 reserved;
	vec4  matrix[3];
	vec4  weights;
}
color;
vec4 unpackSource(uint index, uint format) {
	if (format == 0x48344258u || format == 0x48344241u) {
		vec4 p = vec4(unpackHalf2x16(raw.rgba[index]), unpackHalf2x16(raw.rgba[index + 1u]));
		if (format == 0x48344258u) {
			p.a = 1.0;
		}
		return p;
	}
	uint v = raw.rgba[index];
	vec4 p;
	if (format == 0x30335258u || format == 0x30335241u || format == 0x30334258u || format == 0x30334241u) {
		p = vec4(float(v & 1023u) / 1023.0, float((v >> 10) & 1023u) / 1023.0, float((v >> 20) & 1023u) / 1023.0,
				 float(v >> 30) / 3.0);
		if (format == 0x30335258u || format == 0x30335241u) {
			p = p.bgra;
		}
	} else {
		p = unpackUnorm4x8(v);
		if (format == 0x34325258u || format == 0x34325241u) {
			p = p.bgra;
		}
	}
	if (format == 0x34325258u || format == 0x34324258u || format == 0x30335258u || format == 0x30334258u) {
		p.a = 1.0;
	}
	return p;
}
float compound(float v) { return v <= 0.04045 ? v / 12.92 : pow((v + 0.055) / 1.055, 2.4); }
float decodeTransfer(float v) {
	if (color.power > 0.0) {
		return sign(v) * pow(abs(v), color.power);
	}
	float p = max(v, 0.0);
	if (color.transfer == 2u) {
		return pow(p, 2.2);
	}
	if (color.transfer == 3u) {
		return pow(p, 2.8);
	}
	if (color.transfer == 4u) {
		return p < 0.0912 ? p / 4.0 : pow((p + 0.1115) / 1.1115, 1.0 / 0.45);
	}
	if (color.transfer == 5u) {
		return v;
	}
	if (color.transfer == 6u) {
		return pow(10.0, 2.0 * (p - 1.0));
	}
	if (color.transfer == 7u) {
		return pow(10.0, 2.5 * (p - 1.0));
	}
	if (color.transfer == 8u) {
		return sign(v) * (abs(v) < 0.081 ? abs(v) / 4.5 : pow((abs(v) + 0.099) / 1.099, 1.0 / 0.45));
	}
	if (color.transfer == 10u) {
		return sign(v) * compound(abs(v));
	}
	if (color.transfer == 11u) {
		float q = pow(min(p, 1.0), 1.0 / 78.84375);
		return pow(max(q - 0.8359375, 0.0) / (18.851563 - 18.6875 * q), 1.0 / 0.15930176);
	}
	if (color.transfer == 12u) {
		return pow(p, 2.6) * (52.37 / 48.0);
	}
	if (color.transfer == 13u) {
		return p <= 0.5 ? p * p / 3.0 : (exp((p - 0.5599107) / 0.17883277) + 0.28466892) / 12.0;
	}
	return compound(p); // validated sRGB transfer IDs 9 and 14
}
vec4 normalizeSource(vec4 encoded) {
	encoded.a = color.alphaMode == 0u ? 1.0 : clamp(encoded.a, 0.0, 1.0);
	if (encoded.a == 0.0) {
		return vec4(0.0);
	}
	if (color.alphaMode == 2u) {
		encoded.rgb /= encoded.a;
	}
	vec3 rgb;
	if (color.transfer == 13u) {
		float gamma = max(1.2 + 0.42 * log(color.maximum / 1000.0) / log(10.0), 1.0);
		float beta  = sqrt(3.0 * pow(color.minimum / color.maximum, 1.0 / gamma));
		vec3  e     = max((1.0 - beta) * encoded.rgb + beta, vec3(0.0));
		rgb         = vec3(decodeTransfer(e.r), decodeTransfer(e.g), decodeTransfer(e.b));
		float y     = max(dot(rgb, color.weights.rgb), 0.0);
		rgb *= color.maximum / color.reference * (gamma == 1.0 ? 1.0 : pow(y, gamma - 1.0));
	} else if (color.transfer == 1u) {
		float black = pow(color.minimum, 1.0 / 2.4);
		rgb         = pow(max((pow(color.maximum, 1.0 / 2.4) - black) * encoded.rgb + black, vec3(0.0)), vec3(2.4)) /
					  color.reference;
	} else {
		float swing = color.transfer == 11u ? 10000.0 : color.maximum - color.minimum;
		rgb         = (vec3(decodeTransfer(encoded.r), decodeTransfer(encoded.g), decodeTransfer(encoded.b)) * swing +
					   color.minimum) /
					  color.reference;
	}
	return vec4(dot(color.matrix[0].rgb, rgb), dot(color.matrix[1].rgb, rgb), dot(color.matrix[2].rgb, rgb), encoded.a);
}
)GLSL";
