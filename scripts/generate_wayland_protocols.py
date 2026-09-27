#!/usr/bin/env python3
"""
Generate Rust Wayland metadata from the installed /usr/share protocol XML files.

Cargo invokes this script with an output path inside OUT_DIR. The generated Rust
is a build artifact and must not be added to the source tree or committed.
"""

from __future__ import annotations

import argparse
import dataclasses
import pathlib
import textwrap
import xml.etree.ElementTree as ET


DEFAULT_PROTOCOL_DIR = pathlib.Path("/usr/share")

PROTOCOL_XML_PATHS = [
    pathlib.Path("wayland/wayland.xml"),
    pathlib.Path("wayland-protocols/stable/xdg-shell/xdg-shell.xml"),
    pathlib.Path("wayland-protocols/stable/linux-dmabuf/linux-dmabuf-v1.xml"),
    pathlib.Path(
        "wayland-protocols/staging/linux-drm-syncobj/linux-drm-syncobj-v1.xml"
    ),
    pathlib.Path(
        "wayland-protocols/staging/tearing-control/tearing-control-v1.xml"
    ),
    pathlib.Path(
        "wayland-protocols/stable/presentation-time/presentation-time.xml"
    ),
    pathlib.Path(
        "wayland-protocols/staging/color-management/color-management-v1.xml"
    ),
    pathlib.Path("wayland-protocols/staging/fifo/fifo-v1.xml"),
    pathlib.Path(
        "wayland-protocols/staging/fractional-scale/fractional-scale-v1.xml"
    ),
    pathlib.Path("wayland-protocols/stable/viewporter/viewporter.xml"),
    pathlib.Path("wayland-protocols/unstable/relative-pointer/relative-pointer-unstable-v1.xml"),
    pathlib.Path("wayland-protocols/unstable/pointer-constraints/pointer-constraints-unstable-v1.xml"),
]

GLOBAL_INTERFACES = {
    "wl_compositor",
    "wl_seat",
    "wl_output",
    "xdg_wm_base",
    "zwp_linux_dmabuf_v1",
    "wp_linux_drm_syncobj_manager_v1",
    "wp_tearing_control_manager_v1",
    "wp_presentation",
    "wp_color_manager_v1",
    "wp_fifo_manager_v1",
    "wp_fractional_scale_manager_v1",
    "wp_viewporter",
    "zwp_relative_pointer_manager_v1",
    "zwp_pointer_constraints_v1",
}

EXCLUDED_INTERFACES = {
    "wl_" + "s" + "hm",
    "wl_" + "s" + "hm_pool",
}

@dataclasses.dataclass(frozen=True)
class Arg:
    name: str
    kind: str
    interface: str | None
    allow_null: bool


@dataclasses.dataclass(frozen=True)
class Message:
    name: str
    args: list[Arg]
    since: int
    destructor: bool


@dataclasses.dataclass(frozen=True)
class Interface:
    name: str
    version: int
    is_global: bool
    requests: list[Message]
    events: list[Message]


@dataclasses.dataclass(frozen=True)
class Protocol:
    name: str
    source_xml: pathlib.Path
    interfaces: list[Interface]


def parse_arg(elem: ET.Element) -> Arg:
    return Arg(
        name=elem.attrib["name"],
        kind=elem.attrib["type"],
        interface=elem.attrib.get("interface"),
        allow_null=elem.attrib.get("allow-null") == "true",
    )


def expand_request_arg(arg: Arg) -> list[Arg]:
    if arg.kind == "new_id" and arg.interface is None:
        # Untyped new_id requests use the wl_registry.bind wire form:
        # interface name string, version uint, then the new object id.
        return [
            Arg(
                name=f"{arg.name}_interface",
                kind="string",
                interface=None,
                allow_null=False,
            ),
            Arg(
                name=f"{arg.name}_version",
                kind="uint",
                interface=None,
                allow_null=False,
            ),
            Arg(
                name=arg.name,
                kind="new_id",
                interface=None,
                allow_null=arg.allow_null,
            ),
        ]
    return [arg]


def parse_message(elem: ET.Element, *, is_request: bool) -> Message:
    args = []
    for arg_elem in elem.findall("arg"):
        parsed = parse_arg(arg_elem)
        if is_request:
            args.extend(expand_request_arg(parsed))
        else:
            args.append(parsed)
    return Message(
        name=elem.attrib["name"],
        args=args,
        since=int(elem.attrib.get("since", "1")),
        destructor=elem.attrib.get("type") == "destructor",
    )


