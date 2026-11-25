#pragma once

#include <expected>
#include <utility>

namespace video_editor::core::detail {
	template<class T, class E>
	T take_result(std::expected<T, E> &&result) { return std::move(*result); }
	template<class E>
	void take_result(std::expected<void, E> &&) {}
} // namespace video_editor::core::detail

// Evaluate once, return the original explicit error, then move the checked
// value. The project's GNU C++26 compiler supports statement expressions.
#define VIDEO_EDITOR_TRY(...)                                                   \
	__extension__ ({ auto video_editor_checked_result = (__VA_ARGS__);                        \
	   if (!video_editor_checked_result)                                       \
	       return std::unexpected(std::move(video_editor_checked_result.error())); \
	   ::video_editor::core::detail::take_result(std::move(video_editor_checked_result)); })
