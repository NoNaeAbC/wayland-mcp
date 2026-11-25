# Rootless Xwayland feasibility

Rootless Xwayland is not implemented or validated. A working rootless desktop
requires an X window manager and a Wayland role adapter alongside Xwayland.

The `xwayland-shell-v1` protocol in `wayland-protocols`
defines a privileged surface role and an association serial shared with the X11
`WL_SURFACE_SERIAL` property. Its shell must be hidden from ordinary clients.
The current proxy denies that shell and manages xdg-shell surfaces; there is no
XWM or X11-to-Wayland role adapter. Adding the XML to the generator or exposing
the global does not implement those missing pieces.

A concrete implementation starts with a trusted launch channel, private X sockets
and authentication, a dedicated WM connection, and an XCB XWM. It must associate
X windows with Xwayland surfaces by serial, create separate upstream xdg roles,
translate configure/resize acknowledgements and preserve popup/transient ordering.
Selections and XDND must terminate at the same human/private transfer domains.
The console must receive only scoped GUI primitives, never the X endpoint.

The first working slice is two independently visible software-rendered X11
windows, each with its own inventory identity, capture, resize and model input.
Follow it with transient menus, map/unmap and crash cleanup; then selections,
agent-authored input recording/replay and GPU buffer sharing. Do not substitute
one desktop window or claim support from a successful server launch.
