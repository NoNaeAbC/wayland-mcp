#pragma once
#include <expected>
#include <string>
#include <string_view>
#include <utility>

namespace visual {
	template<class T>
	using Result = std::expected<T, std::string>;
	using Status = Result<void>;

	// Resource handles belong to one owner; copying or moving them implicitly
	// would duplicate destruction. Owners stay at stable addresses behind pointers.
	struct Non_Copyable {
		Non_Copyable()                                = default;
		~Non_Copyable()                               = default;
		Non_Copyable(const Non_Copyable &)            = delete;
		Non_Copyable &operator=(const Non_Copyable &) = delete;
		Non_Copyable(Non_Copyable &&)                 = delete;
		Non_Copyable &operator=(Non_Copyable &&)      = delete;
	};
} // namespace visual

// Propagate an explicit error while allowing ordinary RAII cleanup to run.
#define VISUAL_TRY(expression)                                                                                         \
	do {                                                                                                               \
		auto visual_status = (expression);                                                                             \
		if (!visual_status) {                                                                                          \
			return std::unexpected(std::move(visual_status.error()));                                                  \
		}                                                                                                              \
	} while (false)