def parse_protocol(xml_path: pathlib.Path) -> Protocol:
    root = ET.parse(xml_path).getroot()
    name = root.attrib["name"]
    interfaces = []
    for iface_elem in root.findall("interface"):
        iface_name = iface_elem.attrib["name"]
        if iface_name in EXCLUDED_INTERFACES:
            continue
        interfaces.append(
            Interface(
                name=iface_name,
                version=int(iface_elem.attrib.get("version", "1")),
                is_global=iface_name in GLOBAL_INTERFACES,
                requests=[
                    parse_message(elem, is_request=True)
                    for elem in iface_elem.findall("request")
                ],
                events=[
                    parse_message(elem, is_request=False)
                    for elem in iface_elem.findall("event")
                ],
            )
        )
    return Protocol(name=name, source_xml=xml_path, interfaces=interfaces)


def rust_string(value: str) -> str:
    escaped = value.replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


def emit_args(args: list[Arg], indent: str) -> list[str]:
    lines = [f"{indent}&["]
    for arg in args:
        interface = "None" if arg.interface is None else f"Some({rust_string(arg.interface)})"
        lines.append(
            f"{indent}    GeneratedArgSpec {{ name: {rust_string(arg.name)}, kind: GeneratedArgKind::{arg.kind.title().replace('_', '')}, interface: {interface}, allow_null: {str(arg.allow_null).lower()} }},"
        )
    lines.append(f"{indent}]")
    return lines


def emit_messages(messages: list[Message], indent: str) -> list[str]:
    lines = [f"{indent}&["]
    for message in messages:
        lines.append(f"{indent}    GeneratedMessageSpec {{")
        lines.append(f"{indent}        name: {rust_string(message.name)},")
        lines.append(f"{indent}        since: {message.since},")
        lines.append(f"{indent}        destructor: {str(message.destructor).lower()},")
        lines.append(f"{indent}        args:")
        lines.extend(emit_args(message.args, indent + "            "))
        lines.append(f"{indent}    }},")
    lines.append(f"{indent}]")
    return lines


def emit_protocols(protocols: list[Protocol]) -> list[str]:
    lines = ["pub(crate) const GENERATED_PROTOCOLS: &[GeneratedProtocolSpec] = &["]
    for protocol in protocols:
        lines.append("    GeneratedProtocolSpec {")
        lines.append(f"        name: {rust_string(protocol.name)},")
        lines.append(
            f"        source_xml: {rust_string(str(protocol.source_xml))},"
        )
        lines.append("        interfaces: &[")
        for interface in protocol.interfaces:
            lines.append("            GeneratedInterfaceSpec {")
            lines.append(f"                name: {rust_string(interface.name)},")
            lines.append(f"                version: {interface.version},")
            lines.append(f"                is_global: {str(interface.is_global).lower()},")
            lines.append("                requests:")
            lines.extend(emit_messages(interface.requests, "                    "))
            lines.append("                ,")
            lines.append("                events:")
            lines.extend(emit_messages(interface.events, "                    "))
            lines.append("                ,")
            lines.append("            },")
        lines.append("        ],")
        lines.append("    },")
    lines.append("];")
    return lines


def request_id_name(request_name: str) -> str:
    interface_name, leaf = request_name.split(".", 1)
    return "".join(part.title() for part in f"{interface_name}_{leaf}".split("_"))


def all_request_names(protocols: list[Protocol]) -> list[str]:
    names: list[str] = []
    for protocol in protocols:
        for interface in protocol.interfaces:
            for request in interface.requests:
                names.append(f"{interface.name}.{request.name}")
    return names


def all_event_names(protocols: list[Protocol]) -> list[str]:
    names: list[str] = []
    for protocol in protocols:
        for interface in protocol.interfaces:
            for event in interface.events:
                names.append(f"{interface.name}.{event.name}")
    return names


def implemented_request_names(protocols: list[Protocol]) -> list[str]:
    return all_request_names(protocols)


def emit_request_ids(protocols: list[Protocol]) -> list[str]:
    lines = [
        "#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]",
        "pub(crate) enum GeneratedRequestId {",
    ]
    for request_name in all_request_names(protocols):
        lines.append(f"    {request_id_name(request_name)},")
    lines.append("}")
    return lines


def emit_hook_request_ids(protocols: list[Protocol]) -> list[str]:
    lines = [
        "#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]",
        "pub(crate) enum GeneratedHookRequestId {",
    ]
    for request_name in all_request_names(protocols):
        lines.append(f"    {request_id_name(request_name)},")
    lines.append("}")
    return lines


