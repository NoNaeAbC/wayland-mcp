#pragma once

#include <algorithm>
#include <array>
#include <cstdint>
#include <optional>
#include <span>
#include <vector>

namespace video_editor::color {
	// Preserve integer codewords: PQ statistics are /4095, but ST 2094-10 trims
	// are /4096. Conflating those denominators changes the authored grade.
	struct St2094_10_Trim {
		std::uint16_t target_pq = 0, slope = 2048, offset = 2048, power = 2048;
		std::uint16_t chroma = 2048, saturation = 2048;
		bool          operator==(const St2094_10_Trim &) const = default;
	};
	struct St2094_10 {
		bool                         valid = false, refresh = false, has_statistics = false, has_active_area = false;
		std::uint16_t                min_pq = 0, max_pq = 0, avg_pq = 0;
		std::vector<St2094_10_Trim>  trims;
		std::array<std::uint16_t, 4> active_area{}; // left, right, top, bottom
		bool                         operator==(const St2094_10 &) const = default;
	};
	struct Vivid_Spline {
		std::uint8_t  mode = 0, mb = 0, strength = 0;
		std::uint16_t threshold = 0, delta1 = 0, delta2 = 0;
		bool          operator==(const Vivid_Spline &) const = default;
	};
	struct Vivid_Tone {
		std::uint16_t             target_pq = 0;
		bool                      base      = false;
		std::uint16_t             p = 0, a = 0, b = 0;
		std::uint8_t              m = 0, n = 0, k1 = 0, k2 = 0, k3 = 0, delta_mode = 0, delta = 0;
		std::vector<Vivid_Spline> splines;
		bool                      operator==(const Vivid_Tone &) const = default;
	};
	struct Hdr_Vivid {
		bool                      valid   = false;
		std::uint8_t              version = 0;
		std::uint16_t             min_pq = 0, avg_pq = 0, variance_pq = 0, max_pq = 0;
		std::vector<Vivid_Tone>   tones;
		std::vector<std::uint8_t> saturation;
		bool                      operator==(const Hdr_Vivid &) const = default;
	};
	struct Dynamic_HDR {
		std::optional<St2094_10> st2094_10;
		std::optional<Hdr_Vivid> vivid;
		std::uint32_t            sequence_id                           = 0;
		bool                     operator==(const Dynamic_HDR &) const = default;
	};

	namespace hdr_detail {
		struct Bits {
			std::span<const std::uint8_t> bytes;
			std::size_t                   pos = 0;
			bool                          ok  = true;
			std::uint32_t                 get(std::uint32_t n) {
				if (n > 32 || pos > bytes.size() * 8 || n > bytes.size() * 8 - pos) {
					ok = false;
					return 0;
				}
				std::uint32_t v = 0;
				for (std::uint32_t i = 0; i < n; ++i, ++pos) {
					v = (v << 1) | ((bytes[pos / 8] >> (7 - pos % 8)) & 1);
				}
				return v;
			}
			std::uint32_t ue() {
				std::uint32_t n = 0;
				while (ok && !get(1)) {
					if (++n >= 31) {
						ok = false;
						return 0;
					}
				}
				return ((1U << n) - 1) + get(n);
			}
			void zeros_to(std::size_t end) {
				if (end < pos || end > bytes.size() * 8) {
					ok = false;
					return;
				}
				while (ok && pos < end) {
					if (get(1)) {
						ok = false;
					}
				}
			}
			void align() { zeros_to((pos + 7) / 8 * 8); }
		};
	} // namespace hdr_detail

