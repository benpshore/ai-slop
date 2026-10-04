#!/usr/bin/env node
/** Integrated import and storage lifecycle regressions against the real component.
 * Synthetic React/jsdom fixtures mock extraction, persistence and network.
 * Run: node scripts/test-web-workspace-lifecycle.cjs
 * Browser layout/device/storage behavior is covered separately, not inferred here.
 */
const assert = require('node:assert/strict');
const {createRequire} = require('node:module');
const Module = require('node:module');
const fs = require('node:fs');
const path = require('node:path');
const {File} = require('node:buffer');
const web = path.resolve(__dirname, '../web');
const req = createRequire(path.join(web, 'package.json'));
const {JSDOM} = req('jsdom');
const dom = new JSDOM('<div id="root"></div>', {url:'https://reader.test/'});
for (const key of ['window','document','DOMParser','HTMLElement','Element','NodeFilter','HTMLInputElement','CustomEvent','MutationObserver','getComputedStyle','Node','Event','MouseEvent','history','location']) global[key] = key === 'window' ? dom.window : dom.window[key];
Object.defineProperty(global, 'navigator', {value:dom.window.navigator, configurable:true});
global.File = File;
global.IS_REACT_ACT_ENVIRONMENT = true;
global.requestAnimationFrame = callback => { queueMicrotask(callback); return 1; };
window.scrollTo = () => {};
const React = req('react'), {createRoot} = req('react-dom/client'), {act} = React;
const ts = req('typescript');
const row = (id, name) => ({id,title:name,kind:'text',original_name:name,status:'ready',engine:'Synthetic',created_at:'2026-10-04T00:00:00Z',sha256:'private-hash',bytes:32,source_url:null});
const result = (title, text) => ({title,text,markdown:text,links:[],warnings:[],engine:'Synthetic',status:'ready'});
const remote = row('remote','Saved elsewhere');
const rows = new Map([[remote.id,remote]]), savedResults = new Map([[remote.id,result(remote.title,'Remote reading')]]);
const holds = new Map();
function hold(key) { let release; const promise = new Promise(resolve => { release = resolve; }); const control = {promise,release}; holds.set(key,control); return control; }
let rejectWrites=false, rejectClear=false, deleteStatus=204;
const originalCalls=[],resultCalls=[],deleteCalls=[],uploadProgress=new Map(),failOriginal=new Set();
let stored = null;
Object.defineProperty(navigator,'clipboard',{value:{readText:async()=>'',writeText:async()=>{}},configurable:true});
const helpers = {
  '@/components/ui/button':{Button:({asChild,children,variant,...props})=>asChild?React.cloneElement(children,props):React.createElement('button',props,children)},
  '@/lib/clip':{clipHtml:()=>{throw Error('Unexpected HTML path');},parseFeed:()=>{throw Error('Unexpected feed');},textDois:()=>[],doiFrom:()=>undefined,safeUrl:value=>{try{const url=new URL(value);return /^https?:$/.test(url.protocol)?url.href:null;}catch{return null;}}},
  '@/lib/imports':{expandUploads:async function*(){throw Error('Unexpected archive');}},
  '@/lib/image-ocr':{recognizeImage:async()=>{throw Error('Unexpected OCR');}},
  '@/lib/office':{extractOffice:async()=>{throw Error('Unexpected Office');}},
  '@/lib/article-assets':{retainArticleImages:async(_record,value)=>value},
  '@/lib/workspace-storage':{readWorkspace:async()=>{await holds.get('restore')?.promise;return stored;},writeWorkspace:async(_owner,value)=>{if(rejectWrites)throw Error('Synthetic checkpoint failure');stored=value;},clearSavedWorkspaceCache:async()=>{await holds.get('clear')?.promise;if(rejectClear)throw Error('Synthetic cache failure');stored={...stored,items:stored.items.map(item=>{if(item.phase!=='saved'||item.savePending||!item.record)return item;const {file,decoded,...source}=item.source;return {...item,result:undefined,source:{...source,...(file?{type:'stored',name:file.name}:{})}};})};return {snapshot:stored,clearedItems:1};}},
  '@/lib/upload-client':{
    decodeSource:async file=>file.text(),
    uploadOriginal:async(file,{onProgress,signal})=>{
      originalCalls.push(file.name);uploadProgress.set(file.name,onProgress);onProgress?.(.4);
      await holds.get('upload:'+file.name)?.promise;if(failOriginal.has(file.name))throw Error('Synthetic original failure');if(!file.name.startsWith('receipt-'))signal?.throwIfAborted();const value=row('doc'+originalCalls.length,file.name);rows.set(value.id,value);onProgress?.(1);return value;
    },
    saveExtracted:async(record,value,{onProgress,signal}={})=>{
      resultCalls.push(record.id);onProgress?.(.5);await holds.get('save:'+record.original_name)?.promise;if(!record.original_name.startsWith('receipt-'))signal?.throwIfAborted();savedResults.set(record.id,value);rows.set(record.id,{...record,status:value.status});onProgress?.(1);
    },
    captureSource:async()=>{throw Error('Unexpected URL');},uploadAssetFile:async()=>{throw Error('Unexpected asset');}
  }
};
global.fetch = async (value,options={})=>{
  const url=new URL(value,'https://reader.test');
  if(url.pathname==='/api/documents'){const documents=[...rows.values()];await holds.get('list')?.promise;return Response.json({documents});}
  const id=url.pathname.split('/')[3];
  if(options.method==='DELETE'){
    assert.equal(JSON.parse(options.body).confirmDocumentId,id);deleteCalls.push(id);
    if(deleteStatus===500)return new Response('Synthetic server failure before deletion',{status:500});
    rows.delete(id);savedResults.delete(id);
    if(deleteStatus===503)return new Response('The document is removed from your library. Storage cleanup is incomplete; retry Delete to finish.',{status:503});
    return new Response(null,{status:204});
  }
  if(url.pathname.endsWith('/original'))return new Response('Fresh recovered text');
  const valueAtRequest={record:rows.get(id),result:savedResults.get(id)||null};
  await holds.get('get:'+id)?.promise;
  return valueAtRequest.record?Response.json(valueAtRequest):new Response('Deleted',{status:404});
};