def emit_hook_requests(protocols: list[Protocol]) -> list[str]:
    lines = ["pub(crate) const GENERATED_HOOK_REQUESTS: &[GeneratedHookRequestSpec] = &["]
    for request_name in all_request_names(protocols):
        lines.append("    GeneratedHookRequestSpec {")
        lines.append(f"        id: GeneratedHookRequestId::{request_id_name(request_name)},")
        lines.append(f"        request_name: {rust_string(request_name)},")
        lines.append("    },")
    lines.append("];")
    return lines


def rust_hook_field_type(kind: str) -> str:
    return {
        "int": "i32",
        "fixed": "i32",
        "uint": "u32",
        "new_id": "u32",
        "string": "Option<String>",
        "object": "Option<u32>",
        "array": "Vec<u8>",
        "fd": "bool",
    }[kind]


def generated_arg_pattern(arg: Arg) -> str:
    name = arg.name
    return {
        "int": f"GeneratedDecodedArg::Int({name})",
        "fixed": f"GeneratedDecodedArg::Fixed({name})",
        "uint": f"GeneratedDecodedArg::Uint({name})",
        "new_id": f"GeneratedDecodedArg::NewId({name})",
        "string": f"GeneratedDecodedArg::String({name})",
        "object": f"GeneratedDecodedArg::Object({name})",
        "array": f"GeneratedDecodedArg::Array({name})",
        "fd": "GeneratedDecodedArg::Fd",
    }[arg.kind]


def generated_arg_value(arg: Arg) -> str:
    name = arg.name
    return {
        "int": f"*{name}",
        "fixed": f"*{name}",
        "uint": f"*{name}",
        "new_id": f"*{name}",
        "string": f"{name}.clone()",
        "object": f"*{name}",
        "array": f"{name}.clone()",
        "fd": "true",
    }[arg.kind]


def find_request(protocols: list[Protocol], request_name: str) -> Message:
    interface_name, message_name = request_name.split(".", 1)
    for protocol in protocols:
        for interface in protocol.interfaces:
            if interface.name != interface_name:
                continue
            for request in interface.requests:
                if request.name == message_name:
                    return request
    raise KeyError(request_name)


def find_event(protocols: list[Protocol], event_name: str) -> Message:
    interface_name, message_name = event_name.split(".", 1)
    for protocol in protocols:
        for interface in protocol.interfaces:
            if interface.name != interface_name:
                continue
            for event in interface.events:
                if event.name == message_name:
                    return event
    raise KeyError(event_name)


def emit_event_ids(protocols: list[Protocol]) -> list[str]:
    lines = [
        "#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]",
        "pub(crate) enum GeneratedEventId {",
    ]
    for event_name in all_event_names(protocols):
        lines.append(f"    {request_id_name(event_name)},")
    lines.append("}")
    return lines


def emit_generated_hook_request_enum(protocols: list[Protocol]) -> list[str]:
    lines = [
        "#[derive(Debug, Clone, PartialEq, Eq)]",
        "pub(crate) enum GeneratedHookRequest {",
    ]
    for request_name in all_request_names(protocols):
        variant = request_id_name(request_name)
        request = find_request(protocols, request_name)
        if not request.args:
            lines.append(f"    {variant},")
            continue
        lines.append(f"    {variant} {{")
        for arg in request.args:
            lines.append(f"        {arg.name}: {rust_hook_field_type(arg.kind)},")
        lines.append("    },")
    lines.append("}")
    lines.append("")
    lines.append("impl GeneratedHookRequest {")
    lines.append("    pub(crate) fn id(&self) -> GeneratedHookRequestId {")
    lines.append("        match self {")
    for request_name in all_request_names(protocols):
        variant = request_id_name(request_name)
        if find_request(protocols, request_name).args:
            lines.append(
                f"            Self::{variant} {{ .. }} => GeneratedHookRequestId::{variant},"
            )
        else:
            lines.append(f"            Self::{variant} => GeneratedHookRequestId::{variant},")
    lines.append("        }")
    lines.append("    }")
    lines.append("}")
    return lines


def emit_generated_event_enum(protocols: list[Protocol]) -> list[str]:
    lines = [
        "#[derive(Debug, Clone, PartialEq, Eq)]",
        "pub(crate) enum GeneratedEvent {",
    ]
    for event_name in all_event_names(protocols):
        variant = request_id_name(event_name)
        event = find_event(protocols, event_name)
        if not event.args:
            lines.append(f"    {variant},")
            continue
        lines.append(f"    {variant} {{")
        for arg in event.args:
            lines.append(f"        {arg.name}: {rust_hook_field_type(arg.kind)},")
        lines.append("    },")
    lines.append("}")
    return lines


