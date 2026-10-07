#pragma once
#include "visual_result.h"
#include <cstdint>
#include <string_view>
#include <vector>

// Source is supplied in memory. No include resolver or file loader is
// installed.
[[nodiscard]] visual::Result<std::vector<uint32_t>> compile_visual_glsl(std::string_view source);
