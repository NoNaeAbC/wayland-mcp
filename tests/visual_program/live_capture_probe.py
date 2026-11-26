#!/usr/bin/env python3
"""Exercise release MCP captures and both observers against a live Vulkan cube.

Requires vkcube, Pillow, FFmpeg, the release binary and a Wayland session with
Vulkan and VA-API access. Decodes PNGs and recordings in memory. Recordings
are written in the temporary test directory.
"""
import importlib.util, pathlib, tempfile, os, json, time, base64, hashlib, io
import subprocess
from PIL import Image
ROOT=pathlib.Path(__file__).resolve().parents[2]
spec=importlib.util.spec_from_file_location('live',ROOT/'scripts/validate-live.py')
live=importlib.util.module_from_spec(spec);spec.loader.exec_module(live)

def check_recording(mcp, window_id, producer_height):
    start = mcp.js(
        f"return await wayland.startRecording({{windowId:{window_id}, maxFrames:8, frameRate:12}});"
    )

    def finished():
        status = mcp.js(f"return await wayland.recordingStatus({{recordingId:{start['recordingId']}}});")
        return status if status["stopReason"] else None

    status = live.wait_until(finished, "complete H.265 recording")
    assert status["framesWritten"] == 8 and status["stopReason"] == "frame_limit", status
    width, height = status["width"], status["height"]
    decoded = subprocess.run(
        ["ffmpeg", "-v", "error", "-threads", "1", "-i", start["path"], "-filter_threads", "1",
         "-pix_fmt", "rgb24", "-c:v", "rawvideo", "-threads:v", "1", "-f", "rawvideo", "pipe:1"],
        capture_output=True, check=True,
    ).stdout
    frame_bytes = width * height * 3
    assert len(decoded) == 8 * frame_bytes, (width, height, len(decoded))
    for frame in range(8):
        start_byte = frame * frame_bytes
        image = Image.frombytes("RGB", (width, height), decoded[start_byte:start_byte + frame_bytes])
        for x, y in [(8, 8), (width - 9, 8), (8, producer_height - 9), (width - 9, producer_height - 9)]:
            pixel = image.getpixel((x, y))
            assert all(abs(channel - 51) <= 6 for channel in pixel), (width, height, frame, x, y, pixel)
    print("RECORDING", width, producer_height, height, "eight frames with correct background pixels", flush=True)

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
  cube.terminate()
  cube.wait(timeout=5)
  m.js('return await wayland.waitForWindowGone({windowId:'+wid+'});')
  for width, height in ((480, 300), (480, 320), (512, 300), (512, 320)):
   producer = live.spawn(['vkcube','--wsi','wayland','--width',str(width),'--height',str(height)],
                         client_env,d,f'recording-client-{width}x{height}')
   try:
    window = m.js('return await wayland.waitForWindow({});')
    assert (window['width'], window['height']) == (width, height), window
    check_recording(m, json.dumps(window['window_id']), height)
   finally:
    producer.terminate()
    producer.wait(timeout=5)
   m.js('return await wayland.waitForWindowGone({windowId:'+json.dumps(window['window_id'])+'});')
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