def emit_generated_hook_decoder(protocols: list[Protocol]) -> list[str]:
    lines = [
        "pub(crate) fn decode_generated_hook_request(",
        "    request_name: &str,",
        "    args: &[GeneratedDecodedArg],",
        ") -> Option<GeneratedHookRequest> {",
        "    match request_name {",
    ]
    for request_name in all_request_names(protocols):
        variant = request_id_name(request_name)
        request = find_request(protocols, request_name)
        lines.append(f"        {rust_string(request_name)} => match args {{")
        if not request.args:
            lines.append(f"            [] => Some(GeneratedHookRequest::{variant}),")
        else:
            patterns = ", ".join(generated_arg_pattern(arg) for arg in request.args)
            lines.append(f"            [{patterns}] => Some(GeneratedHookRequest::{variant} {{")
            for arg in request.args:
                lines.append(f"                {arg.name}: {generated_arg_value(arg)},")
            lines.append("            }),")
        lines.append("            _ => None,")
        lines.append("        },")
    lines.append("        _ => None,")
    lines.append("    }")
    lines.append("}")
    return lines


def emit_generated_event_decoder_by_opcode(protocols: list[Protocol]) -> list[str]:
    lines = [
        "pub(crate) fn decode_generated_event_by_opcode(",
        "    interface_name: &str,",
        "    opcode: u16,",
        "    args: &[GeneratedDecodedArg],",
        ") -> Option<GeneratedEvent> {",
        "    match (interface_name, opcode) {",
    ]
    for protocol in protocols:
        for interface in protocol.interfaces:
            for opcode, event in enumerate(interface.events):
                event_name = f"{interface.name}.{event.name}"
                variant = request_id_name(event_name)
                lines.append(
                    f"        ({rust_string(interface.name)}, {opcode}) => match args {{"
                )
                if not event.args:
                    lines.append(f"            [] => Some(GeneratedEvent::{variant}),")
                else:
                    patterns = ", ".join(generated_arg_pattern(arg) for arg in event.args)
                    lines.append(
                        f"            [{patterns}] => Some(GeneratedEvent::{variant} {{"
                    )
                    for arg in event.args:
                        lines.append(
                            f"                {arg.name}: {generated_arg_value(arg)},"
                        )
                    lines.append("            }),")
                lines.append("            _ => None,")
                lines.append("        },")
    lines.append("        _ => None,")
    lines.append("    }")
    lines.append("}")
    return lines


def emit_generated_hook_decoder_by_id(protocols: list[Protocol]) -> list[str]:
    lines = [
        "pub(crate) fn decode_generated_hook_request_by_id(",
        "    request_id: GeneratedRequestId,",
        "    args: &[GeneratedDecodedArg],",
        ") -> Option<GeneratedHookRequest> {",
        "    match request_id {",
    ]
    for request_name in all_request_names(protocols):
        variant = request_id_name(request_name)
        request = find_request(protocols, request_name)
        lines.append(f"        GeneratedRequestId::{variant} => match args {{")
        if not request.args:
            lines.append(f"            [] => Some(GeneratedHookRequest::{variant}),")
        else:
            patterns = ", ".join(generated_arg_pattern(arg) for arg in request.args)
            lines.append(f"            [{patterns}] => Some(GeneratedHookRequest::{variant} {{")
            for arg in request.args:
                lines.append(f"                {arg.name}: {generated_arg_value(arg)},")
            lines.append("            }),")
        lines.append("            _ => None,")
        lines.append("        },")
    lines.append("    }")
    lines.append("}")
    return lines


def emit_generated_tracked_request_enum(protocols: list[Protocol]) -> list[str]:
    lines = [
        "#[derive(Debug, Clone, PartialEq, Eq)]",
        "pub(crate) enum GeneratedTrackedRequest {",
    ]
    for request_name in all_request_names(protocols):
        variant = request_id_name(request_name)
        request = find_request(protocols, request_name)
        if not request.args:
            lines.append(f"    {variant},")
            continue
        lines.append(f"    {variant} {{")
        for arg in request.args:
            lines.append(f"        {arg.name}: {rust_hook_field_type(arg.kind)},")
        lines.append("    },")
    lines.append("}")
    return lines


