#![allow(dead_code)]

use std::collections::HashMap;

use crate::gui_wayland_generated::GENERATED_HOOK_REQUESTS;
use crate::gui_wayland_generated::GENERATED_PROTOCOLS;
use crate::gui_wayland_generated::GeneratedHookRequestId;
use crate::gui_wayland_generated::GeneratedHookRequestSpec;
use crate::gui_wayland_generated::GeneratedInterfaceSpec;
use crate::gui_wayland_generated::GeneratedMessageSpec;
use crate::gui_wayland_generated::GeneratedRequestId;
use crate::gui_wayland_generated::find_generated_request;
use crate::gui_wayland_generated::find_generated_request_id;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GeneratedGlobalSpec {
    pub(crate) interface_name: &'static str,
    pub(crate) version: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct WaylandGeneratedProtocolRegistry {
    globals: Vec<GeneratedGlobalSpec>,
    requests_by_name: HashMap<
        String,
        (
            &'static GeneratedInterfaceSpec,
            &'static GeneratedMessageSpec,
        ),
    >,
    requests_by_id: HashMap<
        GeneratedRequestId,
        (
            &'static GeneratedInterfaceSpec,
            &'static GeneratedMessageSpec,
        ),
    >,
    hook_requests_by_id: HashMap<GeneratedHookRequestId, &'static GeneratedHookRequestSpec>,
    hook_requests_by_name: HashMap<&'static str, &'static GeneratedHookRequestSpec>,
}

impl WaylandGeneratedProtocolRegistry {
    pub(crate) fn new() -> Self {
        let mut globals = Vec::new();
        let mut requests_by_name = HashMap::new();
        let mut requests_by_id = HashMap::new();

        for protocol in GENERATED_PROTOCOLS {
            for interface in protocol.interfaces {
                if interface.is_global {
                    globals.push(GeneratedGlobalSpec {
                        interface_name: interface.name,
                        version: interface.version,
                    });
                }
                for request in interface.requests {
                    let request_name = format!("{}.{}", interface.name, request.name);
                    let request_id =
                        find_generated_request_id(&request_name).unwrap_or_else(|| {
                            panic!(
                                "generated request {request_name} is missing its GeneratedRequestId"
                            )
                        });
                    requests_by_name.insert(request_name, (interface, request));
                    requests_by_id.insert(request_id, (interface, request));
                }
            }
        }

        let mut hook_requests_by_id = HashMap::new();
        let mut hook_requests_by_name = HashMap::new();
        for hook in GENERATED_HOOK_REQUESTS {
            assert!(
                find_generated_request(hook.request_name).is_some(),
                "generated hook request {} does not exist in generated protocol metadata",
                hook.request_name
            );
            hook_requests_by_id.insert(hook.id, hook);
            hook_requests_by_name.insert(hook.request_name, hook);
        }

        Self {
            globals,
            requests_by_name,
            requests_by_id,
            hook_requests_by_id,
            hook_requests_by_name,
        }
    }

    pub(crate) fn globals(&self) -> &[GeneratedGlobalSpec] {
        &self.globals
    }

    pub(crate) fn request(
        &self,
        request_name: &str,
    ) -> Option<(
        &'static GeneratedInterfaceSpec,
        &'static GeneratedMessageSpec,
    )> {
        self.requests_by_name.get(request_name).copied()
    }

    pub(crate) fn request_by_id(
        &self,
        request_id: GeneratedRequestId,
    ) -> Option<(
        &'static GeneratedInterfaceSpec,
        &'static GeneratedMessageSpec,
    )> {
        self.requests_by_id.get(&request_id).copied()
    }

    pub(crate) fn hook_request_by_id(
        &self,
        id: GeneratedHookRequestId,
    ) -> Option<&'static GeneratedHookRequestSpec> {
        self.hook_requests_by_id.get(&id).copied()
    }

    pub(crate) fn hook_request_by_name(
        &self,
        request_name: &str,
    ) -> Option<&'static GeneratedHookRequestSpec> {
        self.hook_requests_by_name.get(request_name).copied()
    }

    pub(crate) fn hook_requests(
        &self,
    ) -> impl Iterator<Item = &'static GeneratedHookRequestSpec> + '_ {
        self.hook_requests_by_id.values().copied()
    }
}

#[derive(Debug)]
pub(crate) struct WaylandHookRegistry<H> {
    hooks: HashMap<GeneratedHookRequestId, Vec<H>>,
}

impl<H> WaylandHookRegistry<H> {
    pub(crate) fn register(&mut self, request_id: GeneratedHookRequestId, hook: H) {
        self.hooks.entry(request_id).or_default().push(hook);
    }

    pub(crate) fn hooks_for(&self, request_id: GeneratedHookRequestId) -> &[H] {
        self.hooks
            .get(&request_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub(crate) fn has_hooks(&self, request_id: GeneratedHookRequestId) -> bool {
        self.hooks
            .get(&request_id)
            .map(|hooks| !hooks.is_empty())
            .unwrap_or(false)
    }
}

impl<H> Default for WaylandHookRegistry<H> {
    fn default() -> Self {
        Self {
            hooks: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui_wayland_generated::GeneratedHookRequestId;

    #[test]
    fn registry_exposes_expected_globals() {
        let registry = WaylandGeneratedProtocolRegistry::new();
        let globals = registry
            .globals()
            .iter()
            .map(|global| global.interface_name)
            .collect::<Vec<_>>();
        assert!(globals.contains(&"wl_compositor"));
        assert!(globals.contains(&"zwp_linux_dmabuf_v1"));
        assert!(globals.contains(&"xdg_wm_base"));
    }

    #[test]
    fn registry_resolves_hook_requests_by_id_and_name() {
        let registry = WaylandGeneratedProtocolRegistry::new();
        let by_id = registry
            .hook_request_by_id(GeneratedHookRequestId::WlSurfaceCommit)
            .expect("hook request id");
        let by_name = registry
            .hook_request_by_name("wl_surface.commit")
            .expect("hook request name");
        assert_eq!(by_id.request_name, "wl_surface.commit");
        assert_eq!(by_id.request_name, by_name.request_name);
    }

    #[test]
    fn registry_resolves_requests_by_generated_id() {
        let registry = WaylandGeneratedProtocolRegistry::new();
        let (interface, request) = registry
            .request_by_id(GeneratedRequestId::WlSurfaceCommit)
            .expect("request id");
        assert_eq!(interface.name, "wl_surface");
        assert_eq!(request.name, "commit");
    }

    #[test]
    fn hook_registry_groups_hooks_by_generated_request_id() {
        let mut hooks = WaylandHookRegistry::default();
        hooks.register(GeneratedHookRequestId::WlSurfaceCommit, "hook-a");
        hooks.register(GeneratedHookRequestId::WlSurfaceCommit, "hook-b");
        hooks.register(GeneratedHookRequestId::WlSurfaceAttach, "hook-c");

        assert!(hooks.has_hooks(GeneratedHookRequestId::WlSurfaceCommit));
        assert_eq!(
            hooks.hooks_for(GeneratedHookRequestId::WlSurfaceCommit),
            ["hook-a", "hook-b"]
        );
        assert_eq!(
            hooks.hooks_for(GeneratedHookRequestId::WlSurfaceAttach),
            ["hook-c"]
        );
    }
}
