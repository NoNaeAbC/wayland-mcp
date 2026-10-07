import assert from "node:assert/strict";
import {readFile} from "node:fs/promises";
import {makeOnResult} from "./result_callback.mjs";

// Input contains only actual 32-byte shader results from the native GPU probe.
const records=(await readFile(process.argv[2],"utf8")).trim().split("\n").map(JSON.parse);
const expected=[
  [0,0,0,0,0,1,0,0],
  [27,7,9,7,9,2,0,0],
  [17,7,9,7,9,3,0,0],
  [25,11,12,11,12,4,0,0],
  [28,0,0,0,0,5,0,0],
  [16,0,0,0,0,6,0,0],
  [1,7,9,7,9,7,0,0],
];
assert.equal(records.length,expected.length);
const events=[];
const onResult=makeOnResult({emit:event=>events.push(event)});
for(let i=0;i<records.length;i++) {
  assert.equal(records[i].frame,i+1);
  assert.deepEqual(records[i].words,expected[i]);
  const bytes=new ArrayBuffer(32);
  const view=new DataView(bytes);
  records[i].words.forEach((word,index)=>view.setUint32(index*4,word,true));
  await onResult(bytes);
}
assert.deepEqual(events.map(event=>event.type),[
  "target-entered","target-mask-changed","target-mask-changed",
  "target-exited","target-mask-changed",
]);
assert.deepEqual(events[0].bounds,{minX:7,minY:9,maxX:7,maxY:9});
assert.deepEqual(events[2].bounds,{minX:11,minY:12,maxX:11,maxY:12});
await assert.rejects(onResult(new ArrayBuffer(31)),RangeError);
console.log(JSON.stringify({passed:true,results:records.length,events}));