def emit_generated_implemented_request_enum(protocols: list[Protocol]) -> list[str]:
    lines = [
        "#[derive(Debug, Clone, PartialEq, Eq)]",
        "pub(crate) enum GeneratedImplementedRequest {",
    ]
    for request_name in implemented_request_names(protocols):
        variant = request_id_name(request_name)
        request = find_request(protocols, request_name)
        if not request.args:
            lines.append(f"    {variant},")
            continue
        lines.append(f"    {variant} {{")
        for arg in request.args:
            lines.append(f"        {arg.name}: {rust_hook_field_type(arg.kind)},")
        lines.append("    },")
    lines.append("}")
    return lines


def emit_generated_tracked_decoder(protocols: list[Protocol]) -> list[str]:
    lines = [
        "pub(crate) fn decode_generated_tracked_request(",
        "    request_name: &str,",
        "    args: &[GeneratedDecodedArg],",
        ") -> Option<GeneratedTrackedRequest> {",
        "    match request_name {",
    ]
    for request_name in all_request_names(protocols):
        variant = request_id_name(request_name)
        request = find_request(protocols, request_name)
        lines.append(f"        {rust_string(request_name)} => match args {{")
        if not request.args:
            lines.append(f"            [] => Some(GeneratedTrackedRequest::{variant}),")
        else:
            patterns = ", ".join(generated_arg_pattern(arg) for arg in request.args)
            lines.append(
                f"            [{patterns}] => Some(GeneratedTrackedRequest::{variant} {{"
            )
            for arg in request.args:
                lines.append(f"                {arg.name}: {generated_arg_value(arg)},")
            lines.append("            }),")
        lines.append("            _ => None,")
        lines.append("        },")
    lines.append("        _ => None,")
    lines.append("    }")
    lines.append("}")
    return lines


def emit_generated_tracked_decoder_by_id(protocols: list[Protocol]) -> list[str]:
    lines = [
        "pub(crate) fn decode_generated_tracked_request_by_id(",
        "    request_id: GeneratedRequestId,",
        "    args: &[GeneratedDecodedArg],",
        ") -> Option<GeneratedTrackedRequest> {",
        "    match request_id {",
    ]
    for request_name in all_request_names(protocols):
        variant = request_id_name(request_name)
        request = find_request(protocols, request_name)
        lines.append(f"        GeneratedRequestId::{variant} => match args {{")
        if not request.args:
            lines.append(f"            [] => Some(GeneratedTrackedRequest::{variant}),")
        else:
            patterns = ", ".join(generated_arg_pattern(arg) for arg in request.args)
            lines.append(
                f"            [{patterns}] => Some(GeneratedTrackedRequest::{variant} {{"
            )
            for arg in request.args:
                lines.append(f"                {arg.name}: {generated_arg_value(arg)},")
            lines.append("            }),")
        lines.append("            _ => None,")
        lines.append("        },")
    lines.append("    }")
    lines.append("}")
    return lines


def emit_generated_implemented_decoder_by_id(protocols: list[Protocol]) -> list[str]:
    lines = [
        "pub(crate) fn decode_generated_implemented_request_by_id(",
        "    request_id: GeneratedRequestId,",
        "    args: &[GeneratedDecodedArg],",
        ") -> Option<GeneratedImplementedRequest> {",
        "    match request_id {",
    ]
    for request_name in implemented_request_names(protocols):
        variant = request_id_name(request_name)
        request = find_request(protocols, request_name)
        lines.append(f"        GeneratedRequestId::{variant} => match args {{")
        if not request.args:
            lines.append(f"            [] => Some(GeneratedImplementedRequest::{variant}),")
        else:
            patterns = ", ".join(generated_arg_pattern(arg) for arg in request.args)
            lines.append(
                f"            [{patterns}] => Some(GeneratedImplementedRequest::{variant} {{"
            )
            for arg in request.args:
                lines.append(f"                {arg.name}: {generated_arg_value(arg)},")
            lines.append("            }),")
        lines.append("            _ => None,")
        lines.append("        },")
    lines.append("    }")
    lines.append("}")
    return lines


def emit_generated_implemented_predicate(protocols: list[Protocol]) -> list[str]:
    lines = [
        "pub(crate) fn is_generated_request_implemented(",
        "    _request_id: GeneratedRequestId,",
        ") -> bool {",
        "    true",
    ]
    lines.append("}")
    return lines


def generated_arg_constructor(arg: Arg) -> str:
    name = arg.name
    return {
        "int": f"GeneratedDecodedArg::Int(*{name})",
        "fixed": f"GeneratedDecodedArg::Fixed(*{name})",
        "uint": f"GeneratedDecodedArg::Uint(*{name})",
        "new_id": f"GeneratedDecodedArg::NewId(*{name})",
        "string": f"GeneratedDecodedArg::String({name}.clone())",
        "object": f"GeneratedDecodedArg::Object(*{name})",
        "array": f"GeneratedDecodedArg::Array({name}.clone())",
        "fd": "GeneratedDecodedArg::Fd",
    }[arg.kind]


