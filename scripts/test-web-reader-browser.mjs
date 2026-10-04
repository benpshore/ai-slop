#!/usr/bin/env node
/**
 * Real Chromium QA of the checked-in Workspace component and stylesheet.
 *
 * Install the locked web dependencies, then run:
 *   node scripts/test-web-reader-browser.mjs
 * Optional: PLAYWRIGHT_MODULE=/absolute/path/to/playwright/index.js
 *           CHROMIUM_EXECUTABLE=/absolute/path/to/chromium
 *           READER_QA_OUTPUT=/tmp/pdftextract-reader-qa
 *
 * React, DOMPurify, the import registry/controls, document-client, Button, and CSS
 * are real. Extraction, upload, remote document
 * APIs are synthetic mocks; recovery uses browser IndexedDB through observable
 * test wrappers that can fail the cache-clear boundary. This does not validate PDF/OCR
 * accuracy, production authentication, private storage, browser durability,
 * native TPE, device Safari, arbitrary OS picker behavior, or deployed code.
 * The one read-only public Site navigation records access status only.
 */
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import fs from 'node:fs/promises';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const web = path.join(root, 'web');
const require = createRequire(import.meta.url);
const webRequire = createRequire(path.join(web, 'package.json'));
const buildRequire = createRequire(webRequire.resolve('@tailwindcss/postcss'));
const { build } = buildRequire('esbuild');
const postcss = buildRequire('postcss');
const tailwind = webRequire('@tailwindcss/postcss');
const { chromium } = require(process.env.PLAYWRIGHT_MODULE || 'playwright');
const output = path.resolve(process.env.READER_QA_OUTPUT || '/tmp/pdftextract-reader-qa');
const temporary = await fs.mkdtemp(path.join(os.tmpdir(), 'tpe-reader-browser-'));
await fs.mkdir(output, { recursive: true });
const checks = [];
const browserErrors = [];
const pass = name => { checks.push(name); console.log('PASS ' + name); };

