#!/usr/bin/env python3
"""Exercise release MCP captures and both observers against a live Vulkan cube.

Requires vkcube, Pillow, the built release binary and an accessible Wayland/DRM
session. Decodes returned PNGs in memory; writes no image artifacts. No mock
backend or synthetic frame source is used.
"""
import importlib.util, pathlib, tempfile, os, json, time, base64, hashlib, io
from PIL import Image
ROOT=pathlib.Path(__file__).resolve().parents[2]
spec=importlib.util.spec_from_file_location('live',ROOT/'scripts/validate-live.py')
live=importlib.util.module_from_spec(spec);spec.loader.exec_module(live)
with tempfile.TemporaryDirectory(prefix='mcp-live-review-') as temp:
 d=pathlib.Path(temp)
 try:
  env=dict(os.environ,WAYLAND_MCP_ARTIFACT_DIR=str(d/'artifacts'))
  m=live.Mcp(live.spawn([str(ROOT/'target/release/wayland-mcp')],env,d,'server',input_pipe=True,output_pipe=True))
  m.call('initialize',dict(protocolVersion='2024-11-05',capabilities={},clientInfo=dict(name='live-review',version='1')))
  launch=m.js('return await wayland.environment();')
  client_env=dict(env,**{k:launch[k] for k in ['XDG_RUNTIME_DIR','WAYLAND_DISPLAY']})
  app=['vkcube','--wsi','wayland','--width','320','--height','240']
  cube=live.spawn(app,client_env,d,'client')
  w=live.wait_until(lambda:next((w for w in m.js('return await wayland.windows();') if w['mapped']),None),'live Vulkan window')
  time.sleep(.5)
  w=next(w for w in m.js('return await wayland.windows();') if w['mapped'])
  wid=json.dumps(w['window_id']); print('WINDOW',json.dumps(w),flush=True)
  hashes=[]
  for mode in ['window','buffer','window']:
   time.sleep(.15)
   r=m.call('tools/call',dict(name='gui_console',arguments=dict(code=f'return await wayland.screenshot({{windowId:{wid},coordinateSpace:{json.dumps(mode)}}});')))
   if r.get('isError'):raise RuntimeError(r)
   imgs=[x for x in r['content'] if x['type']=='image'];assert len(imgs)==1,r
   raw=base64.b64decode(imgs[0]['data']); im=Image.open(io.BytesIO(raw)).convert('RGB')
   assert len(im.getcolors(im.width*im.height))>10,'blank capture'
   hashes.append(hashlib.sha256(im.tobytes()).hexdigest());print('CAPTURE',mode,im.size,hashes[-1],flush=True)
  assert hashes[0]!=hashes[2],'animated cube captures did not change'
  shader='#version 450\nlayout(local_size_x=1) in; layout(set=0,binding=0,rgba16f) readonly uniform image2D frame; layout(set=0,binding=5,std430) buffer Result {uint words[];} result; void main(){result.words[0]=floatBitsToUint(imageLoad(frame,ivec2(160,120)).r);}'
  args=dict(windowId=w['window_id'],passes=[dict(source=shader,dispatch=[1,1,1])],resultBytes=4,sourceColor=dict(transfer='srgb',primaries='bt709',alpha='opaque'))
  m.js('globalThis.results=[];globalThis.watch=await wayland.onVisualProgram('+json.dumps(args)+',(b,meta)=>results.push({words:Array.from(new Uint32Array(b)),meta})); return watch.id;')
  time.sleep(1)
  r=m.js('let metrics;try{metrics=await watch.metrics();}catch(e){metrics={error:e.message};} const status=watch.status();const end=await watch.unsubscribe();return {results,metrics,status,end};')
  assert len(r['results'])>1,r
  assert all('sourceColor' in x['meta'] for x in r['results'])
  print('PROGRAM',json.dumps({'callbacks':len(r['results']),'distinctResults':len({tuple(x['words']) for x in r['results']}),'sourceColor':r['results'][0]['meta']['sourceColor'],'metrics':r['metrics']}),flush=True)
  assert len({tuple(x['words']) for x in r['results']})>1,'observer is returning stale pixels'
  fixed=dict(windowId=w['window_id'],sourceColor=args['sourceColor'],rules=[dict(id='motion',rect=[0,0,min(w['width'],320),min(w['height'],240)],kind='change',threshold=.005,minPixels=8)])
  m.js('globalThis.fixedResults=[];globalThis.fixedWatch=await wayland.onVisual('+json.dumps(fixed)+',e=>fixedResults.push(e));return fixedWatch.id;')
  time.sleep(.5)
  f=m.js('let metrics;try{metrics=await fixedWatch.metrics();}catch(e){metrics={error:e.message}};const status=fixedWatch.status();await fixedWatch.unsubscribe();return {results:fixedResults,metrics,status};')
  if f['metrics'].get('framesProcessed',0)<=1:print('DIAGNOSTICS',json.dumps(m.js('return {diagnostics:await wayland.diagnostics(),windows:await wayland.windows()};')),flush=True)
  assert f['metrics'].get('framesProcessed',0)>1,f
  assert f['results'],f
  print('FIXED',json.dumps({'events':len(f['results']),'metrics':f['metrics']}),flush=True)
  # Concurrent native reads must finish while real agent shaders compile.
  heavy=shader.replace('result.words[0]=floatBitsToUint(imageLoad(frame,ivec2(160,120)).r);', 'float v=imageLoad(frame,ivec2(160,120)).r;'+('v=sin(v+0.123456)+cos(v*0.4567);'*100)+'result.words[0]=floatBitsToUint(v);')
  compiling_args=dict(args,passes=[dict(source=heavy,dispatch=[1,1,1]) for _ in range(8)])
  progress=m.js('const started=Date.now();let compiled=false;const compiling=wayland.onVisualProgram('+json.dumps(compiling_args)+',()=>{}).then(w=>{compiled=true;return w;});await wayland.windows();const readMs=Date.now()-started;const readBeforeCompile=!compiled;const w=await compiling;const compileMs=Date.now()-started;await w.unsubscribe();return {readBeforeCompile,readMs,compileMs};')
  assert progress['readBeforeCompile'], 'shader compilation blocked native reads: '+json.dumps(progress)
  print('COMPILATION_PROGRESS',json.dumps(progress),flush=True)
  # End the evaluation before compilation finishes. Its late registration must
  # be reclaimed, or another observer on the same window will hit the budget.
  abandoned=m.js('wayland.onVisualProgram('+json.dumps(compiling_args)+',()=>{}).catch(()=>{});await wayland.sleep(20);return "evaluation ended";')
  assert abandoned=='evaluation ended'
  time.sleep(max(1.5,progress['compileMs']/1000*2))
  reclaimed=m.js('const w=await wayland.onVisualProgram('+json.dumps(args)+',()=>{});const id=w.id;await w.unsubscribe();return id;')
  assert reclaimed,'late compiler registration was not reclaimed'
  print('CANCELLED_COMPILATION_RECLAIMED',reclaimed,flush=True)
  assert not list(d.rglob('*.png')),'unexpected image files'
  print('PASS: live release MCP captures changing Vulkan frames and delivers GPU program callbacks, no image files.',flush=True)
 finally:
  for p in reversed(live.processes):
   if p.poll() is None:p.terminate()
  for p in reversed(live.processes):
   try:p.wait(timeout=3)
   except Exception:p.kill();p.wait()
  for log in list(d.glob('*.log')) + list(d.glob('artifacts/*.jsonl')):
   text=log.read_text()
   if text and ('client_disconnected' in text or log.suffix=='.log'):print(log.name,text[-8000:],flush=True)
  for f in live.logs:f.close()