const localModules = new Map();
function loadLocal(name, parent = path.join(web, 'app/workspace.tsx')) {
    const base = name.startsWith('@/') ? path.join(web, name.slice(2)) : path.resolve(path.dirname(parent), name);
    const filename = [base, base + '.ts', base + '.tsx'].find(value => fs.existsSync(value) && fs.statSync(value).isFile());
    if (!filename) throw new Error('Unknown local module: ' + name);
    if (localModules.has(filename)) return localModules.get(filename).exports;
    const child = new Module(filename); child.filename = filename; child.paths = Module._nodeModulePaths(web); localModules.set(filename, child);
    child.require = dependency => helpers[dependency] || (dependency.startsWith('@/') || dependency.startsWith('.') ? loadLocal(dependency, filename) : req(dependency));
    child._compile(ts.transpileModule(fs.readFileSync(filename, 'utf8'), {compilerOptions:{module:ts.ModuleKind.CommonJS,jsx:ts.JsxEmit.ReactJSX,target:ts.ScriptTarget.ES2022,esModuleInterop:true}}).outputText, filename);
    return child.exports;
}
const source=path.join(web,'app/workspace.tsx'), mod=new Module(source);
mod.filename=source;mod.paths=Module._nodeModulePaths(web);mod.require=name=>helpers[name]||(name.startsWith('@/')?loadLocal(name):req(name));
mod._compile(ts.transpileModule(fs.readFileSync(source,'utf8'),{compilerOptions:{module:ts.ModuleKind.CommonJS,jsx:ts.JsxEmit.ReactJSX,target:ts.ScriptTarget.ES2022,esModuleInterop:true}}).outputText,source);
const Workspace=mod.exports.default;
const checks=[];
const pass=(name,condition=true)=>{assert(condition,name);checks.push(name);};
const flush=async(callback=()=>{},delay=20)=>act(async()=>{await callback();await new Promise(resolve=>setTimeout(resolve,delay));});
const button=name=>[...document.querySelectorAll('button')].find(node=>node.textContent.trim()===name);
const queueRow=name=>[...document.querySelectorAll('.queue-item')].find(node=>node.querySelector('strong').textContent===name);
const pick=async(files,label='Choose source files')=>flush(()=>{const input=document.querySelector('input[aria-label="'+label+'"]');Object.defineProperty(input,'files',{value:files,configurable:true});input.dispatchEvent(new Event('change',{bubbles:true}));assert.equal(input.value,'');});
const inButton=(label,within=document)=>[...within.querySelectorAll('button')].find(node=>node.textContent.trim()===label);
const dialog=()=>document.querySelector('[role=alertdialog]');
const confirm=async label=>{await flush(()=>inButton(label).click());assert(dialog());await flush(()=>inButton(label,dialog()).click());};
const queueAction=(name,label)=>inButton(label,queueRow(name));
let root;
const mount=async()=>{root=createRoot(document.getElementById('root'));await flush(()=>root.render(React.createElement(Workspace,{userId:'synthetic-owner'})));};
const unmount=async()=>{await flush(()=>root.unmount());root=null;};
(async()=>{
  await mount();
  const originalReceipt=hold('upload:receipt-original.txt');
  await pick([new File(['Immediate original preview'],'receipt-original.txt')]);
  assert.equal(document.querySelector('.reading').textContent,'Immediate original preview');
  await flush(()=>queueAction('receipt-original.txt','Cancel').click());
  assert(queueAction('receipt-original.txt','Remove from queue').disabled);
  await flush(()=>uploadProgress.get('receipt-original.txt')(.95));
  assert.equal(queueRow('receipt-original.txt').querySelector('progress').value,40);
  await flush(()=>originalReceipt.release());
  assert(queueRow('receipt-original.txt').querySelector('.phase-cancelled'));
  assert.equal(document.querySelector('.reading').textContent,'Immediate original preview');
  pass('Cancel holds removal until settlement, rejects late progress and retains a confirmed original receipt');
  await flush(()=>queueAction('receipt-original.txt','Save again').click());
  assert.equal(originalCalls.filter(name=>name==='receipt-original.txt').length,1);
  assert(queueRow('receipt-original.txt').querySelector('.phase-saved'));
  const originalId=[...rows.values()].find(value=>value.original_name==='receipt-original.txt').id;
  pass('Retry after a cancelled original receipt saves the retained result without uploading again');

  const resultReceipt=hold('save:receipt-result.txt');
  await pick([new File(['Immediate result preview'],'receipt-result.txt')]);
  await flush(()=>queueRow('receipt-result.txt').querySelector('.queue-open').click());
  assert(inButton('Delete saved document').disabled);
  await flush(()=>queueAction('receipt-result.txt','Cancel').click());
  assert(queueAction('receipt-result.txt','Remove from queue').disabled);
  await flush(()=>resultReceipt.release());
  assert(queueRow('receipt-result.txt').querySelector('.phase-saved'));
  assert(!queueAction('receipt-result.txt','Save again'));
  pass('A confirmed result commit after Cancel stays Saved and clears the retry payload');
  const resultId=[...rows.values()].find(value=>value.original_name==='receipt-result.txt').id;

  failOriginal.add('pending.txt');
  await pick([new File(['Uncommitted but readable'],'pending.txt')]);
  await flush(()=>queueRow('pending.txt').querySelector('.queue-open').click());
  assert(queueAction('pending.txt','Retry'));
  await flush(()=>queueAction('pending.txt','Remove from queue').click());
  assert(dialog());await flush(()=>inButton('Keep',dialog()).click());
  assert(queueRow('pending.txt'));assert.equal(deleteCalls.length,0);
  pass('Discarding an unfinished import requires confirmation and never calls document DELETE');
  rejectWrites=true;
  await flush(()=>queueAction('pending.txt','Remove from queue').click());
  await flush(()=>inButton('Remove from queue',dialog()).click());
  assert(queueRow('pending.txt'));assert.match(document.body.textContent,/checkpoint failure/);
  rejectWrites=false;
  if(dialog())await flush(()=>inButton('Keep',dialog()).click());
  await flush(()=>queueAction('pending.txt','Remove from queue').click());
  await flush(()=>inButton('Remove from queue',dialog()).click());
  assert(!queueRow('pending.txt'));assert.equal(deleteCalls.length,0);
  pass('A failed local removal checkpoint preserves its File and result, then succeeds on explicit retry');
  await flush(()=>queueRow('receipt-result.txt').querySelector('.queue-open').click());
  await flush(()=>queueAction('receipt-result.txt','Remove from queue').click());
  assert(!queueRow('receipt-result.txt'));assert(rows.has(resultId));assert.equal(document.querySelector('.reading').textContent,'Immediate result preview');
  pass('Removing a saved queue entry keeps its library record and active reading');

  failOriginal.add('unfinished.txt');
  await pick([new File(['Keep this recovery text'],'unfinished.txt')]);
  rejectClear=true;await confirm('Clear saved copies');
  assert(dialog());assert.match(dialog().textContent,/Synthetic cache failure/);
  assert(queueRow('unfinished.txt'));rejectClear=false;
  await flush(()=>inButton('Keep',dialog()).click());
  pass('Cache cleanup failure stays visible and leaves unfinished recovery available');
  const clearing=hold('clear'),newArrival=hold('upload:new-arrival.txt');
  await confirm('Clear saved copies');assert(dialog());
  await pick([new File(['New arrival preview'],'new-arrival.txt')]);
  await flush(()=>clearing.release());holds.delete('clear');
  assert(stored.items.find(item=>item.name==='unfinished.txt').result);
  assert(stored.items.find(item=>item.name==='new-arrival.txt').result);
  assert(stored.items.find(item=>item.name==='new-arrival.txt').source.file instanceof File);
  assert.equal(document.querySelector('.reading').textContent,'Immediate result preview');
  assert.equal(deleteCalls.length,0);
  await flush(()=>queueAction('new-arrival.txt','Cancel').click());await flush(()=>newArrival.release());
  pass('Cache clear preserves unfinished work and arrivals while it waits, without replacing the active reader');

  await flush(()=>queueRow('receipt-original.txt').querySelector('.queue-open').click());
  deleteStatus=500;await confirm('Delete saved document');assert(dialog());assert(rows.has(originalId));
  await flush(()=>inButton('Keep',dialog()).click());
  await flush(()=>inButton('Re-read original').click());
  assert.equal(document.querySelector('.reading').textContent,'Fresh recovered text');
  pass('A failure before deletion leaves the document usable and releases its writer lock');

  const staleList=hold('list'),staleReader=hold('get:remote');
  await flush(()=>document.querySelector('button[aria-label=Search]').click());
  await flush(()=>button('Saved elsewhereSaved').click());
  deleteStatus=503;await confirm('Delete saved document');
  assert(dialog());assert(!document.querySelector('.reading'));assert(!rows.has(originalId));
  assert.equal(stored.pendingDeletion.record.id,originalId);
  await flush(()=>{staleList.release();staleReader.release();});holds.delete('list');holds.delete('get:remote');
  assert(!document.querySelector('.reading'));
  assert(![...document.querySelectorAll('.document-item')].some(node=>node.textContent.includes('receipt-original')));
  pass('Cleanup 503 hides the tombstoned document immediately and blocks late list/reader resurrection');
  const callsBeforeReload=deleteCalls.length;
  await unmount();await mount();
  assert.equal(deleteCalls.length,callsBeforeReload);assert.match(document.querySelector('.library').textContent,/Cleanup pending for “receipt-original.txt”/);
  pass('Restoring pending cleanup keeps unrelated saved documents visible', [...document.querySelectorAll('.document-item')].some(node=>node.textContent.includes('receipt-result.txt')));
  await confirm('Clear saved copies');assert.equal(stored.pendingDeletion.record.id,originalId);
  deleteStatus=204;await confirm('Delete saved document');
  assert.equal(deleteCalls.length,callsBeforeReload+1);assert.equal(stored.pendingDeletion,undefined);
  pass('Cleanup retry survives reload and cache clear and only runs after another explicit confirmation');

  await flush(()=>[...document.querySelectorAll('.document-item')].find(node=>node.textContent.includes('receipt-result.txt')).click());
  rejectWrites=true;const callsBeforeCheckpointFailure=deleteCalls.length;
  await confirm('Delete saved document');assert(dialog());assert(!rows.has(resultId));
  assert.match(dialog().textContent,/finish updating local recovery/);
  rejectWrites=false;await flush(()=>inButton('Delete saved document',dialog()).click());
  assert.equal(deleteCalls.length,callsBeforeCheckpointFailure+1);
  pass('A successful remote deletion with failed local checkpoint retains a retry without repeating DELETE');

  await unmount();
  const removed=row('removed','Removed synthetic.txt');rows.set(removed.id,removed);savedResults.set(removed.id,result(removed.title,'Stale removed content'));
  stored={version:1,items:[],draft:{url:'',kind:'file',paste:'',query:''},selection:null,view:'text',scroll:0,pendingDeletion:{record:removed,cleanupComplete:false}};
  history.replaceState({},'','/');const recovering=hold('restore'),staleRemoved=hold('get:removed');
  await mount();assert(!document.body.textContent.includes('Bring your reading here.'));assert.match(document.querySelector('#reader').textContent,/Restoring your workspace/);
  await pick([new File(['Unrelated live reading'],'unrelated.txt')]);
  await flush(()=>[...document.querySelectorAll('.document-item')].find(node=>node.textContent.includes('Removed synthetic.txt')).click());
  await flush(()=>recovering.release());holds.delete('restore');
  await flush(()=>staleRemoved.release());holds.delete('get:removed');
  assert.equal(document.querySelector('.reading').textContent,'Unrelated live reading');
  assert(![...document.querySelectorAll('.document-item')].some(node=>node.textContent.includes('Removed synthetic.txt')));
  pass('Pending-cleanup restoration invalidates an already requested stale reader and keeps unrelated live work');
  await unmount();dom.window.close();
  console.log(JSON.stringify({checks,limitations:'Real React confirmation controls, attempt registry, text helper and deletion client; synthetic storage/network. No existing user documents or deployed services.'},null,2));
})().catch(async error=>{console.error(error);for(const value of holds.values())value.release();if(root)await unmount();dom.window.close();process.exitCode=1;});