const fixture = String.raw`
import {readWorkspace as actualReadWorkspace, writeWorkspace as actualWriteWorkspace, clearSavedWorkspaceCache as actualClearSavedWorkspaceCache} from ${JSON.stringify(path.join(web,'lib/workspace-storage.ts'))};
const persisted=JSON.parse(localStorage.getItem('synthetic-backend')||'{}');
const rows = new Map(persisted.rows||[]), results = new Map(persisted.results||[]), originals = new Map(persisted.originals||[]);
let nextId=persisted.nextId||0, snapshot=null;
const persistBackend=()=>localStorage.setItem('synthetic-backend',JSON.stringify({rows:[...rows],results:[...results],originals:[...originals],nextId}));
const gates=new Map(), failures=new Map();let prefixArmed=false;
const qa=window.__qa={events:[],operations:[],networkCalls:[],workerCalls:0,holdAfterReload:key=>sessionStorage.setItem('qa-hold-on-start',key),holdPrefix:()=>{prefixArmed=true;qa.hold('prefix');},rows:()=>[...rows.values()],snapshot:()=>snapshot,
 hold:(key)=>{let resolve;const promise=new Promise(done=>resolve=done);gates.set(key,{promise,resolve});},
 release:key=>{gates.get(key)?.resolve();gates.delete(key);},
 failOnce:key=>failures.set(key,true),
 seed:async(name,source)=>{const record=await uploadOriginal(new File([source],name,{type:'text/html'}),{});await saveExtracted(record,clipHtml(source,'https://fixture.invalid/',name));return record.id;}};
const startupGate=sessionStorage.getItem('qa-hold-on-start');if(startupGate){qa.hold(startupGate);sessionStorage.removeItem('qa-hold-on-start');}
async function stage(key,signal){
 qa.events.push(key);signal?.throwIfAborted();
 if(failures.delete(key))throw Error('Synthetic interrupted '+key);
 const gate=gates.get(key);if(!gate)return;
 await new Promise((resolve,reject)=>{const aborted=()=>reject(new DOMException('Cancelled','AbortError'));signal?.addEventListener('abort',aborted,{once:true});gate.promise.then(()=>{signal?.removeEventListener('abort',aborted);resolve();});});
 signal?.throwIfAborted();
}
const actualArrayBuffer=Blob.prototype.arrayBuffer;Blob.prototype.arrayBuffer=async function(){if(prefixArmed){prefixArmed=false;await stage('prefix');}return actualArrayBuffer.call(this);};
window.fetch=async(input,options={})=>{
 const url=new URL(String(input),location.origin);qa.networkCalls.push({method:options.method||'GET',path:url.pathname});
 if(url.pathname==='/api/documents'){const documents=[...rows.values()];await stage('list',options.signal);return Response.json({documents});}
 const id=url.pathname.split('/')[3];
 if(options.method==='DELETE'){
  if(JSON.parse(options.body||'{}').confirmDocumentId!==id)throw Error('DELETE lacked exact confirmation');
  qa.operations.push({type:'delete-attempt',id});
  try{await stage('delete:'+id,options.signal);}catch(error){rows.delete(id);results.delete(id);persistBackend();qa.operations.push({type:'delete-tombstone',id});return new Response(error.message,{status:503});}
  rows.delete(id);results.delete(id);originals.delete(id);persistBackend();qa.operations.push({type:'delete-complete',id});return new Response(null,{status:204});
 }
 if(url.pathname.endsWith('/original'))return rows.has(id)?new Response(originals.get(id)||'Synthetic original'):new Response('Synthetic document not found',{status:404});
 if(rows.has(id)){const payload={record:rows.get(id),result:results.get(id)||null};await stage('get:'+id,options.signal);return Response.json(payload);}
 if(url.pathname.startsWith('/api/documents/'))return new Response('Synthetic document not found',{status:404});
 throw Error('Unexpected synthetic request: '+url.pathname);
};
window.Worker=class {constructor(){qa.workerCalls++;throw Error('PDF workers are outside this synthetic reader QA');}};
export async function uploadOriginal(file,{signal,onProgress}){
 onProgress?.(.25);await stage('upload:'+file.name,signal);
 const source=await file.text(),id='fixture-'+(++nextId);
 const row={id,title:file.name,original_name:file.name,kind:source.startsWith('<')?'html':'text',mime:file.type,status:'uploaded',engine:'',created_at:'2026-10-04T00:00:00Z',sha256:'synthetic-checksum-hidden-in-details',bytes:file.size,source_url:null};
 rows.set(id,row);originals.set(id,source);persistBackend();onProgress?.(1);return row;
}
export async function decodeSource(file,type,signal){await stage('decode:'+file.name,signal);return file.text();}
export async function saveExtracted(record,result,{signal,onProgress}={}){
 onProgress?.(.5);await stage('save:'+record.original_name,signal);
 results.set(record.id,result);rows.set(record.id,{...record,title:result.title,status:result.status});persistBackend();onProgress?.(1);
}
export const captureSource=()=>{throw Error('Unexpected URL capture');};
export const uploadAssetFile=()=>{throw Error('Unexpected asset upload');};
export async function readWorkspace(owner){snapshot=await actualReadWorkspace(owner);return snapshot;}
export async function writeWorkspace(owner,value){if(owner!=='synthetic-owner')throw Error('Unexpected owner');await actualWriteWorkspace(owner,value);snapshot=await actualReadWorkspace(owner);qa.operations.push({type:'checkpoint'});}
export async function clearSavedWorkspaceCache(owner){
 if(owner!=='synthetic-owner')throw Error('Unexpected owner');qa.operations.push({type:'clear-attempt'});await stage('clear');
 const cleared=await actualClearSavedWorkspaceCache(owner);snapshot=cleared.snapshot;qa.operations.push({type:'clear-complete'});return cleared;
}
export function clipHtml(source,url,name){const d=new DOMParser().parseFromString(source,'text/html');return {title:name,text:d.body.innerText||d.body.textContent,markdown:'# '+name+'\n\n'+d.body.textContent,html:d.body.innerHTML,links:[],warnings:[],engine:'Synthetic HTML fixture',status:'ready'};}
export const parseFeed=()=>{throw Error('Unexpected feed parser');};
export const textDois=()=>[];
export const doiFrom=()=>undefined;
export function safeUrl(value){try{const u=new URL(value);return /^https?:$/.test(u.protocol)?u.href:null;}catch{return null;}}
export async function* expandUploads(files){for(const file of files)yield {file,path:file.name};}
export const recognizeImage=()=>{throw Error('Unexpected OCR');};
export const extractOffice=()=>{throw Error('Unexpected Office parsing');};
export const retainArticleImages=async(record,result)=>result;
export const retainOfficeAssets=async(record,result)=>result;
`;
const mocked = new Set(['clip','imports','image-ocr','office','upload-client','article-assets','workspace-storage']);
let server, browser;
try {
  const testedSources={};for(const name of ['web/app/workspace.tsx','web/app/globals.css','web/lib/import-queue.ts','web/components/import-controls.tsx','web/lib/document-client.ts','web/lib/text-import.ts','web/lib/workspace-storage.ts'])testedSources[name]=createHash('sha256').update(await fs.readFile(path.join(root,name))).digest('hex');
  const entry = path.join(temporary, 'entry.tsx');
  await fs.writeFile(entry, `import React from 'react';import {createRoot} from 'react-dom/client';import Workspace from ${JSON.stringify(path.join(web, 'app/workspace.tsx'))};createRoot(document.getElementById('root')!).render(<Workspace userId="synthetic-owner"/>);`);
  await build({entryPoints:[entry],outfile:path.join(temporary,'bundle.js'),bundle:true,format:'iife',platform:'browser',jsx:'automatic',nodePaths:[path.join(web,'node_modules')],define:{'process.env.NODE_ENV':'"test"'},plugins:[{name:'synthetic-services',setup(builder){builder.onResolve({filter:/^@\//},args=>{if(mocked.has(args.path.replace('@/lib/','')))return {path:'fixture',namespace:'synthetic'};return {path:path.join(web,args.path.slice(2)+(args.path.startsWith('@/components/')?'.tsx':'.ts'))};});builder.onLoad({filter:/.*/,namespace:'synthetic'},()=>({contents:fixture,loader:'js',resolveDir:web}));}}]});
  const css = await postcss([tailwind({base:web})]).process(await fs.readFile(path.join(web,'app/globals.css'),'utf8'),{from:path.join(web,'app/globals.css')});
  await fs.writeFile(path.join(temporary,'styles.css'),css.css);
  server = http.createServer(async(req,res)=>{try{const name=new URL(req.url,'http://local.test').pathname;res.setHeader('Cache-Control','no-store');if(name==='/bundle.js'||name==='/styles.css'){res.setHeader('Content-Type',name.endsWith('.js')?'text/javascript':'text/css');res.end(await fs.readFile(path.join(temporary,name)));}else{res.setHeader('Content-Type','text/html');res.end('<!doctype html><html lang="en"><head><meta name="viewport" content="width=device-width,initial-scale=1"><title>Synthetic reader QA</title><link rel="stylesheet" href="/styles.css"></head><body><div id="root"></div><script src="/bundle.js"></script></body></html>');}}catch(error){res.statusCode=500;res.end(String(error));}});
  await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
  const origin='http://127.0.0.1:'+server.address().port;
  browser=await chromium.launch({headless:true,executablePath:process.env.CHROMIUM_EXECUTABLE||'/usr/bin/chromium',args:['--no-sandbox','--disable-dev-shm-usage']});
  const context=await browser.newContext({viewport:{width:1440,height:1000},permissions:['clipboard-read','clipboard-write']});
  const page=await context.newPage();page.on('pageerror',error=>browserErrors.push(String(error)));
  await page.goto(origin);
  const visible = async locator => {await locator.waitFor({state:'visible',timeout:10000});};
  const text = async(locator,pattern)=>{await page.waitForFunction(({selector,pattern})=>new RegExp(pattern).test(document.querySelector(selector)?.textContent||''),{selector:locator,pattern:pattern.source});};
  const hold=key=>page.evaluate(key=>window.__qa.hold(key),key), release=key=>page.evaluate(key=>window.__qa.release(key),key);
  const row=name=>page.locator('.queue-item').filter({has:page.locator('.queue-open strong').filter({hasText:name})});
  const saved=async name=>{await visible(row(name).last().locator('.phase-saved'));};
  const file=(name,contents='Synthetic '+name)=>({name,mimeType:name.endsWith('.html')?'text/html':'text/plain',buffer:Buffer.from(contents)});
  const upload=files=>page.getByLabel('Choose source files',{exact:true}).setInputFiles(files);
  await visible(page.getByText('Bring your reading here.',{exact:true}));
  for(const width of [1280,1440]){await page.setViewportSize({width,height:1000});const intake=await page.locator('.intake').evaluate(el=>({client:el.clientWidth,scroll:el.scrollWidth,overflow:[...el.querySelectorAll('*')].filter(child=>child.getBoundingClientRect().right>el.getBoundingClientRect().right).map(child=>({tag:child.tagName,class:child.className,right:child.getBoundingClientRect().right}))}));await page.screenshot({path:path.join(output,`desktop-empty-${width}.png`)});assert(intake.scroll<=intake.client+1,JSON.stringify({width,...intake}));}
  pass('empty desktop intake has no internal horizontal scrollbar at1280px or1440px');
  const readerIdentity=await page.locator('#reader').evaluate(el=>{el.dataset.qaIdentity='reader-shell';return el.dataset.qaIdentity;});
  await page.locator('summary').filter({hasText:/^Upload$/}).click();
  const chooserPromise=page.waitForEvent('filechooser');await page.getByRole('button',{name:'Add files',exact:true}).click();
  assert.equal((await chooserPromise).isMultiple(),true);
  pass('visible Upload opens a real multiple-file picker');

  const article='<h3>Readable fixture heading</h3>'+Array.from({length:38},(_,i)=>'<p>Reader paragraph '+i+'. This independently authored synthetic document supports navigation, selection, and scroll checks. '+'Long readable content. '.repeat(4)+'</p>').join('');
  await hold('upload:first.html');await hold('decode:first.html');await hold('save:first.html');
  await upload([file('first.html',article)]);
  let progress=page.getByRole('progressbar',{name:'Saving original for first.html',exact:true});
  await visible(progress);assert.equal(await progress.getAttribute('value'),'25');assert.match(await progress.getAttribute('aria-valuetext'),/25% of this stage/);
  assert.equal(await page.locator('#reader').getAttribute('data-qa-identity'),readerIdentity);
  await release('upload:first.html');
  progress=page.getByRole('progressbar',{name:'Extracting for first.html',exact:true});await visible(progress);assert.equal(await progress.getAttribute('value'),null);
  await release('decode:first.html');
  progress=page.getByRole('progressbar',{name:'Saving result for first.html',exact:true});await visible(progress);assert.equal(await progress.getAttribute('value'),'50');
  await visible(page.locator('.reading h3'));await visible(page.getByRole('button',{name:'Copy Markdown',exact:true}));await visible(page.getByRole('button',{name:'Download Markdown',exact:true}));
  await page.screenshot({path:path.join(output,'desktop-readable-during-save.png')});
  pass('actual upload/extraction/save stages and readable output before save finishes');

  await page.getByRole('button',{name:'Copy Markdown',exact:true}).click();
  const clipboard=await page.evaluate(()=>navigator.clipboard.readText());assert.match(clipboard,/^# first.html\n\nReadable fixture heading/);
  const downloadPromise=page.waitForEvent('download');await page.getByRole('button',{name:'Download Markdown',exact:true}).click();const download=await downloadPromise;assert.match(download.suggestedFilename(),/\.md$/);assert.equal(await fs.readFile(await download.path(),'utf8'),clipboard);
  await page.getByRole('button',{name:'Plain text',exact:true}).click();assert.equal(await page.getByRole('button',{name:'Plain text',exact:true}).getAttribute('aria-pressed'),'true');assert.equal(await page.locator('.reading h3').count(),0);assert.match(page.url(),/mode=plain/);
  await page.getByRole('button',{name:'Reading',exact:true}).click();await visible(page.locator('.reading h3'));
  assert.equal(await page.getByText('synthetic-checksum-hidden-in-details',{exact:true}).isVisible(),false);
  await page.getByText('Details and review notes',{exact:true}).click();await visible(page.getByText('synthetic-checksum-hidden-in-details',{exact:true}));await page.getByText('Details and review notes',{exact:true}).click();
  pass('Markdown clipboard/download, named reading modes, and collapsed diagnostics');
  await release('save:first.html');await saved('first.html');

  await page.evaluate(()=>{document.querySelector('.reading').dataset.qaStable='yes';window.scrollTo(0,900);});await page.waitForTimeout(180);
  const before=await page.evaluate(()=>({scroll:scrollY,text:document.querySelector('.reading').textContent,url:location.href}));
  await hold('upload:second.txt');await upload([file('second.txt','Second document reader content')]);await visible(page.locator('.processing-status'));
  assert.deepEqual(await page.evaluate(()=>({scroll:scrollY,text:document.querySelector('.reading').textContent,url:location.href})),before);
  assert.equal(await page.locator('.reading').getAttribute('data-qa-stable'),'yes');
  await release('upload:second.txt');await saved('second.txt');
  assert.deepEqual(await page.evaluate(()=>({scroll:scrollY,text:document.querySelector('.reading').textContent,url:location.href})),before);
  pass('background import preserves reader DOM, selection, URL, and scroll before/during/after');

  await page.getByRole('button',{name:'Open second.txt',exact:true}).click();await text('.reading',/Second document reader content/);await page.waitForFunction(()=>document.activeElement?.id==='reader');
  pass('completed background import has an explicit Open result action and reader focus');
  await page.getByRole('button',{name:'Plain text',exact:true}).click();const secondUrl=page.url();
  await page.goBack();await text('.reading',/Readable fixture heading/);await page.waitForFunction(()=>Math.abs(scrollY-900)<3);assert.equal(await page.getByRole('button',{name:'Reading',exact:true}).getAttribute('aria-pressed'),'true');
  await page.goForward();await text('.reading',/Second document reader content/);assert.equal(page.url(),secondUrl);assert.equal(await page.getByRole('button',{name:'Plain text',exact:true}).getAttribute('aria-pressed'),'true');
  pass('browser back/forward restores document, reading mode, and prior reader scroll');

  await upload([file('repeat.txt')]);await saved('repeat.txt');await upload([file('repeat.txt')]);await saved('repeat.txt');assert.equal(await row('repeat.txt').count(),2);
  const directory=path.join(temporary,'folder-fixture');await fs.mkdir(directory);await fs.writeFile(path.join(directory,'folder-member.txt'),'Folder member content');
  await page.getByLabel('Choose a folder',{exact:true}).setInputFiles(directory);await saved('folder-fixture/folder-member.txt');
  assert.match(await page.locator('.reading').textContent(),/Second document reader content/);
  pass('repeated same-file picks and supported folder picker append to one queue');

  await hold('upload:cancel.txt');await upload([file('cancel.txt')]);await visible(row('cancel.txt').locator('.phase-uploading'));await page.getByRole('button',{name:/^Cancel (import of )?cancel\.txt$/}).click();await visible(row('cancel.txt').locator('.phase-cancelled'));
  assert.equal(await page.evaluate(()=>window.__qa.rows().some(row=>row.original_name==='cancel.txt')),false);
  await release('upload:cancel.txt');await row('cancel.txt').getByRole('button',{name:'Retry',exact:true}).click();await saved('cancel.txt');
  await page.evaluate(()=>window.__qa.failOnce('upload:failure.txt'));await upload([file('failure.txt')]);await visible(row('failure.txt').locator('.phase-failed'));assert.match(await row('failure.txt').textContent(),/Synthetic interrupted upload:failure.txt/);await row('failure.txt').getByRole('button',{name:'Retry',exact:true}).click();await saved('failure.txt');
  await page.evaluate(()=>window.__qa.failOnce('save:save-failure.txt'));await upload([file('save-failure.txt','Unsaved readable result')]);await visible(row('save-failure.txt').locator('.phase-failed'));await row('save-failure.txt').locator('.queue-open').click();await text('.reading',/Unsaved readable result/);await row('save-failure.txt').getByRole('button',{name:'Save again',exact:true}).click();await saved('save-failure.txt');
  pass('active cancellation, upload retry, and retained-result save retry');

  const markdown='# Markdown reader heading\n\n- First list item\n- Second list item\n\n[Source reference](https://example.invalid/reference)\n\n<img src="https://tracking.invalid/pixel" onerror="window.__qaXss=true">\n<style>body{display:none}</style>\n<script>window.__qaXss=true</script>\n\n<a href="javascript:alert(1)">Unsafe raw link</a>\n\n[Unsafe link](javascript:alert(1))';
  await upload([file('fixture.md',markdown)]);await saved('fixture.md');await page.getByRole('button',{name:'Open fixture.md',exact:true}).click();
  await visible(page.locator('.reading h1').filter({hasText:'Markdown reader heading'}));assert.equal(await page.locator('.reading li').count(),2);assert.equal(await page.locator('.reading a').filter({hasText:'Source reference'}).getAttribute('href'),'https://example.invalid/reference');assert.equal(await page.locator('.reading img,.reading style,.reading script,.reading [onerror],.reading a[href^="javascript:"]').count(),0);assert.equal(await page.evaluate(()=>window.__qaXss),undefined);assert.equal(await page.locator('.reading a').filter({hasText:'Unsafe raw link'}).getAttribute('href'),null);
  await page.getByRole('button',{name:'Plain text',exact:true}).click();assert.equal(await page.locator('.reading').textContent(),markdown);await page.getByRole('button',{name:'Reading',exact:true}).click();
  pass('real Markdown headings/lists/links render safely while Plain text preserves source');

  const mobile=await browser.newPage();mobile.on('pageerror',error=>browserErrors.push(String(error)));await mobile.setViewportSize({width:390,height:844});await mobile.goto(origin);await mobile.getByText('Bring your reading here.',{exact:true}).waitFor();
  await mobile.evaluate(article=>window.__qa.seed('saved-mobile.html',article),article);await mobile.getByText('Saved documents',{exact:true}).click();await mobile.getByRole('button',{name:'Search',exact:true}).click();await mobile.getByRole('button',{name:'saved-mobile.html Saved',exact:true}).click();await mobile.getByText('Saved documents',{exact:true}).click();
  await mobile.locator('.reading p').nth(4).waitFor();await mobile.locator('.reading p').nth(4).evaluate(el=>{el.dataset.qaAnchor='yes';window.scrollTo(0,el.getBoundingClientRect().top+scrollY-150);});await mobile.waitForTimeout(180);
  const anchorTop=await mobile.locator('.reading p').nth(4).evaluate(el=>el.getBoundingClientRect().top);
  for(const name of ['mobile-first.txt','mobile-next.txt','mobile-third.txt']){
    await mobile.evaluate(name=>window.__qa.hold('upload:'+name),name);await mobile.getByLabel('Choose source files',{exact:true}).setInputFiles(file(name));await mobile.locator('.phase-uploading').waitFor();await mobile.waitForTimeout(100);
    const during=await mobile.locator('.reading p').nth(4).evaluate(el=>el.getBoundingClientRect().top);assert(Math.abs(during-anchorTop)<3,JSON.stringify({name,stage:'during',anchorTop,during}));
    await mobile.evaluate(name=>window.__qa.release('upload:'+name),name);await mobile.locator('.queue-item').filter({hasText:name}).locator('.phase-saved').waitFor();await mobile.waitForTimeout(100);
    const after=await mobile.locator('.reading p').nth(4).evaluate(el=>el.getBoundingClientRect().top);assert(Math.abs(after-anchorTop)<3,JSON.stringify({name,stage:'after',anchorTop,after}));
  }
  await mobile.screenshot({path:path.join(output,'mobile-stable-reader.png')});await mobile.getByRole('button',{name:'Open mobile-third.txt',exact:true}).click();await mobile.waitForFunction(()=>document.querySelector('.reading')?.textContent.includes('Synthetic mobile-third.txt'));await mobile.waitForFunction(()=>document.activeElement?.id==='reader');const mobileHeading=await mobile.locator('#reader').evaluate(el=>({focused:el===document.activeElement,top:el.getBoundingClientRect().top}));assert(mobileHeading.focused&&mobileHeading.top>=0&&mobileHeading.top<844,JSON.stringify(mobileHeading));await mobile.close();
  pass('mobile saved reader holds reading position as empty intake queue appears and grows');

  await hold('upload:motion.txt');await upload([file('motion.txt')]);await visible(page.locator('.processing-status'));
  await page.emulateMedia({reducedMotion:'reduce'});
  const motion=await page.locator('.processing-status svg').first().evaluate(el=>({animation:getComputedStyle(el).animationName,duration:getComputedStyle(el).animationDuration}));
  assert(motion.animation==='none'||motion.duration.split(',').every(value=>parseFloat(value)<.001),JSON.stringify(motion));
  pass('processing spinner respects reduced motion');
  for(const width of [390,768])for(const scale of [1,2]){
    await page.setViewportSize({width,height:1000});await page.evaluate(scale=>{document.documentElement.style.fontSize=18*scale+'px';window.scrollTo(0,0);},scale);
    await page.waitForTimeout(80);
    const layout=await page.evaluate(()=>({width:innerWidth,document:document.documentElement.scrollWidth,body:document.body.scrollWidth,reader:document.querySelector('#reader').getBoundingClientRect().toJSON(),overflow:[...document.querySelectorAll('body *')].map(el=>({tag:el.tagName,class:el.className,text:el.textContent?.slice(0,70),right:el.getBoundingClientRect().right})).filter(el=>el.right>innerWidth+1)}));
    await page.screenshot({path:path.join(output,`mobile-${width}-text-${scale*100}.png`),fullPage:true});
    assert(layout.document<=width+1&&layout.body<=width+1,JSON.stringify({width,scale,...layout}));
    const controls=await page.locator('.composer-actions').boundingBox();assert(controls.height<=scale*100,'Upload and send controls must remain a compact usable row');const send=await page.getByRole('button',{name:'Import pasted source',exact:true}).boundingBox();assert(send.width>=48&&send.height>=48&&send.x+send.width<=width,'Send control must remain visible and touch sized');
    const hint=page.locator('.composer-hint');if(await hint.isVisible())assert((await hint.boundingBox()).height<=scale*65,'Composer hint must not wrap one character per line at enlarged text');
    await visible(page.getByRole('button',{name:'Download Markdown',exact:true}));
  }
  pass('390px and 768px layouts at 100% and 200% text have no horizontal page overflow');
  await release('upload:motion.txt');await saved('motion.txt');
  const textContext=await browser.newContext({viewport:{width:1024,height:900},permissions:['clipboard-read','clipboard-write']});const textPage=await textContext.newPage();textPage.on('pageerror',error=>browserErrors.push(String(error)));await textPage.goto(origin);await textPage.getByText('Bring your reading here.',{exact:true}).waitFor();
  const textRow=name=>textPage.locator('.queue-item').filter({has:textPage.locator('.queue-open strong').filter({hasText:name})});
  await textPage.evaluate(()=>window.__qa.hold('upload:Pasted text.txt'));await textPage.getByLabel('Paste a link or text',{exact:true}).fill('some text');await textPage.getByRole('button',{name:'Import pasted source',exact:true}).click();await textPage.waitForFunction(()=>document.querySelector('.reading')?.textContent==='some text');
  assert.equal(await textPage.evaluate(()=>window.__qa.rows().some(row=>row.original_name==='Pasted text.txt')),false);assert.equal(await textPage.getByRole('button',{name:'Retry save',exact:true}).count(),0);
  await textPage.getByRole('button',{name:'Copy Markdown',exact:true}).click();assert.equal(await textPage.evaluate(()=>navigator.clipboard.readText()),'some text');const textDownloadPromise=textPage.waitForEvent('download');await textPage.getByRole('button',{name:'Download Markdown',exact:true}).click();assert.equal(await fs.readFile(await (await textDownloadPromise).path(),'utf8'),'some text');await textPage.screenshot({path:path.join(output,'text-readable-before-original.png')});await textPage.evaluate(()=>window.__qa.release('upload:Pasted text.txt'));await textRow('Pasted text.txt').locator('.phase-saved').waitFor();
  pass('exact some text is readable, copyable, and downloadable before original upload completes');

  await textPage.evaluate(()=>window.__qa.failOnce('upload:preview-retry.txt'));await textPage.getByLabel('Choose source files',{exact:true}).setInputFiles(file('preview-retry.txt','Readable preview survives original failure and reload'));await textRow('preview-retry.txt').locator('.phase-failed').waitFor();await textRow('preview-retry.txt').locator('.queue-open').click();await textPage.waitForFunction(()=>document.querySelector('.reading')?.textContent==='Readable preview survives original failure and reload');await textPage.waitForTimeout(400);await textPage.reload();await textPage.waitForFunction(()=>document.querySelector('.reading')?.textContent==='Readable preview survives original failure and reload');
  const retained=await textPage.evaluate(async()=>{const item=window.__qa.snapshot().items.find(item=>item.name==='preview-retry.txt');return {source:await item.source.file.text(),result:item.result.text,record:item.record?.id};});assert.equal(retained.source,'Readable preview survives original failure and reload');assert.equal(retained.result,retained.source);assert.equal(retained.record,undefined);await textRow('preview-retry.txt').getByRole('button',{name:/^(Retry|Save again)$/}).click();await textRow('preview-retry.txt').locator('.phase-saved').waitFor();assert.equal(await textPage.evaluate(()=>window.__qa.rows().filter(row=>row.original_name==='preview-retry.txt').length),1);
  pass('original upload failure preserves preview and File across actual IndexedDB reload and retry');

  await textPage.evaluate(()=>window.__qa.failOnce('save:result-retry.txt'));await textPage.getByLabel('Choose source files',{exact:true}).setInputFiles(file('result-retry.txt','Result retry retains its confirmed original'));await textRow('result-retry.txt').locator('.phase-failed').waitFor();await textRow('result-retry.txt').getByRole('button',{name:'Save again',exact:true}).click();await textRow('result-retry.txt').locator('.phase-saved').waitFor();assert.equal(await textPage.evaluate(()=>window.__qa.events.filter(event=>event==='upload:result-retry.txt').length),1);assert.equal(await textPage.evaluate(()=>window.__qa.rows().filter(row=>row.original_name==='result-retry.txt').length),1);
  pass('text result-save retry reuses the confirmed original');

  await textRow('result-retry.txt').locator('.queue-open').click();await textPage.waitForTimeout(400);const reloadId=await textPage.evaluate(()=>window.__qa.rows().find(row=>row.original_name==='result-retry.txt').id);await textPage.evaluate(id=>window.__qa.holdAfterReload('get:'+id),reloadId);await textPage.reload();await textPage.getByText('Opening document…',{exact:true}).waitFor();assert.equal(await textPage.getByText('Bring your reading here.',{exact:true}).count(),0);await textPage.evaluate(id=>window.__qa.release('get:'+id),reloadId);await textPage.waitForFunction(()=>document.querySelector('.reading')?.textContent==='Result retry retains its confirmed original');
  pass('saved-document reload keeps loading state without a false empty-reader prompt');

  await textPage.evaluate(()=>window.__qa.holdPrefix());const beforeCancelCalls=await textPage.evaluate(()=>window.__qa.networkCalls.length);await textPage.getByLabel('Choose source files',{exact:true}).setInputFiles(file('cancel-before.txt','Cancel before original storage'));await textPage.getByRole('button',{name:/^Cancel (import of )?cancel-before\.txt$/}).click();await textPage.evaluate(()=>window.__qa.release('prefix'));await textRow('cancel-before.txt').locator('.phase-cancelled').waitFor();assert.equal(await textPage.evaluate(()=>window.__qa.events.includes('upload:cancel-before.txt')),false);assert.equal(await textPage.evaluate(()=>window.__qa.networkCalls.length),beforeCancelCalls);assert.equal(await textPage.evaluate(()=>window.__qa.workerCalls),0);await textContext.close();
  pass('cancelling real text preparation before upload performs no import network calls or Worker creation');

  const lifecycle=await browser.newPage({viewport:{width:1440,height:1000}});lifecycle.on('pageerror',error=>browserErrors.push(String(error)));await lifecycle.goto(origin);await lifecycle.getByText('Bring your reading here.',{exact:true}).waitFor();
  const lifecycleRow=name=>lifecycle.locator('.queue-item').filter({has:lifecycle.locator('.queue-open strong').filter({hasText:name})});
  const lifecycleUpload=files=>lifecycle.getByLabel('Choose source files',{exact:true}).setInputFiles(files);
  const lifecycleSaved=name=>lifecycleRow(name).locator('.phase-saved').waitFor();
  const showContainingDetails=async locator=>{await locator.evaluate(el=>{const ancestors=[];for(let parent=el.parentElement;parent;parent=parent.parentElement)if(parent.tagName==='DETAILS'&&!parent.open)ancestors.unshift(parent);for(const detail of ancestors)detail.querySelector(':scope > summary')?.click();});await locator.waitFor({state:'visible'});};
  const library=()=>lifecycle.locator('.library');
  const openLibrary=async()=>{if((await library().getAttribute('open'))===null)await library().locator('summary').first().click();};
  const selectLibrary=async name=>{await openLibrary();await library().locator('.document-item').filter({hasText:name}).click();await lifecycle.waitForFunction(name=>document.querySelector('.reader-heading h2')?.textContent===name,name);};
  const mutationLog=()=>lifecycle.evaluate(()=>window.__qa.operations.map(value=>({...value})));
  const dialog=()=>lifecycle.getByRole('alertdialog');
  const keepDialog=async()=>{await dialog().getByRole('button',{name:'Keep',exact:true}).click();await dialog().waitFor({state:'hidden'});};

  await lifecycleUpload([file('kept.txt','SAVED LIBRARY CONTENT MUST SURVIVE LOCAL REMOVAL')]);await lifecycleSaved('kept.txt');
  const keptId=await lifecycle.evaluate(()=>window.__qa.rows().find(row=>row.original_name==='kept.txt').id);
  await lifecycleRow('kept.txt').getByRole('button',{name:/^Remove/}).click();await lifecycleRow('kept.txt').waitFor({state:'detached'});await openLibrary();
  assert.equal(await library().locator('.document-item').filter({hasText:'kept.txt'}).count(),1);assert.equal(await lifecycle.evaluate(()=>window.__qa.rows().some(row=>row.original_name==='kept.txt')),true);assert.equal((await mutationLog()).filter(value=>value.type.startsWith('delete')).length,0);
  pass('local queue removal preserves saved library documents and never sends DELETE');

  await lifecycle.evaluate(()=>window.__qa.failOnce('upload:unfinished.txt'));await lifecycleUpload([file('unfinished.txt','UNFINISHED LOCAL FILE MUST SURVIVE')]);await lifecycleRow('unfinished.txt').locator('.phase-failed').waitFor();await lifecycle.waitForTimeout(400);
  const beforeRemove=await mutationLog();await lifecycleRow('unfinished.txt').getByRole('button',{name:/^Remove/}).click();await dialog().waitFor();assert.match(await dialog().innerText(),/unfinished recovery copy/);await keepDialog();assert.deepEqual(await mutationLog(),beforeRemove);assert.equal(await lifecycleRow('unfinished.txt').count(),1);
  await lifecycle.evaluate(()=>window.__qa.failOnce('save:unsaved.txt'));await lifecycleUpload([file('unsaved.txt','UNSAVED RESULT MUST SURVIVE')]);await lifecycleRow('unsaved.txt').locator('.phase-failed').waitFor();
  const clearButton=lifecycle.getByRole('button',{name:'Clear saved copies',exact:true,includeHidden:true});await showContainingDetails(clearButton);await lifecycle.waitForTimeout(400);
  const beforeClearCancel=await mutationLog();await clearButton.click();await dialog().waitFor();assert.match(await dialog().innerText(),/Unfinished imports and unsaved results are kept/);await lifecycle.screenshot({path:path.join(output,'storage-clear-confirmation.png')});await keepDialog();assert.deepEqual(await mutationLog(),beforeClearCancel);
  await selectLibrary('kept.txt');const deleteButton=lifecycle.getByRole('button',{name:'Delete saved document',exact:true,includeHidden:true});await showContainingDetails(deleteButton);await lifecycle.waitForTimeout(400);
  const beforeDeleteCancel=await mutationLog();await deleteButton.click();await dialog().waitFor();assert.match(await dialog().innerText(),/kept\.txt/);await keepDialog();assert.deepEqual(await mutationLog(),beforeDeleteCancel);
  pass('Keep cancels local-discard, cache-clear, and document-delete dialogs without writes');

  await lifecycle.evaluate(()=>window.__qa.failOnce('clear'));await showContainingDetails(clearButton);await clearButton.click();await dialog().getByRole('button',{name:'Clear saved copies',exact:true}).click();await dialog().getByRole('alert').waitFor();assert.match(await dialog().innerText(),/Synthetic interrupted clear/);await dialog().getByRole('button',{name:'Clear saved copies',exact:true}).click();await dialog().waitFor({state:'hidden'});
  const recovery=await lifecycle.evaluate(async()=>{const snapshot=window.__qa.snapshot();const unfinished=snapshot.items.find(item=>item.name==='unfinished.txt'),unsaved=snapshot.items.find(item=>item.name==='unsaved.txt');return {unfinished:await unfinished.source.file.text(),unsaved:unsaved.result.text,savePending:unsaved.savePending,rows:window.__qa.rows().map(row=>row.original_name),deletes:window.__qa.operations.filter(op=>op.type.startsWith('delete')).length};});
  assert.equal(recovery.unfinished,'UNFINISHED LOCAL FILE MUST SURVIVE');assert.equal(recovery.unsaved,'UNSAVED RESULT MUST SURVIVE');assert.equal(recovery.savePending,true);assert(recovery.rows.includes('kept.txt'));assert.equal(recovery.deletes,0);
  await lifecycleRow('unsaved.txt').getByRole('button',{name:'Save again',exact:true}).click();await lifecycleSaved('unsaved.txt');
  pass('cache clear failure is retryable and confirmed clear preserves unfinished files/results and saved library');

  await selectLibrary('kept.txt');await selectLibrary('unsaved.txt');await selectLibrary('kept.txt');await showContainingDetails(deleteButton);
  await lifecycle.evaluate(id=>{window.__qa.hold('list');window.__qa.hold('get:'+id);window.__qa.failOnce('delete:'+id);},keptId);
  const requestsBeforeDelete=await lifecycle.evaluate(id=>({list:window.__qa.events.filter(event=>event==='list').length,get:window.__qa.events.filter(event=>event==='get:'+id).length}),keptId);
  await library().locator('.document-item').filter({hasText:'kept.txt'}).click();await library().getByRole('button',{name:'Search',exact:true}).click();await lifecycle.waitForFunction(({before,id})=>window.__qa.events.filter(event=>event==='list').length>before.list&&window.__qa.events.filter(event=>event==='get:'+id).length>before.get,{before:requestsBeforeDelete,id:keptId});
  await showContainingDetails(deleteButton);await deleteButton.click();await dialog().getByRole('button',{name:'Delete saved document',exact:true}).click();await dialog().getByRole('alert').waitFor();assert.match(await dialog().innerText(),/Synthetic interrupted delete/);assert.equal(await lifecycle.evaluate(id=>window.__qa.rows().some(row=>row.id===id),keptId),false);
  await lifecycle.evaluate(id=>{window.__qa.release('list');window.__qa.release('get:'+id);},keptId);await lifecycle.waitForTimeout(100);assert.equal(await library().locator('.document-item').filter({hasText:'kept.txt'}).count(),0);assert.doesNotMatch(await lifecycle.locator('#reader').innerText(),/SAVED LIBRARY CONTENT MUST SURVIVE|Saved privately/);await lifecycle.screenshot({path:path.join(output,'storage-cleanup-pending.png')});
  assert.equal(await lifecycle.evaluate(id=>window.__qa.operations.filter(op=>op.type==='delete-attempt'&&op.id===id).length,keptId),1);await lifecycle.waitForTimeout(400);await lifecycle.reload();await deleteButton.waitFor({state:'visible'});await library().locator('.document-item').filter({hasText:'unsaved.txt'}).waitFor({state:'visible',timeout:10000});assert.match(await library().innerText(),/kept\.txt/);assert.equal(await lifecycle.evaluate(()=>window.__qa.operations.filter(op=>op.type.startsWith('delete')).length),0);assert.equal(await lifecycle.evaluate(id=>window.__qa.snapshot().pendingDeletion.record.id===id,keptId),true);
  await clearButton.click();await dialog().getByRole('button',{name:'Clear saved copies',exact:true}).click();await dialog().waitFor({state:'hidden'});assert.equal(await lifecycle.evaluate(id=>window.__qa.snapshot().pendingDeletion.record.id===id,keptId),true);assert.equal(await lifecycle.evaluate(()=>window.__qa.operations.filter(op=>op.type.startsWith('delete')).length),0);await deleteButton.click();assert.match(await dialog().innerText(),/kept\.txt/);await dialog().getByRole('button',{name:/^(Delete saved document|Retry.*)$/}).click();await dialog().waitFor({state:'hidden'});assert.equal(await lifecycle.evaluate(id=>window.__qa.operations.filter(op=>op.type==='delete-attempt'&&op.id===id).length,keptId),1);assert.equal(await lifecycle.evaluate(id=>window.__qa.operations.some(op=>op.type==='delete-complete'&&op.id===id),keptId),true);
  await lifecycle.goBack();await lifecycle.waitForTimeout(100);await lifecycle.goBack();await lifecycle.waitForTimeout(100);assert.doesNotMatch(await lifecycle.locator('#reader').innerText(),/SAVED LIBRARY CONTENT MUST SURVIVE/);assert.equal(await library().locator('.document-item').filter({hasText:'kept.txt'}).count(),0);assert.equal(await lifecycle.evaluate(id=>window.__qa.snapshot().items.some(item=>item.record?.id===id),keptId),false);
  pass('503 deletion blocks stale reader/list and Back; reload preserves unrelated library and explicit cleanup retry through cache clear');
  await lifecycle.close();

  assert.deepEqual(browserErrors,[]);pass('no browser runtime errors');

  const live=await context.newPage();let liveSite;
  try{const response=await live.goto('https://pdftextract-alpha.junkmail-edu228.chatgpt.site',{waitUntil:'domcontentloaded',timeout:20000});liveSite={status:response?.status(),url:live.url(),title:await live.title(),text:(await live.locator('body').innerText()).slice(0,650),scope:'Read-only unauthenticated navigation; no login bypass or user documents accessed'};await live.screenshot({path:path.join(output,'private-site-access.png')});}catch(error){liveSite={error:String(error),scope:'Read-only unauthenticated navigation failed; no authentication bypass attempted'};}
  await fs.writeFile(path.join(output,'report.json'),JSON.stringify({checks,testedSources,browserVersion:browser.version(),liveSite,browserErrors,evidence:'Actual Chromium/React/CSS, text preparation, import lifecycle and IndexedDB; synthetic extraction and remote API services; not production Site functional validation'},null,2)+'\n');
  await fs.rm(path.join(output,'failure.json'),{force:true});
  console.log(JSON.stringify({passed:checks.length,output,testedSources,liveSite},null,2));
}catch(error){await fs.writeFile(path.join(output,'failure.json'),JSON.stringify({error:String(error),stack:error.stack,checks,browserErrors},null,2)+'\n');throw error;}
finally{await browser?.close();if(server)await new Promise(resolve=>server.close(resolve));await fs.rm(temporary,{recursive:true,force:true});}
