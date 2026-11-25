#pragma once

#include "h265/frame_vaapi.hpp"

#include <filesystem>

namespace video_editor::h265::encoder {
	struct Mode_Binding {
		encode::Encode_Mode_Restrictions public_;
		std::int32_t                     va_profile_    = -1;
		std::int32_t                     va_entrypoint_ = -1;
		std::uint32_t                    va_rt_format_  = 0;
		std::uint32_t                     max_quality_level_ = 0;
		bool                             va_sao_supported_ = false;
		bool                             legacy_block_sizes_ = false;
	};

	struct Capability_Snapshot {
		encode::Encode_Plan_Restrictions public_;
		std::vector<Mode_Binding>         modes_;
	};

	[[nodiscard]] Result<Capability_Snapshot>
	query_encode_plan_restrictions(const std::filesystem::path &render_node);

	class Va_Encode_Context : public H265_Encode_Context {
	public:
		[[nodiscard]] virtual Result<Va_Surface> prepare_input() = 0;
		[[nodiscard]] virtual Result<H265_Encoded_Picture> encode_prepared(const H265_Encode_Picture &) = 0;
	};

	[[nodiscard]] Result<std::unique_ptr<Va_Encode_Context>>
	create_encode_context(const H265_Encode_Plan &plan, const Capability_Snapshot &snapshot,
					  const std::filesystem::path &render_node, std::optional<std::uint32_t> reference_limit = std::nullopt);
} // namespace video_editor::h265::encoder
