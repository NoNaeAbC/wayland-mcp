#!/usr/bin/env python3
"""Exercise extensions with a visible native GTK window on the current desktop."""
import importlib.util
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import time
import sys
spec=importlib.util.spec_from_file_location('live',Path(__file__).with_name('validate-live.py'))
live=importlib.util.module_from_spec(spec);spec.loader.exec_module(live)
with tempfile.TemporaryDirectory(prefix='wayland-mcp-input-') as temp:
    directory=Path(temp)
    try:
        generated=[]
        for name,category in [('relative-pointer','relative-pointer'),('pointer-constraints','pointer-constraints')]:
            xml=Path('/usr/share/wayland-protocols/unstable')/category/(name+'-unstable-v1.xml')
            header=directory/(name+'-client.h');source=directory/(name+'-protocol.c')
            subprocess.run(['wayland-scanner','client-header',str(xml),str(header)],check=True)
            subprocess.run(['wayland-scanner','private-code',str(xml),str(source)],check=True)
            generated.append(str(source))
        flags=shlex.split(subprocess.check_output(['pkg-config','--cflags','--libs','gtk4','wayland-client'],text=True))
        subprocess.run(['cc','-Wall','-Wextra',str(live.ROOT/'tests/live/input-client.c'),*generated,'-I'+str(directory),'-o',str(directory/'client'),*flags],check=True)
        env=dict(os.environ,WAYLAND_MCP_ARTIFACT_DIR=str(directory/'artifacts'))
        if '--transparent' in sys.argv:env['WAYLAND_MCP_DMABUF_MODE']='transparent'
        if '--trace' in sys.argv:env['WAYLAND_MCP_TRACE']='1'
        mcp=live.Mcp(live.spawn([str(live.ROOT/'target/debug/wayland-mcp')],env,directory,'mcp',input_pipe=True,output_pipe=True))
        mcp.call('initialize',dict(protocolVersion='2024-11-05',capabilities={},clientInfo=dict(name='visible-input',version='1')))
        mcp.process.stdin.write(json.dumps(dict(jsonrpc='2.0',method='notifications/initialized'))+'\n');mcp.process.stdin.flush()
        launch=mcp.js('return await wayland.environment();')
        client_env=dict(env,GDK_BACKEND='wayland',GSK_RENDERER='vulkan' if '--vulkan' in sys.argv else ('gl' if '--gl' in sys.argv else 'cairo'),GTK_A11Y='none')
        for key in ['XDG_RUNTIME_DIR','WAYLAND_DISPLAY']:client_env[key]=launch[key]
        client=live.spawn([str(directory/'client')],client_env,directory,'client')
        live.wait_until(lambda:'GLOBALS:' in (directory/'client.log').read_text(),'native extension bindings')
        window=live.wait_until(lambda:next((w for w in mcp.js('return await wayland.windows();') if w.get('title')=='MCP input and subsurface compositor' and w.get('mapped')),None),'visible input window')
        mcp.js(f'globalThis.windowId={json.dumps(window["window_id"])};globalThis.samples=[];globalThis.sub=await wayland.onInput({{windowId,origin:"all",devices:["pointer","touch","relative_pointer","pointer_constraints"]}},e=>samples.push(e));return true;')
        mcp.js('const w=(await wayland.windows()).find(w=>w.window_id===windowId);await wayland.pointerEvent({windowId,event:{type:"enter",x:Math.round(w.width/2),y:Math.round(w.height/2)}});return w;')
        result=mcp.js('globalThis.surfaceId=samples.find(e=>e.device==="pointer"&&e.event.type==="enter").surfaceId;await wayland.touchEvent({windowId,surfaceId,event:{type:"down",id:4,x:180*256,y:180*256}});await wayland.touchEvent({windowId,surfaceId,event:{type:"down",id:9,x:220*256,y:180*256}});await wayland.touchEvent({windowId,surfaceId,event:{type:"motion",id:4,x:190*256+64,y:185*256}});await wayland.touchEvent({windowId,surfaceId,event:{type:"shape",id:4,major:12*256,minor:6*256}});await wayland.touchEvent({windowId,surfaceId,event:{type:"orientation",id:4,orientation:45*256}});await wayland.touchEvent({windowId,surfaceId,event:{type:"frame"}});await wayland.touchEvent({windowId,surfaceId,event:{type:"up",id:4}});await wayland.touchEvent({windowId,surfaceId,event:{type:"up",id:9}});await wayland.touchEvent({windowId,surfaceId,event:{type:"frame"}});return true;')
        live.wait_until(lambda:'TOUCH_UP:9' in (directory/'client.log').read_text(),'native multi-touch delivery')
        mcp.js('const capture=wayland.captureNextFrame({windowId,timeoutMs:3000});await wayland.sleep(30);await wayland.pointerEvent({windowId,surfaceId,coordinateSpace:"surface-fixed",event:{type:"relative_motion",utime_hi:0,utime_lo:1,dx:1,dy:0,dx_unaccel:1,dy_unaccel:0}});return await capture;')
        print('VISIBLE INPUT WINDOW: '+json.dumps(window),flush=True)
        print('Control directory: '+str(directory),flush=True)
        print((directory/'client.log').read_text(),flush=True)
        while True:
            time.sleep(.25)
            if client.poll() is not None:raise RuntimeError('native input client exited')
            if (directory/'stop').exists():break
            if (directory/'code.js').exists():
                try:
                    result=mcp.js((directory/'code.js').read_text());(directory/'result.json').write_text(json.dumps(result,indent=2));print('Control result: '+json.dumps(result),flush=True)
                except RuntimeError as error:print('Control failed: '+str(error),flush=True)
                (directory/'code.js').unlink()
    except BaseException:
        import shutil
        saved=Path(tempfile.mkdtemp(prefix="wayland-mcp-input-failure-"))
        for log in directory.glob("*.log"):shutil.copyfile(log,saved/log.name)
        print("Failure logs: "+str(saved),flush=True)
        raise
    finally:
        for log in directory.glob('*.log'):print(log.name+'\n'+log.read_text()[-4000:],flush=True)
        for process in reversed(live.processes):
            if process.poll() is None:process.terminate()
        for process in reversed(live.processes):
            try:process.wait(timeout=3)
            except subprocess.TimeoutExpired:process.kill();process.wait()
        for log in live.logs:log.close()