	// ETSI TS 103 572, tables 1-3. Input begins at app_identifier, and includes
	// the final reserved 0xff byte. Unknown extension levels are length-delimited.
	inline St2094_10 parse_st2094_10(std::span<const std::uint8_t> bytes, bool dvb1 = false) {
		St2094_10        h;
		hdr_detail::Bits r{bytes};
		if (r.ue() != 1 || r.ue() != 0) {
			return h;
		}
		h.refresh      = r.get(1);
		bool area_seen = false;
		if (h.refresh) {
			const auto count = r.ue();
			if (count > 254) {
				return h;
			}
			if (count) {
				r.align();
			}
			for (std::uint32_t i = 0; i < count && r.ok; ++i) {
				const auto length = r.ue();
				const auto level  = r.get(8);
				if (length > 1023 || area_seen) {
					return h;
				}
				const auto end = r.pos + length * 8;
				if (level == 1) {
					if (length != 5 || h.has_statistics) {
						return h;
					}
					h.has_statistics = true;
					h.min_pq         = r.get(12);
					h.max_pq         = r.get(12);
					h.avg_pq         = r.get(12);
					if (!(h.min_pq < h.avg_pq && h.avg_pq < h.max_pq)) {
						return h;
					}
				} else if (level == 2) {
					if (length != 11 || h.trims.size() >= 16) {
						return h;
					}
					St2094_10_Trim t;
					t.target_pq  = r.get(12);
					t.slope      = r.get(12);
					t.offset     = r.get(12);
					t.power      = r.get(12);
					t.chroma     = r.get(12);
					t.saturation = r.get(12);
					if (r.get(13) != 8191 || t.target_pq == 0) {
						return h;
					}
					for (const auto &old: h.trims) {
						if (old.target_pq == t.target_pq) {
							return h;
						}
					}
					h.trims.push_back(t);
				} else if (level == 5) {
					if (length != 7 || (!h.has_statistics && h.trims.empty())) {
						return h;
					}
					area_seen = h.has_active_area = true;
					for (auto &v: h.active_area) {
						v = r.get(13);
					}
				} else {
					if (end > bytes.size() * 8) {
						return h;
					}
					r.pos = end; // Reserved extension payload is opaque, not zero padding.
				}
				r.zeros_to(end);
			}
		}
		r.align();
		if (dvb1) { r.zeros_to(bytes.size() * 8);
		} else if (r.get(8) != 255 || r.pos != bytes.size() * 8) {
			r.ok = false;
		}
		h.valid = r.ok;
		return h;
	}

	// T/UWA 005.1-2022 table 10 (system_start_code through saturation gains).
	inline Hdr_Vivid parse_hdr_vivid(std::span<const std::uint8_t> bytes) {
		Hdr_Vivid        h;
		hdr_detail::Bits r{bytes};
		h.version = r.get(8);
		if (h.version != 1) {
			return h;
		}
		h.min_pq      = r.get(12);
		h.avg_pq      = r.get(12);
		h.variance_pq = r.get(12);
		h.max_pq      = r.get(12);
		if (h.min_pq > h.avg_pq || h.avg_pq > h.max_pq) {
			return h;
		}
		if (r.get(1)) {
			const auto count = r.get(1) + 1;
			for (std::uint32_t i = 0; i < count && r.ok; ++i) {
				Vivid_Tone t;
				t.target_pq = r.get(12);
				t.base      = r.get(1);
				if (t.base) {
					t.p          = r.get(14);
					t.m          = r.get(6);
					t.a          = r.get(10);
					t.b          = r.get(10);
					t.n          = r.get(6);
					t.k1         = r.get(2);
					t.k2         = r.get(2);
					t.k3         = r.get(4);
					t.delta_mode = r.get(3);
					t.delta      = r.get(7);
					if (t.k1 > 1 || t.k2 > 1 || (t.k3 != 1 && t.k3 != 2)) {
						return h;
					}
				}
				if (r.get(1)) {
					const auto splines = r.get(1) + 1;
					for (std::uint32_t j = 0; j < splines && r.ok; ++j) {
						Vivid_Spline s;
						s.mode = r.get(2);
						if (s.mode == 0 || s.mode == 2) {
							s.mb = r.get(8);
						}
						s.threshold = r.get(12);
						s.delta1    = r.get(10);
						s.delta2    = r.get(10);
						s.strength  = r.get(8);
						t.splines.push_back(s);
					}
				}
				h.tones.push_back(t);
			}
		}
		if (r.get(1)) {
			const auto count = r.get(3);
			for (std::uint32_t i = 0; i < count && r.ok; ++i) {
				h.saturation.push_back(r.get(8));
			}
		}
		// The enclosing SEI may add payload-extension bits; do not mistake those
		// for another field or require a particular extension alignment pattern.
		h.valid = r.ok;
		return h;
	}

