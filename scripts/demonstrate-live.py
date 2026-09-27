#!/usr/bin/env python3
"""Show native GTK test windows through the MCP on the current desktop.

Only the private model clipboard is used; the host clipboard is not read or
changed. Windows stay open until this script is interrupted.
"""
import importlib.util
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import time

import sys
if '--input' in sys.argv:
    import runpy
    runpy.run_path(str(Path(__file__).with_name('demonstrate-input.py')), run_name='__main__')
    raise SystemExit

path=Path(__file__).with_name('validate-live.py')
spec=importlib.util.spec_from_file_location('live',path)
live=importlib.util.module_from_spec(spec)
spec.loader.exec_module(live)

with tempfile.TemporaryDirectory(prefix='wayland-mcp-visible-') as temp:
    directory=Path(temp)
    try:
        flags=shlex.split(subprocess.check_output(['pkg-config','--cflags','--libs','gtk4'],text=True))
        subprocess.run(['cc',str(live.ROOT/'tests/live/clipboard-client.c'),'-o',str(directory/'client'),*flags],check=True)
        env=dict(os.environ,WAYLAND_MCP_ARTIFACT_DIR=str(directory/'artifacts'))
        mcp=live.Mcp(live.spawn([str(live.ROOT/'target/debug/wayland-mcp')],env,directory,'mcp',input_pipe=True,output_pipe=True))
        mcp.call('initialize',dict(protocolVersion='2024-11-05',capabilities={},clientInfo=dict(name='visible-demo',version='1')))
        mcp.process.stdin.write(json.dumps(dict(jsonrpc='2.0',method='notifications/initialized'))+'\n');mcp.process.stdin.flush()
        launch=mcp.js('return await wayland.environment();')
        client_env=dict(env,GDK_BACKEND='wayland',GSK_RENDERER='cairo',GTK_A11Y='none')
        for key in ['XDG_RUNTIME_DIR','WAYLAND_DISPLAY']:client_env[key]=launch[key]
        owner=live.spawn([str(directory/'client'),'MCP clipboard — source','PRIVATE CLIPBOARD: visible GTK test'],client_env,directory,'source')
        owner_id=live.wait_until(lambda:next((w['window_id'] for w in mcp.js('return await wayland.windows();') if w.get('title')=='MCP clipboard — source' and w.get('mapped')),None),'visible source window')
        mcp.js(f"const w=(await wayland.windows()).find(w=>w.window_id==={json.dumps(owner_id)});await wayland.click({{windowId:w.window_id,x:Math.round(0.19*w.width),y:Math.round(0.59*w.height)}});return true;")
        receiver=live.spawn([str(directory/'client'),'MCP clipboard — destination',''],client_env,directory,'destination')
        receiver_id=live.wait_until(lambda:next((w['window_id'] for w in mcp.js('return await wayland.windows();') if w.get('title')=='MCP clipboard — destination' and w.get('mapped')),None),'visible destination window')
        mcp.js(f"const w=(await wayland.windows()).find(w=>w.window_id==={json.dumps(receiver_id)});await wayland.click({{windowId:w.window_id,x:Math.round(0.445*w.width),y:Math.round(0.59*w.height)}});return true;")
        try:
            live.wait_until(lambda:'TEXT:PRIVATE CLIPBOARD: visible GTK test' in (directory/'destination.log').read_text(),'visible private paste')
        except RuntimeError as error:
            print('Clipboard check failed: '+str(error),flush=True)
        print('VISIBLE WINDOWS: source and destination. Copy/paste result: '+repr((directory/'destination.log').read_text()),flush=True)
        initial=mcp.js(f"globalThis.samples=[];globalThis.recording=await wayland.onInput({{windowId:{json.dumps(receiver_id)},origin:'human',devices:['pointer']}},e=>samples.push(e));return recording.initialState;")
        print('HUMAN RECORDING ENABLED: move the mouse over the destination window to reproduce a bug. Windows stay open.',flush=True)
        print('Control directory: '+str(directory),flush=True)
        last=0
        while True:
            time.sleep(1)
            count=mcp.js('return samples.length;')
            if count!=last:
                print(f'Human recording: {count} events',flush=True);last=count
            if (directory/'code.js').exists():
                try:
                    result=mcp.js((directory/'code.js').read_text())
                    (directory/'result.json').write_text(json.dumps(result,indent=2))
                    print('Control result: '+json.dumps(result),flush=True)
                except RuntimeError as error:
                    print('Control failed: '+str(error),flush=True)
                (directory/'code.js').unlink()
            if (directory/'replay').exists():
                try:
                    end=mcp.js('return await recording.unsubscribe();')
                    samples=mcp.js('return samples;')
                    (directory/'recording.json').write_text(json.dumps(samples,indent=2))
                    result=mcp.js("globalThis.replay=async()=>{let previous;for(const e of samples){if(previous!==undefined)await wayland.sleep(Math.min(100,e.timestampMs-previous));await wayland.pointerEvent({windowId:e.windowId,surfaceId:e.surfaceId,coordinateSpace:'surface-fixed',event:e.event});previous=e.timestampMs;}};await replay();await replay();await replay();return samples.length;")
                    print(f'Recording ended: {end}; replayed {result} events three times.',flush=True)
                except RuntimeError as error:
                    print('Replay failed: '+str(error),flush=True)
                    print('Available windows: '+json.dumps(mcp.js('return await wayland.windows();')),flush=True)
                (directory/'replay').unlink()
    except BaseException:
        for log in directory.glob('*.log'):
            print(log.name+'\n'+log.read_text()[-6000:],flush=True)
        raise
    finally:
        for process in reversed(live.processes):
            if process.poll() is None:process.terminate()
        for process in reversed(live.processes):
            try:process.wait(timeout=3)
            except subprocess.TimeoutExpired:process.kill();process.wait()
        for log in live.logs:log.close()
