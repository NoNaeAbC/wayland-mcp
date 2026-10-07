#!/usr/bin/env python3
"""Check memory-only bootstrap API and failed-capture image-file boundary.

Successful memory capture/present/dispose is exercised by the console unit test.
This production probe requests an absent window and must create no image files.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--binary", type=Path, default=ROOT / "target/release/wayland-mcp",
                    help="built MCP executable (default: release build)")
binary = parser.parse_args().binary.resolve()
with tempfile.TemporaryDirectory(prefix="visual-boundary-") as directory:
    env = dict(os.environ, WAYLAND_MCP_ARTIFACT_DIR=directory)
    server = subprocess.Popen([str(binary)], env=env,
                              stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE, text=True)
    serial = 0

    def call(method, params):
        global serial
        serial += 1
        server.stdin.write(json.dumps(dict(jsonrpc="2.0", id=serial,
                                          method=method, params=params)) + "\n")
        server.stdin.flush()
        while True:
            line = server.stdout.readline()
            assert line, "MCP exited unexpectedly"
            response = json.loads(line)
            if response.get("id") == serial:
                assert "error" not in response, response
                return response["result"]

    try:
        call("initialize", dict(protocolVersion="2024-11-05", capabilities={},
                                clientInfo=dict(name="gpu-boundary-check", version="1")))
        source = """
const methods = [
  () => wayland.screenshot({windowId:'absent'}),
  () => wayland.captureNextFrame({windowId:'absent'}),
  () => wayland.beginObservation({windowId:'absent'}),
  () => wayland.presentImage(1),
  () => wayland.disposeImage(1),
];
const outcomes=[];
for(const method of methods) {
  try {await method(); outcomes.push('UNEXPECTED SUCCESS');}
  catch(error) {outcomes.push(error.message);}
}
return {outcomes,help:wayland.help};
"""
        result = call("tools/call", dict(name="gui_console", arguments=dict(code=source)))
        assert not result.get("isError"), result
        structured = result["structuredContent"]
        outcomes = structured["value"]["outcomes"]
        assert len(outcomes) == 5, structured
        assert all(value != "UNEXPECTED SUCCESS" for value in outcomes), structured
        assert all("pixel readback is disabled" not in value for value in outcomes), structured
        assert "No image files are written" in structured["value"]["help"], structured
        assert structured["images"] == [], structured
        assert all(block["type"] != "image" for block in result["content"]), result
        assert not list(Path(directory).rglob("*.png")), "unexpected pixel artifact"
        print("Memory-only bootstrap API advertised; absent-window capture fails without images or image files.")
    finally:
        server.terminate()
        try:
            server.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            server.kill()
            server.communicate()