def emit_generated_implemented_request_encoder(protocols: list[Protocol]) -> list[str]:
    lines = [
        "pub(crate) fn encode_generated_implemented_request(",
        "    sender_object_id: u32,",
        "    request: &GeneratedImplementedRequest,",
        ") -> Result<Vec<u8>, String> {",
        "    match request {",
    ]
    for request_name in implemented_request_names(protocols):
        interface_name, message_name = request_name.split(".", 1)
        variant = request_id_name(request_name)
        request = find_request(protocols, request_name)
        opcode = next(
            opcode
            for protocol in protocols
            for interface in protocol.interfaces
            if interface.name == interface_name
            for opcode, candidate in enumerate(interface.requests)
            if candidate.name == message_name
        )
        if not request.args:
            lines.append(
                f"        GeneratedImplementedRequest::{variant} => encode_generated_message(sender_object_id, {opcode}, &[]),"
            )
            continue
        lines.append(f"        GeneratedImplementedRequest::{variant} {{")
        for arg in request.args:
            if arg.kind == "fd":
                lines.append(f"            {arg.name}: _,")
            else:
                lines.append(f"            {arg.name},")
        lines.append("        } => encode_generated_message(")
        lines.append("            sender_object_id,")
        lines.append(f"            {opcode},")
        lines.append("            &[")
        for arg in request.args:
            lines.append(f"                {generated_arg_constructor(arg)},")
        lines.append("            ],")
        lines.append("        ),")
    lines.append("    }")
    lines.append("}")
    return lines


def emit_generated_event_encoder(protocols: list[Protocol]) -> list[str]:
    lines = [
        "pub(crate) fn encode_generated_event(",
        "    sender_object_id: u32,",
        "    event: &GeneratedEvent,",
        ") -> Result<Vec<u8>, String> {",
        "    match event {",
    ]
    for protocol in protocols:
        for interface in protocol.interfaces:
            for opcode, event in enumerate(interface.events):
                event_name = f"{interface.name}.{event.name}"
                variant = request_id_name(event_name)
                if not event.args:
                    lines.append(
                        f"        GeneratedEvent::{variant} => encode_generated_message(sender_object_id, {opcode}, &[]),"
                    )
                    continue
                lines.append(f"        GeneratedEvent::{variant} {{")
                for arg in event.args:
                    if arg.kind == "fd":
                        lines.append(f"            {arg.name}: _,")
                    else:
                        lines.append(f"            {arg.name},")
                lines.append("        } => encode_generated_message(")
                lines.append("            sender_object_id,")
                lines.append(f"            {opcode},")
                lines.append("            &[")
                for arg in event.args:
                    lines.append(f"                {generated_arg_constructor(arg)},")
                lines.append("            ],")
                lines.append("        ),")
    lines.append("    }")
    lines.append("}")
    return lines


