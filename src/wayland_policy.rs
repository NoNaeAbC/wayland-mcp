//! Capability policy is deliberately independent of generated wire decoders.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Exposure {
    Forward,
    Mediate,
    Deny,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct GlobalPolicy {
    pub(crate) exposure: Exposure,
    pub(crate) max_version: u32,
}

pub(crate) fn global(interface: &str) -> GlobalPolicy {
    use Exposure::*;
    let (exposure, max_version) = match interface {
        "wl_compositor" => (Forward, 7),
        "wl_subcompositor" => (Forward, 1),
        "wl_shm" => (Forward, 2),
        "xdg_wm_base" => (Forward, 7),
        "wp_viewporter" => (Forward, 1),
        "wp_fractional_scale_manager_v1" => (Forward, 1),
        "wl_data_device_manager" => (Mediate, 3),
        "wl_seat" => (Mediate, 10),
        "zwp_relative_pointer_manager_v1" => (Mediate, 1),
        "zwp_pointer_constraints_v1" => (Mediate, 1),
        "wl_output" => (Mediate, 4),
        "zwp_linux_dmabuf_v1" => (Mediate, 6),
        "wp_linux_drm_syncobj_manager_v1" => (Mediate, 1),
        "wp_color_manager_v1" => (Mediate, 3),
        "wp_presentation" => (Mediate, 2),
        "wp_tearing_control_manager_v1" => (Forward, 1),
        "wp_fifo_manager_v1" => (Forward, 1),
        // Remains closed until the local data-device service is installed.
        // Merely adding a decoder must never expose a host clipboard capability.
        _ => (Deny, 0),
    };
    GlobalPolicy {
        exposure,
        max_version,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_support_never_grants_an_unreviewed_capability() {
        for name in [
            "ext_data_control_manager_v1",
            "zwp_primary_selection_device_manager_v1",
            "zwlr_screencopy_manager_v1",
            "ext_image_copy_capture_manager_v1",
            "zwlr_foreign_toplevel_manager_v1",
            "zwlr_virtual_pointer_manager_v1",
            "zwp_virtual_keyboard_manager_v1",
            "ext_session_lock_manager_v1",
            "wp_security_context_manager_v1",
            "xwayland_shell_v1",
            "future_decoded_protocol",
        ] {
            assert_eq!(global(name).exposure, Exposure::Deny, "{name}");
        }
    }
}