	inline void parse_dynamic_hdr_t35(Dynamic_HDR &h, std::span<const std::uint8_t> data) {
		constexpr std::array<std::uint8_t, 8> st{0xb5, 0, 0x3b, 0, 0, 0, 0, 9};
		constexpr std::array<std::uint8_t, 8> dvb1{0xb5, 0, 0x31, 0x47, 0x41, 0x39, 0x34, 9};
		constexpr std::array<std::uint8_t, 5> vivid{0x26, 0, 4, 0, 5};
		if (data.size() >= st.size() && std::equal(st.begin(), st.end(), data.begin())) {
			h.st2094_10 = parse_st2094_10(data.subspan(st.size()));
		}
		if (data.size() >= dvb1.size() && std::equal(dvb1.begin(), dvb1.end(), data.begin())) {
			h.st2094_10 = parse_st2094_10(data.subspan(dvb1.size()), true);
		}
		if (data.size() >= vivid.size() && std::equal(vivid.begin(), vivid.end(), data.begin())) {
			h.vivid = parse_hdr_vivid(data.subspan(vivid.size()));
		}
	}

	// HEVC/VVC SEI EBSP, excluding the two-byte NAL header. Bound every payload
	// before dispatch so malformed sizes cannot read into a following message.
	template<class Visitor>
	inline bool visit_hdr_sei(std::span<const std::uint8_t> ebsp, Visitor visitor) {
		std::vector<std::uint8_t> rbsp;
		rbsp.reserve(ebsp.size());
		std::uint32_t zeros = 0;
		for (auto b: ebsp) {
			if (zeros == 2 && b == 3) {
				zeros = 0;
				continue;
			}
			rbsp.push_back(b);
			zeros = b == 0 ? zeros + 1 : 0;
		}
		std::size_t p      = 0;
		const auto  number = [&]() {
			std::size_t v = 0;
			while (p < rbsp.size()) {
				const auto b = rbsp[p++];
				v += b;
				if (b != 255) {
					break;
				}
			}
			return v;
		};
		while (p + 2 <= rbsp.size()) {
			const auto type = number(), size = number();
			if (size > rbsp.size() - p) {
				// Still identify a truncated registered message, so a damaged
				// dynamic grade cannot silently become ordinary static HDR.
				visitor(type, std::span<const std::uint8_t>{rbsp}.subspan(p));
				return false;
			}
			visitor(type, std::span<const std::uint8_t>{rbsp}.subspan(p, size));
			p += size;
		}
		return true;
	}
	inline Dynamic_HDR parse_dynamic_hdr_sei(std::span<const std::uint8_t> ebsp) {
		Dynamic_HDR h;
		if (!visit_hdr_sei(ebsp, [&](auto type, auto payload) {
				if (type == 4) {
					parse_dynamic_hdr_t35(h, payload);
				}
			})) {
			if (h.st2094_10) {
				h.st2094_10->valid = false;
			}
			if (h.vivid) {
				h.vivid->valid = false;
			}
		}
		return h;
	}

	inline void merge_dynamic_hdr(Dynamic_HDR &dst, const Dynamic_HDR &src) {
		if (src.st2094_10) {
			dst.st2094_10 = src.st2094_10;
		}
		if (src.vivid) {
			dst.vivid = src.vivid;
		}
	}

	// Run exactly once, AFTER POC ordering, not in NAL/decode order. Copies the
	// applicable ST metadata onto each picture; cache hits and random seeks then
	// need no mutable playback-time metadata state. Vivid remains picture-local.
	template<class Frames, class Presentation>
	inline void resolve_dynamic_hdr(Frames &frames, const Presentation &presentation) {
		std::optional<St2094_10>     active;
		std::optional<std::uint32_t> sequence;
		for (const auto &record: presentation) {
			auto &h = frames[record.decoded_frame_index_].dynamic_hdr_;
			if (sequence != h.sequence_id) {
				active.reset();
				sequence = h.sequence_id;
			}
			if (h.st2094_10) {
				if (!h.st2094_10->valid || h.st2094_10->refresh) {
					active = h.st2094_10;
				}
			}
			h.st2094_10 = active;
		}
	}
} // namespace video_editor::color
