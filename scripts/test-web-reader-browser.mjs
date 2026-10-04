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
 * React, DOMPurify, Button, and CSS are real. Extraction, upload, remote document
 * APIs and recovery storage are synthetic mocks. This does not validate PDF/OCR
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
const rows = new Map(), results = new Map(), originals = new Map();
let nextId=0, snapshot=null;
const gates=new Map(), failures=new Map();
const qa=window.__qa={events:[],rows:()=>[...rows.values()],snapshot:()=>snapshot,
 hold:(key)=>{let resolve;const promise=new Promise(done=>resolve=done);gates.set(key,{promise,resolve});},
 release:key=>{gates.get(key)?.resolve();gates.delete(key);},
 failOnce:key=>failures.set(key,true),
 seed:async(name,source)=>{const record=await uploadOriginal(new File([source],name,{type:'text/html'}),{});await saveExtracted(record,clipHtml(source,'https://fixture.invalid/',name));return record.id;}};
async function stage(key,signal){
 qa.events.push(key);signal?.throwIfAborted();
 if(failures.delete(key))throw Error('Synthetic interrupted '+key);
 const gate=gates.get(key);if(!gate)return;
 await new Promise((resolve,reject)=>{const aborted=()=>reject(new DOMException('Cancelled','AbortError'));signal?.addEventListener('abort',aborted,{once:true});gate.promise.then(()=>{signal?.removeEventListener('abort',aborted);resolve();});});
 signal?.throwIfAborted();
}
window.fetch=async(input,options={})=>{
 const url=new URL(String(input),location.origin);
 if(url.pathname==='/api/documents')return Response.json({documents:[...rows.values()]});
 const id=url.pathname.split('/')[3];
 if(url.pathname.endsWith('/original'))return new Response(originals.get(id)||'Synthetic original');
 if(rows.has(id))return Response.json({record:rows.get(id),result:results.get(id)||null});
 throw Error('Unexpected synthetic request: '+url.pathname);
};
window.Worker=class {constructor(){throw Error('PDF workers are outside this synthetic reader QA');}};
export async function uploadOriginal(file,{signal,onProgress}){
 onProgress?.(.25);await stage('upload:'+file.name,signal);
 const source=await file.text(),id='fixture-'+(++nextId);
 const row={id,title:file.name,original_name:file.name,kind:source.startsWith('<')?'html':'text',mime:file.type,status:'uploaded',engine:'',created_at:'2026-10-04T00:00:00Z',sha256:'synthetic-checksum-hidden-in-details',bytes:file.size,source_url:null};
 rows.set(id,row);originals.set(id,source);onProgress?.(1);return row;
}
export async function decodeSource(file,type,signal){await stage('decode:'+file.name,signal);return file.text();}
export async function saveExtracted(record,result,{signal,onProgress}={}){
 onProgress?.(.5);await stage('save:'+record.original_name,signal);
 results.set(record.id,result);rows.set(record.id,{...record,title:result.title,status:result.status});onProgress?.(1);
}
export const captureSource=()=>{throw Error('Unexpected URL capture');};
export const uploadAssetFile=()=>{throw Error('Unexpected asset upload');};
export async function readWorkspace(){return snapshot;}
export async function writeWorkspace(owner,value){if(owner!=='synthetic-owner')throw Error('Unexpected owner');snapshot=value;}
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
  const testedSources={};for(const name of ['web/app/workspace.tsx','web/app/globals.css'])testedSources[name]=createHash('sha256').update(await fs.readFile(path.join(root,name))).digest('hex');
  const entry = path.join(temporary, 'entry.tsx');
  await fs.writeFile(entry, `import React from 'react';import {createRoot} from 'react-dom/client';import Workspace from ${JSON.stringify(path.join(web, 'app/workspace.tsx'))};createRoot(document.getElementById('root')!).render(<Workspace userId="synthetic-owner"/>);`);
  await build({entryPoints:[entry],outfile:path.join(temporary,'bundle.js'),bundle:true,format:'iife',platform:'browser',jsx:'automatic',nodePaths:[path.join(web,'node_modules')],define:{'process.env.NODE_ENV':'"test"'},plugins:[{name:'synthetic-services',setup(builder){builder.onResolve({filter:/^@\//},args=>{if(mocked.has(args.path.replace('@/lib/','')))return {path:'fixture',namespace:'synthetic'};return {path:path.join(web,args.path.slice(2)+(args.path.startsWith('@/components/')?'.tsx':'.ts'))};});builder.onLoad({filter:/.*/,namespace:'synthetic'},()=>({contents:fixture,loader:'js'}));}}]});
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

  await hold('upload:cancel.txt');await upload([file('cancel.txt')]);await visible(row('cancel.txt').locator('.phase-uploading'));await page.getByRole('button',{name:'Cancel cancel.txt',exact:true}).click();await visible(row('cancel.txt').locator('.phase-cancelled'));
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

  const mobile=await context.newPage();mobile.on('pageerror',error=>browserErrors.push(String(error)));await mobile.setViewportSize({width:390,height:844});await mobile.goto(origin);await mobile.getByText('Bring your reading here.',{exact:true}).waitFor();
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
  assert.deepEqual(browserErrors,[]);pass('no browser runtime errors');

  const live=await context.newPage();let liveSite;
  try{const response=await live.goto('https://pdftextract-alpha.junkmail-edu228.chatgpt.site',{waitUntil:'domcontentloaded',timeout:20000});liveSite={status:response?.status(),url:live.url(),title:await live.title(),text:(await live.locator('body').innerText()).slice(0,650),scope:'Read-only unauthenticated navigation; no login bypass or user documents accessed'};await live.screenshot({path:path.join(output,'private-site-access.png')});}catch(error){liveSite={error:String(error),scope:'Read-only unauthenticated navigation failed; no authentication bypass attempted'};}
  await fs.writeFile(path.join(output,'report.json'),JSON.stringify({checks,testedSources,browserVersion:browser.version(),liveSite,browserErrors,evidence:'Actual Chromium/React/CSS; synthetic extraction, network and persistence; not production Site functional validation'},null,2)+'\n');
  await fs.rm(path.join(output,'failure.json'),{force:true});
  console.log(JSON.stringify({passed:checks.length,output,testedSources,liveSite},null,2));
}catch(error){await fs.writeFile(path.join(output,'failure.json'),JSON.stringify({error:String(error),stack:error.stack,checks,browserErrors},null,2)+'\n');throw error;}
finally{await browser?.close();if(server)await new Promise(resolve=>server.close(resolve));await fs.rm(temporary,{recursive:true,force:true});}
