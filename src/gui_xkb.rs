use std::ffi::CString;
use std::ffi::c_char;
use std::ptr;
use std::slice;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TextStroke {
    pub(crate) key: u32,
    pub(crate) modifiers: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextPlan {
    pub(crate) strokes: Vec<TextStroke>,
    pub(crate) shift_modifier: Option<u32>,
    pub(crate) control_modifier: Option<u32>,
    pub(crate) alt_modifier: Option<u32>,
    pub(crate) logo_modifier: Option<u32>,
}

#[repr(C)]
struct XkbContext {
    _private: [u8; 0],
}

#[repr(C)]
struct XkbKeymap {
    _private: [u8; 0],
}

#[cfg(test)]
#[repr(C)]
struct XkbRuleNames {
    rules: *const c_char,
    model: *const c_char,
    layout: *const c_char,
    variant: *const c_char,
    options: *const c_char,
}

#[link(name = "xkbcommon")]
unsafe extern "C" {
    fn xkb_context_new(flags: u32) -> *mut XkbContext;
    fn xkb_context_unref(context: *mut XkbContext);
    fn xkb_keymap_new_from_string(
        context: *mut XkbContext,
        string: *const c_char,
        format: u32,
        flags: u32,
    ) -> *mut XkbKeymap;
    fn xkb_keymap_unref(keymap: *mut XkbKeymap);
    fn xkb_keymap_min_keycode(keymap: *mut XkbKeymap) -> u32;
    fn xkb_keymap_max_keycode(keymap: *mut XkbKeymap) -> u32;
    fn xkb_keymap_num_layouts_for_key(keymap: *mut XkbKeymap, key: u32) -> u32;
    fn xkb_keymap_num_levels_for_key(keymap: *mut XkbKeymap, key: u32, layout: u32) -> u32;
    fn xkb_keymap_key_get_mods_for_level(
        keymap: *mut XkbKeymap,
        key: u32,
        layout: u32,
        level: u32,
        masks_out: *mut u32,
        masks_size: usize,
    ) -> usize;
    fn xkb_keymap_key_get_syms_by_level(
        keymap: *mut XkbKeymap,
        key: u32,
        layout: u32,
        level: u32,
        syms_out: *mut *const u32,
    ) -> i32;
    fn xkb_utf32_to_keysym(codepoint: u32) -> u32;
    fn xkb_keymap_mod_get_index(keymap: *mut XkbKeymap, name: *const c_char) -> u32;
    #[cfg(test)]
    fn xkb_keymap_new_from_names(
        context: *mut XkbContext,
        names: *const XkbRuleNames,
        flags: u32,
    ) -> *mut XkbKeymap;
    #[cfg(test)]
    fn xkb_keymap_get_as_string(keymap: *mut XkbKeymap, format: u32) -> *mut c_char;
}

struct Context(*mut XkbContext);

impl Drop for Context {
    fn drop(&mut self) {
        unsafe { xkb_context_unref(self.0) };
    }
}

struct Keymap(*mut XkbKeymap);

impl Drop for Keymap {
    fn drop(&mut self) {
        unsafe { xkb_keymap_unref(self.0) };
    }
}

pub(crate) fn plan_text(
    keymap_text: &str,
    layout_group: u32,
    text: &str,
    allow_compose: bool,
) -> Result<TextPlan, String> {
    let keymap_text = CString::new(keymap_text.trim_end_matches('\0'))
        .map_err(|_| "the client XKB keymap contains an interior NUL byte".to_string())?;
    let context = Context(unsafe { xkb_context_new(0) });
    if context.0.is_null() {
        return Err("libxkbcommon could not create a context".to_string());
    }
    let keymap =
        Keymap(unsafe { xkb_keymap_new_from_string(context.0, keymap_text.as_ptr(), 1, 0) });
    if keymap.0.is_null() {
        return Err(
            "libxkbcommon could not parse the keymap supplied to the Wayland client".to_string(),
        );
    }

    let mut strokes = Vec::with_capacity(text.chars().count());
    let find = |symbol| find_stroke(keymap.0, layout_group, symbol);
    for character in text.chars() {
        if let Some(stroke) = find(character_keysym(character)) {
            strokes.push(stroke);
        } else if allow_compose
            && let Some(dead_key) = spacing_accent_keysym(character).and_then(find)
            && let Some(space) = find(character_keysym(' '))
        {
            strokes.extend([dead_key, space]);
        } else {
            let hint = if allow_compose {
                r#". Apps that support Unicode hex input can use typeText with inputMethod "unicode-hex"."#
            } else {
                ""
            };
            return Err(format!(
                "character {} is not directly representable by the target client's active XKB layout group {layout_group}{hint}",
                quoted_character(character)
            ));
        }
    }
    Ok(TextPlan {
        strokes,
        shift_modifier: modifier_mask(keymap.0, b"Shift\0"),
        control_modifier: modifier_mask(keymap.0, b"Control\0"),
        alt_modifier: modifier_mask(keymap.0, b"Mod1\0"),
        logo_modifier: modifier_mask(keymap.0, b"Mod4\0"),
    })
}

fn modifier_mask(keymap: *mut XkbKeymap, name: &'static [u8]) -> Option<u32> {
    let index = unsafe { xkb_keymap_mod_get_index(keymap, name.as_ptr().cast()) };
    (index < u32::BITS).then(|| 1u32 << index)
}

fn character_keysym(character: char) -> u32 {
    // Newline and tab are control characters whose keyboard keysyms do not
    // round-trip through xkb_utf32_to_keysym in the same form.
    match character {
        '\n' | '\r' => 0xff0d, // XKB_KEY_Return
        '\t' => 0xff09,        // XKB_KEY_Tab
        character => unsafe { xkb_utf32_to_keysym(u32::from(character)) },
    }
}

fn spacing_accent_keysym(character: char) -> Option<u32> {
    Some(match character {
        '`' => 0xfe50, // XKB_KEY_dead_grave
        '^' => 0xfe52, // XKB_KEY_dead_circumflex
        '~' => 0xfe53, // XKB_KEY_dead_tilde
        _ => return None,
    })
}

fn find_stroke(
    keymap: *mut XkbKeymap,
    layout_group: u32,
    target_keysym: u32,
) -> Option<TextStroke> {
    if target_keysym == 0 {
        return None;
    }

    let mut best: Option<TextStroke> = None;
    let min_keycode = unsafe { xkb_keymap_min_keycode(keymap) };
    let max_keycode = unsafe { xkb_keymap_max_keycode(keymap) };
    for keycode in min_keycode..=max_keycode {
        if keycode < 8 {
            continue;
        }
        let layout_count = unsafe { xkb_keymap_num_layouts_for_key(keymap, keycode) };
        if layout_count == 0 {
            continue;
        }
        let layout = layout_group % layout_count;
        let level_count = unsafe { xkb_keymap_num_levels_for_key(keymap, keycode, layout) };
        for level in 0..level_count {
            let mut syms = ptr::null();
            let sym_count = unsafe {
                xkb_keymap_key_get_syms_by_level(keymap, keycode, layout, level, &mut syms)
            };
            if sym_count <= 0 || syms.is_null() {
                continue;
            }
            let matches =
                unsafe { slice::from_raw_parts(syms, sym_count as usize) }.contains(&target_keysym);
            if !matches {
                continue;
            }

            let mut masks = [0u32; 32];
            let mask_count = unsafe {
                xkb_keymap_key_get_mods_for_level(
                    keymap,
                    keycode,
                    layout,
                    level,
                    masks.as_mut_ptr(),
                    masks.len(),
                )
            };
            for modifiers in masks.into_iter().take(mask_count.min(masks.len())) {
                let candidate = TextStroke {
                    key: keycode - 8,
                    modifiers,
                };
                let score = (candidate.modifiers.count_ones(), candidate.key);
                if best.is_none_or(|current| score < (current.modifiers.count_ones(), current.key))
                {
                    best = Some(candidate);
                }
            }
        }
    }
    best
}

fn quoted_character(character: char) -> String {
    match character {
        '\n' => "newline".to_string(),
        '\r' => "carriage return".to_string(),
        '\t' => "tab".to_string(),
        character => format!("{character:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    fn keymap_for_layout(layout: &str) -> String {
        let layout = CString::new(layout).unwrap();
        let names = XkbRuleNames {
            rules: ptr::null(),
            model: ptr::null(),
            layout: layout.as_ptr(),
            variant: ptr::null(),
            options: ptr::null(),
        };
        let context = Context(unsafe { xkb_context_new(0) });
        assert!(!context.0.is_null());
        let keymap = Keymap(unsafe { xkb_keymap_new_from_names(context.0, &names, 0) });
        assert!(!keymap.0.is_null());
        let serialized = unsafe { xkb_keymap_get_as_string(keymap.0, 1) };
        assert!(!serialized.is_null());
        let result = unsafe { CStr::from_ptr(serialized) }
            .to_string_lossy()
            .into_owned();
        unsafe { libc::free(serialized.cast()) };
        result
    }

    #[test]
    fn text_plan_uses_the_target_keymap_instead_of_us_positions() {
        let german = keymap_for_layout("de");

        let plan = plan_text(&german, 0, "Az 0!?", false).unwrap();

        assert_eq!(
            plan.strokes,
            vec![
                TextStroke {
                    key: 30,
                    modifiers: 1,
                },
                TextStroke {
                    key: 21,
                    modifiers: 0,
                },
                TextStroke {
                    key: 57,
                    modifiers: 0,
                },
                TextStroke {
                    key: 11,
                    modifiers: 0,
                },
                TextStroke {
                    key: 2,
                    modifiers: 1,
                },
                TextStroke {
                    key: 12,
                    modifiers: 1,
                },
            ]
        );
        assert_ne!(
            plan.strokes[1].key, 44,
            "German z must not use the US Z position"
        );
        assert_ne!(
            plan.strokes[5].key, 53,
            "German ? must not use the US slash position"
        );
        assert_eq!(plan.shift_modifier, Some(1));
        assert_eq!(plan.control_modifier, Some(4));
    }

    #[test]
    fn text_plan_honors_the_active_layout_group() {
        let layouts = keymap_for_layout("us,de");

        let us = plan_text(&layouts, 0, "z?", false).unwrap();
        let german = plan_text(&layouts, 1, "z?", false).unwrap();

        assert_eq!(
            us.strokes
                .iter()
                .map(|stroke| stroke.key)
                .collect::<Vec<_>>(),
            [44, 53]
        );
        assert_eq!(
            german
                .strokes
                .iter()
                .map(|stroke| stroke.key)
                .collect::<Vec<_>>(),
            [21, 12]
        );
    }

    #[test]
    fn text_plan_can_enter_a_literal_caret_with_the_german_dead_key() {
        let german = keymap_for_layout("de");
        assert!(plan_text(&german, 0, "^", false).is_err());
        let plan = plan_text(&german, 0, "^", true).unwrap();
        assert_eq!(
            plan.strokes
                .iter()
                .map(|stroke| (stroke.key, stroke.modifiers))
                .collect::<Vec<_>>(),
            [(41, 0), (57, 0)]
        );
    }

    #[test]
    fn text_plan_rejects_characters_absent_from_the_active_keymap() {
        let german = keymap_for_layout("de");

        let error = plan_text(&german, 0, "🙂", true).unwrap_err();

        assert!(error.contains("not directly representable"));
        assert!(error.contains("unicode-hex"));
    }
}