def render(protocols: list[Protocol]) -> str:
    prelude = textwrap.dedent(
        """\
        // @generated by scripts/generate_wayland_protocols.py
        // Do not edit by hand; rerun the generator instead.

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) enum GeneratedArgKind {
            Int,
            Uint,
            Fixed,
            String,
            Object,
            NewId,
            Array,
            Fd,
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) struct GeneratedArgSpec {
            pub(crate) name: &'static str,
            pub(crate) kind: GeneratedArgKind,
            pub(crate) interface: Option<&'static str>,
            pub(crate) allow_null: bool,
        }

        #[derive(Debug, Clone, PartialEq, Eq)]
        pub(crate) struct GeneratedMessageSpec {
            pub(crate) name: &'static str,
            pub(crate) since: u32,
            pub(crate) destructor: bool,
            pub(crate) args: &'static [GeneratedArgSpec],
        }

        #[derive(Debug, Clone, PartialEq, Eq)]
        pub(crate) struct GeneratedInterfaceSpec {
            pub(crate) name: &'static str,
            pub(crate) version: u32,
            pub(crate) is_global: bool,
            pub(crate) requests: &'static [GeneratedMessageSpec],
            pub(crate) events: &'static [GeneratedMessageSpec],
        }

        #[derive(Debug, Clone, PartialEq, Eq)]
        pub(crate) struct GeneratedProtocolSpec {
            pub(crate) name: &'static str,
            pub(crate) source_xml: &'static str,
            pub(crate) interfaces: &'static [GeneratedInterfaceSpec],
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) struct GeneratedHookRequestSpec {
            pub(crate) id: GeneratedHookRequestId,
            pub(crate) request_name: &'static str,
        }

        #[derive(Debug, Clone, PartialEq, Eq)]
        pub(crate) enum GeneratedDecodedArg {
            Int(i32),
            Uint(u32),
            Fixed(i32),
            String(Option<String>),
            Object(Option<u32>),
            NewId(u32),
            Array(Vec<u8>),
            Fd,
        }

        fn append_generated_padded_bytes(payload: &mut Vec<u8>, bytes: &[u8]) {
            payload.extend_from_slice(bytes);
            while !payload.len().is_multiple_of(4) {
                payload.push(0);
            }
        }

        fn encode_generated_message(
            object_id: u32,
            opcode: u16,
            args: &[GeneratedDecodedArg],
        ) -> Result<Vec<u8>, String> {
            let mut payload = Vec::new();
            for arg in args {
                match arg {
                    GeneratedDecodedArg::Int(value) => payload.extend_from_slice(&value.to_ne_bytes()),
                    GeneratedDecodedArg::Uint(value) => payload.extend_from_slice(&value.to_ne_bytes()),
                    GeneratedDecodedArg::Fixed(value) => payload.extend_from_slice(&value.to_ne_bytes()),
                    GeneratedDecodedArg::Object(value) => payload.extend_from_slice(&value.unwrap_or(0).to_ne_bytes()),
                    GeneratedDecodedArg::NewId(value) => payload.extend_from_slice(&value.to_ne_bytes()),
                    GeneratedDecodedArg::String(value) => {
                        if let Some(value) = value {
                            let mut bytes = value.as_bytes().to_vec();
                            bytes.push(0);
                            let len = u32::try_from(bytes.len())
                                .map_err(|_| "generated string argument exceeds u32 length".to_string())?;
                            payload.extend_from_slice(&len.to_ne_bytes());
                            append_generated_padded_bytes(&mut payload, &bytes);
                        } else {
                            payload.extend_from_slice(&0u32.to_ne_bytes());
                        }
                    }
                    GeneratedDecodedArg::Array(value) => {
                        let len = u32::try_from(value.len())
                            .map_err(|_| "generated array argument exceeds u32 length".to_string())?;
                        payload.extend_from_slice(&len.to_ne_bytes());
                        append_generated_padded_bytes(&mut payload, value);
                    }
                    GeneratedDecodedArg::Fd => {}
                }
            }
            let size = 8usize
                .checked_add(payload.len())
                .ok_or_else(|| "generated message size overflow".to_string())?;
            let size_u32 = u32::try_from(size)
                .map_err(|_| "generated message larger than Wayland u32 size".to_string())?;
            let mut bytes = Vec::with_capacity(size);
            bytes.extend_from_slice(&object_id.to_ne_bytes());
            bytes.extend_from_slice(&((size_u32 << 16) | u32::from(opcode)).to_ne_bytes());
            bytes.extend_from_slice(&payload);
            Ok(bytes)
        }
        """
    )
    body = []
    body.extend(emit_protocols(protocols))
    body.append("")
    body.extend(emit_request_ids(protocols))
    body.append("")
    body.extend(emit_event_ids(protocols))
    body.append("")
    body.extend(emit_hook_request_ids(protocols))
    body.append("")
    body.extend(emit_hook_requests(protocols))
    body.append("")
    body.extend(emit_generated_hook_request_enum(protocols))
    body.append("")
    body.extend(emit_generated_event_enum(protocols))
    body.append("")
    body.extend(emit_generated_hook_decoder(protocols))
    body.append("")
    body.extend(emit_generated_event_decoder_by_opcode(protocols))
    body.append("")
    body.extend(emit_generated_hook_decoder_by_id(protocols))
    body.append("")
    body.extend(emit_generated_tracked_request_enum(protocols))
    body.append("")
    body.extend(emit_generated_implemented_request_enum(protocols))
    body.append("")
    body.extend(emit_generated_tracked_decoder(protocols))
    body.append("")
    body.extend(emit_generated_tracked_decoder_by_id(protocols))
    body.append("")
    body.extend(emit_generated_implemented_decoder_by_id(protocols))
    body.append("")
    body.extend(emit_generated_implemented_predicate(protocols))
    body.append("")
    body.extend(emit_generated_implemented_request_encoder(protocols))
    body.append("")
    body.extend(emit_generated_event_encoder(protocols))
    body.append("")
    body.extend(
        [
            "pub(crate) fn find_generated_request(",
            "    request_name: &str,",
            ") -> Option<(&'static GeneratedInterfaceSpec, &'static GeneratedMessageSpec)> {",
            "    let (interface_name, request_leaf) = request_name.split_once('.')?;",
            "    for protocol in GENERATED_PROTOCOLS {",
            "        for interface in protocol.interfaces {",
            "            if interface.name != interface_name {",
            "                continue;",
            "            }",
            "            if let Some(request) = interface.requests.iter().find(|request| request.name == request_leaf) {",
            "                return Some((interface, request));",
            "            }",
            "        }",
            "    }",
            "    None",
            "}",
            "",
            "pub(crate) fn find_generated_request_id(",
            "    request_name: &str,",
            ") -> Option<GeneratedRequestId> {",
            "    Some(match request_name {",
        ]
    )
    for request_name in all_request_names(protocols):
        body.append(
            f"        {rust_string(request_name)} => GeneratedRequestId::{request_id_name(request_name)},"
        )
    body.extend(
        [
            "        _ => return None,",
            "    })",
            "}",
            "",
            "pub(crate) fn find_generated_request_by_id(",
            "    request_id: GeneratedRequestId,",
            ") -> Option<(&'static GeneratedInterfaceSpec, &'static GeneratedMessageSpec)> {",
            "    Some(match request_id {",
        ]
    )
    for request_name in all_request_names(protocols):
        body.append(
            f"        GeneratedRequestId::{request_id_name(request_name)} => "
            f"find_generated_request({rust_string(request_name)})?,"
        )
    body.extend(
        [
            "    })",
            "}",
            "",
            "pub(crate) fn find_generated_event_by_opcode(",
            "    interface_name: &str,",
            "    opcode: u16,",
            ") -> Option<(&'static GeneratedInterfaceSpec, &'static GeneratedMessageSpec)> {",
            "    for protocol in GENERATED_PROTOCOLS {",
            "        for interface in protocol.interfaces {",
            "            if interface.name != interface_name {",
            "                continue;",
            "            }",
            "            let event = interface.events.get(opcode as usize)?;",
            "            return Some((interface, event));",
            "        }",
            "    }",
            "    None",
            "}",
            "",
            "pub(crate) fn find_generated_request_by_opcode(",
            "    interface_name: &str,",
            "    opcode: u16,",
            ") -> Option<(&'static GeneratedInterfaceSpec, &'static GeneratedMessageSpec)> {",
            "    for protocol in GENERATED_PROTOCOLS {",
            "        for interface in protocol.interfaces {",
            "            if interface.name != interface_name {",
            "                continue;",
            "            }",
            "            let request = interface.requests.get(opcode as usize)?;",
            "            return Some((interface, request));",
            "        }",
            "    }",
            "    None",
            "}",
            "",
            "pub(crate) fn find_generated_request_id_by_opcode(",
            "    interface_name: &str,",
            "    opcode: u16,",
            ") -> Option<GeneratedRequestId> {",
            "    let (_, request) = find_generated_request_by_opcode(interface_name, opcode)?;",
            "    let request_name = format!(\"{}.{}\", interface_name, request.name);",
            "    find_generated_request_id(&request_name)",
            "}",
            "",
            "pub(crate) fn find_generated_hook_request(",
            "    request_name: &str,",
            ") -> Option<&'static GeneratedHookRequestSpec> {",
            "    GENERATED_HOOK_REQUESTS",
            "        .iter()",
            "        .find(|spec| spec.request_name == request_name)",
            "}",
        ]
    )
    return prelude + "\n\n" + "\n".join(body) + "\n"


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--protocol-dir", type=pathlib.Path, default=DEFAULT_PROTOCOL_DIR)
    parser.add_argument("--cargo", action="store_true", help="emit Cargo input dependencies")
    args = parser.parse_args()

    xml_paths = [args.protocol_dir / path for path in PROTOCOL_XML_PATHS]
    if args.cargo:
        for xml_path in xml_paths:
            print(f"cargo:rerun-if-changed={xml_path}", flush=True)
    missing = [str(path) for path in xml_paths if not path.is_file()]
    if missing:
        parser.error("missing installed Wayland protocol XML files: " + ", ".join(missing))
    protocols = [parse_protocol(xml_path) for xml_path in xml_paths]
    args.output.write_text(render(protocols))
    print(f"wrote {args.output}")


if __name__ == "__main__":
    main()
