import assert from 'node:assert/strict';
import {indexedDB} from 'fake-indexeddb';
import {readFile} from 'node:fs/promises';
import {stripTypeScriptTypes} from 'node:module';
// In-memory IndexedDB only; this never opens a browser profile or a Site.
globalThis.indexedDB=indexedDB;
const source=stripTypeScriptTypes(await readFile(new URL('../lib/workspace-storage.ts',import.meta.url),'utf8'));
const load=key=>import(`data:text/javascript;base64,${Buffer.from(source+'\n// '+key).toString('base64')}`);
const a=await load('tab A'), b=await load('tab B');
const owner='synthetic-owner',other='synthetic-neighbor';
const file=new File(['original fixture'],'duplicate.txt');
const item=(id,phase,extra={})=>({id,phase,source:{type:'file',file,decoded:'article fixture'},...extra});
const snapshot={version:1,draft:{paste:'keep draft'},items:[
 item('saved','saved',{record:{id:'saved-record'},result:{text:'cached result'}}),
 item('cancelled','cancelled'),item('pending','waiting'),
 item('save-failure','failed',{record:{id:'pending-record'},savePending:true,result:{text:'unsaved extraction'}}),
 item('saved-flag','saved',{record:{id:'flag-record'},savePending:true,result:{text:'retain pending even if inconsistent phase'}})
]};
await a.writeWorkspace(other,{items:[item('neighbor','saved',{record:{id:'other-record'}})]});
const neighbor=await a.readWorkspace(other);
const writing=a.writeWorkspace(owner,snapshot);
const clearing=a.clearSavedWorkspaceCache(owner);
await writing;
const cleared=await clearing;
assert.equal(cleared.clearedItems,1);
assert.equal(cleared.snapshot.items[0].source.type,'stored');
assert.equal(cleared.snapshot.items[0].source.file,undefined);
assert.equal(cleared.snapshot.items[0].source.decoded,undefined);
assert.equal(cleared.snapshot.items[0].result,undefined);
for(const index of [1,2,3,4])assert.equal(await cleared.snapshot.items[index].source.file.text(),'original fixture');
assert.equal(cleared.snapshot.items[3].result.text,'unsaved extraction');
assert.equal(cleared.snapshot.draft.paste,'keep draft');
assert.deepEqual(await a.readWorkspace(other),neighbor);
// Separate module instance models a stale tab's late checkpoint after clear.
await b.writeWorkspace(owner,snapshot);
const recovered=await a.readWorkspace(owner);
assert.equal(recovered.items[0].source.file,undefined);
assert.equal(recovered.items[3].result.text,'unsaved extraction');
assert.equal(await recovered.items[1].source.file.text(),'original fixture');
assert.equal((await b.clearSavedWorkspaceCache(owner)).clearedItems,0);
assert.deepEqual(await b.clearSavedWorkspaceCache('never-used-owner'),{snapshot:null,clearedItems:0});
assert.deepEqual(await a.readWorkspace(other),neighbor);
const realOpen=indexedDB.open.bind(indexedDB);
indexedDB.open=()=>{throw new DOMException('fixture disk full','QuotaExceededError');};
await assert.rejects(a.writeWorkspace(owner,snapshot),{name:'QuotaExceededError'});
indexedDB.open=realOpen;
await a.writeWorkspace(owner,snapshot);
assert.equal((await a.readWorkspace(owner)).items[3].result.text,'unsaved extraction');
console.log('Workspace storage: owner-scoped saved-copy clear, pending File/result preservation, refresh, stale cache writes, quota failure recovery passed (synthetic IndexedDB).');
