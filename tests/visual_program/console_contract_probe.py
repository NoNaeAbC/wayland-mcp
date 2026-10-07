#!/usr/bin/env python3
"""Validate cross-VM ArrayBuffer packing and bounded result callback decoding.

Mock native transport only; this probe has no frame source or GPU readback.
"""
import json
from pathlib import Path
import subprocess

runtime = (Path(__file__).resolve().parents[2] / "src/wayland_console_runtime.mjs").read_text()
p = subprocess.Popen(["/usr/bin/node", "--permission", "--no-addons", "--disable-sigusr1",
                      "--input-type=module", "--eval", runtime],
                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
def send(value):
    p.stdin.write(json.dumps(value) + "\n")
    p.stdin.flush()
def receive():
    line = p.stdout.readline()
    assert line, p.stderr.read()
    return json.loads(line)
try:
    assert receive()["type"]=="ready"
    send({"type":"eval","id":1,"code":"globalThis.watch=await wayland.onVisualProgram({windowId:'fixture',parameters:Uint8Array.from([0,1,2,255]).buffer},(buffer,meta)=>{globalThis.latest={words:Array.from(new Uint32Array(buffer)),meta};}); return watch.id;"})
    call=receive()
    assert call["type"]=="native_call" and call["method"]=="subscribe_visual_program", call
    assert call["args"]["parameters"]==[0,1,2,255], call
    send({"type":"native_result","id":call["id"],"ok":True,"value":{"id":4503599627370497,"initialState":{}}})
    reply=receive()
    assert reply["ok"], reply
    send({"type":"input_event","subscriptionId":4503599627370497,"event":{"words":[1,100,200,0],"sequence":1,"timestampMs":42}})
    send({"type":"eval","id":2,"code":"await wayland.sleep(10); return latest;"})
    reply=receive()
    assert reply["ok"] and reply["value"]=={"words":[1,100,200,0],"meta":{"sequence":1,"timestampMs":42}}, reply
    print("Cross-VM ArrayBuffer parameters and event-only callback decoded correctly.")
finally:
    p.terminate()
    p.communicate(timeout=5)
