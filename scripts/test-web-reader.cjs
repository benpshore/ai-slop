#!/usr/bin/env node
/** Reader orchestration regressions against the real component.
 * Synthetic React/jsdom fixtures mock extraction, persistence and network.
 * Run: node scripts/test-web-reader.cjs
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
let stored = null, copiedText = '', rejectCopy = false, holdRemote = null, staleGet = null, raceFetches = 0;
Object.defineProperty(navigator,'clipboard',{value:{readText:async()=>'',writeText:async text=>{if(rejectCopy)throw new Error('denied');copiedText=text;}},configurable:true});
const helpers = {
  '@/components/ui/button':{Button:({asChild,children,variant,...props})=>asChild?React.cloneElement(children,props):React.createElement('button',props,children)},
  '@/lib/clip':{clipHtml:()=>{throw Error('Unexpected HTML path');},parseFeed:()=>{throw Error('Unexpected feed');},textDois:()=>[],doiFrom:()=>undefined,safeUrl:value=>{try{const url=new URL(value);return /^https?:$/.test(url.protocol)?url.href:null;}catch{return null;}}},
  '@/lib/imports':{expandUploads:async function*(){throw Error('Unexpected archive');}},
  '@/lib/image-ocr':{recognizeImage:async()=>{throw Error('Unexpected OCR');}},
  '@/lib/office':{extractOffice:async()=>{throw Error('Unexpected Office');}},
  '@/lib/article-assets':{retainArticleImages:async(_record,value)=>value},
  '@/lib/workspace-storage':{readWorkspace:async()=>stored,writeWorkspace:async(_owner,value)=>{stored=value;}},
  '@/lib/upload-client':{
    decodeSource:async file=>file.text(),
    uploadOriginal:async(file,{onProgress,signal})=>{
      const value=row('doc'+rows.size,file.name);rows.set(value.id,value);onProgress?.(.4);
      await holds.get('upload:'+file.name)?.promise;signal?.throwIfAborted();onProgress?.(1);return value;
    },
    saveExtracted:async(record,value,{onProgress,signal}={})=>{
      onProgress?.(.5);await holds.get('save:'+record.original_name)?.promise;signal?.throwIfAborted();savedResults.set(record.id,value);rows.set(record.id,{...record,status:value.status});onProgress?.(1);
    },
    captureSource:async()=>{throw Error('Unexpected URL');},uploadAssetFile:async()=>{throw Error('Unexpected asset');}
  }
};
global.fetch = async value=>{
  const url=new URL(value,'https://reader.test');
  if(url.pathname==='/api/documents')return Response.json({documents:[...rows.values()]});
  const id=url.pathname.split('/')[3];
  if(url.pathname.endsWith('/original'))return new Response('Fresh recovered text');
  if(id==='remote'&&holdRemote)await holdRemote.promise;
  if(id==='race'&&staleGet&&++raceFetches===1){await staleGet.promise;return Response.json({record:row('race','Stale title'),result:result('Stale title','Stale text')});}
  return Response.json({record:rows.get(id),result:savedResults.get(id)||null});
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
let root;
(async()=>{
  root=createRoot(document.getElementById('root'));
  await flush(()=>root.render(React.createElement(Workspace,{userId:'synthetic-owner'})));
  pass('Upload has a visible label',document.querySelector('.add-menu summary').textContent==='Upload');
  holdRemote=hold('remote-first');
  await flush(()=>button('Saved elsewhereSaved').click());
  assert.equal(document.querySelector('#reader').getAttribute('aria-busy'),'true');
  await flush(()=>history.back(),35);
  assert.equal(location.pathname+location.search,'/');
  assert.equal(document.querySelector('#reader').getAttribute('aria-busy'),'false');
  await flush(()=>holdRemote.release());holdRemote=null;
  pass('Back clears loading and rejects a late saved-document response',!document.querySelector('.reading'));

  const saving=hold('save:alpha.md');
  const markdown='# Alpha\n\nA **readable** paragraph.\n\n<script>bad()</script>\n\n![remote](https://tracking.invalid/image.png)';
  await pick([new File([markdown],'alpha.md',{type:'text/markdown'})]);
  assert.equal(document.querySelector('.reading h1').textContent,'Alpha');
  assert.equal(document.querySelector('.reading strong').textContent,'readable');
  assert.equal(document.querySelector('.reading script, .reading img'),null);
  assert(!button('Retry save'));
  pass('Safe formatted Markdown is readable while its save is still pending');
  assert.equal(document.querySelector('progress[aria-label="Saving result for alpha.md"]').value,50);
  pass('Progress describes the actual current save stage',document.querySelector('progress').getAttribute('aria-valuetext').includes('50% of this stage'));
  await flush(()=>button('Plain text').click());
  assert.equal(document.querySelector('.plain-reading').textContent,markdown);
  await flush(()=>[...document.querySelectorAll('summary')].find(node=>node.textContent==='Details and review notes').click());
  assert.equal(button('Plain text').getAttribute('aria-pressed'),'true');
  assert.equal(new URL(location.href).searchParams.get('mode'),'plain');
  pass('Plain text is a real independent reader mode, including when details open');
  await flush(()=>button('Copy Markdown').click());
  assert.equal(copiedText,markdown);
  pass('Copy Markdown stays discoverable after success',!!button('Copy Markdown'));
  rejectCopy=true;
  await flush(()=>button('Copy Markdown').click());rejectCopy=false;
  pass('Clipboard denial gives a download fallback',document.querySelector('[role=alert]').textContent.includes('Download Markdown')&&!!button('Download Markdown'));
  await flush(()=>saving.release());
  const alphaId=new URL(location.href).searchParams.get('document');

  holdRemote=hold('remote-second');
  await flush(()=>button('Saved elsewhereSaved').click());
  await flush(()=>queueRow('alpha.md').querySelector('.queue-open').click());
  assert.equal(new URL(location.href).searchParams.get('document'),alphaId);
  assert.equal(document.querySelector('#reader').getAttribute('aria-busy'),'false');
  await flush(()=>holdRemote.release());holdRemote=null;
  pass('Reselecting the current queue item cancels pending navigation elsewhere',document.querySelector('.plain-reading').textContent===markdown);

  const repeated=new File(['Repeat body'],'repeat.txt');
  await pick([repeated]);await pick([repeated]);
  const folderFile=new File(['Folder body'],'chapter.txt');Object.defineProperty(folderFile,'webkitRelativePath',{value:'Book/chapter.txt'});
  await pick([folderFile],'Choose a folder');
  assert.equal(document.querySelectorAll('.queue-item').length,4);
  assert.equal(document.querySelector('.plain-reading').textContent,markdown);
  pass('Repeated same-file and folder selections append without replacing the reader',!!queueRow('Book/chapter.txt'));
  pass('Another ready result has an explicit Open action',!!button('Open chapter.txt'));

  const literal='# This is literal text\n<tag>Keep these brackets</tag>';
  await pick([new File([literal],'literal.txt')]);
  await flush(()=>button('Open literal.txt').click());
  assert.equal(document.querySelector('.reading h1'),null);
  pass('Ordinary text stays literal in Reading mode',document.querySelector('.reading').textContent===literal);

  const loose=new File(['Loose file'],'loose.txt');
  const member=new File(['Dropped folder member'],'member.txt');
  let delivered=false;
  const directory={name:'Dropped folder',isDirectory:true,isFile:false,createReader:()=>({readEntries:done=>{done(delivered?[]:[{name:member.name,isDirectory:false,isFile:true,file:done=>done(member)}]);delivered=true;}})};
  await flush(()=>{const event=new Event('drop',{bubbles:true,cancelable:true});Object.defineProperty(event,'dataTransfer',{value:{items:[{kind:'file',webkitGetAsEntry:()=>directory,getAsFile:()=>null},{kind:'file',webkitGetAsEntry:()=>null,getAsFile:()=>loose}],files:[],getData:()=>''}});document.querySelector('main').dispatchEvent(event);});
  assert(queueRow('loose.txt'));assert(queueRow('Dropped folder/member.txt'));
  pass('Mixed folder drops keep files that lack directory-entry support',document.querySelector('.reading').textContent===literal);

  const uploading=hold('upload:late.txt');
  await pick([new File(['Late body'],'late.txt')]);
  await flush(()=>queueRow('late.txt').querySelector('.queue-open').click());
  await flush(()=>{const address=new URL(location.href);address.searchParams.set('tab','evidence');address.searchParams.set('mode','plain');history.replaceState(history.state,'',address);});
  await flush(()=>uploading.release());
  assert.equal(new URL(location.href).searchParams.get('tab'),'evidence');
  pass('Late original upload preserves the current address mode',new URL(location.href).searchParams.get('mode')==='plain');

  const racingUpload=hold('upload:history-race.txt');
  await pick([new File(['Queue reading'],'history-race.txt')]);
  await flush(()=>queueRow('history-race.txt').querySelector('.queue-open').click());
  holdRemote=hold('remote-third');
  await flush(()=>button('Saved elsewhereSaved').click());
  await flush(()=>racingUpload.release());
  assert.equal(new URL(location.href).searchParams.get('document'),'remote');
  await flush(()=>holdRemote.release());holdRemote=null;
  pass('Completing an upload cannot replace a pending saved-document address',new URL(location.href).searchParams.get('document')==='remote'&&document.querySelector('.reading').textContent==='Remote reading');

  await flush(()=>root.unmount());root=null;
  const race=row('race','Recovered file.txt');rows.set(race.id,race);
  stored={version:1,items:[{id:'race-queue',name:race.title,source:{type:'stored',name:race.original_name},phase:'interrupted',record:race,progress:null,message:'Interrupted'}],draft:{url:'',kind:'file',paste:'',query:''},selection:{queueId:'race-queue'},view:'text',scroll:0};
  history.replaceState({},'','/?queue=race-queue');staleGet=hold('stale-get');
  root=createRoot(document.getElementById('root'));
  await flush(()=>root.render(React.createElement(Workspace,{userId:'synthetic-owner'})));
  await flush(()=>button('Retry').click());
  assert.equal(document.querySelector('.reading').textContent,'Fresh recovered text');
  await flush(()=>staleGet.release());
  pass('A stale queue fetch cannot overwrite a newer retry result',document.querySelector('.reading').textContent==='Fresh recovered text');
  await flush(()=>root.unmount());root=null;
  console.log(JSON.stringify({checks,limitations:'React/jsdom with synthetic mocked transport/extraction/storage; no deployment or device claims.'},null,2));
})().catch(async error=>{console.error(error);if(root)await flush(()=>root.unmount());process.exitCode=1;});
