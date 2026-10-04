/** Synthetic local Chromium + Node benchmark. Never contacts the private Site.
 * PLAYWRIGHT_MODULE=/path/to/playwright/index.mjs CHROMIUM_PATH=/usr/bin/chromium
 *   node scripts/benchmark-text-import.mjs > ../docs/validation/text-import-benchmark.json
 * Baseline is frozen PR175; candidate applies only the documented integration
 * patch to a temporary copy, leaving the UX owner's workspace.tsx untouched.
 */
import assert from 'node:assert/strict';
import {createRequire} from 'node:module';
import {createServer} from 'node:http';
import {createHash} from 'node:crypto';
import {execFileSync} from 'node:child_process';
import {mkdtemp, mkdir, readFile, writeFile, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {dirname, join, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';

const web=resolve(dirname(fileURLToPath(import.meta.url)),'..'), repo=resolve(web,'..');
const require=createRequire(import.meta.url);
const {build}=require(require.resolve('esbuild',{paths:[require.resolve('vite')]}));
const baseline='baafb472873750e7e84d32c3c8e6fa7a680e37db';
const temporary=await mkdtemp(join(tmpdir(),'tpe-text-latency-'));
const source=execFileSync('git',['show',`${baseline}:web/app/workspace.tsx`],{cwd:repo,encoding:'utf8'});
await mkdir(join(temporary,'web/app'),{recursive:true});
await writeFile(join(temporary,'web/app/workspace.tsx'),source);
execFileSync('git',['apply',join(repo,'docs/integration/text-first-workspace.patch')],{cwd:temporary});
const candidate=await readFile(join(temporary,'web/app/workspace.tsx'),'utf8');
const hashes={baseline:createHash('sha256').update(source).digest('hex'),candidate:createHash('sha256').update(candidate).digest('hex'),helper:createHash('sha256').update(await readFile(join(web,'lib/text-import.ts'))).digest('hex')};
const bundles={};
for(const [variant,workspace] of Object.entries({baseline:source,candidate})){
  const outdir=join(temporary,variant);
  const result=await build({stdin:{contents:"import React from 'react';import{createRoot}from'react-dom/client';import Workspace from 'fixture-workspace';createRoot(document.getElementById('root')).render(React.createElement(Workspace,{userId:'synthetic-owner'}));",resolveDir:web,sourcefile:'entry.tsx'},outdir,bundle:true,write:true,format:'esm',splitting:true,platform:'browser',minify:true,metafile:true,tsconfig:join(web,'tsconfig.json'),define:{'process.env.NODE_ENV':'"production"'},plugins:[{name:'fixture-workspace',setup(builder){
    builder.onResolve({filter:/^fixture-workspace$/},()=>({path:'workspace.tsx',namespace:'fixture'}));
    builder.onLoad({filter:/.*/,namespace:'fixture'},()=>({contents:workspace,loader:'tsx',resolveDir:join(web,'app')}));
    builder.onLoad({filter:/\/lib\/(?:upload-client|text-import)\.ts$/},async args=>{
      let contents=await readFile(args.path,'utf8');
      for(const name of args.path.endsWith('upload-client.ts')?['detectFileKind','decodeSource']:['prepareTextImport']){
        contents=contents.replace(`export async function ${name}`,`async function measured_${name}`);
        contents+=`\nexport async function ${name}(...args:Parameters<typeof measured_${name}>){const start=performance.now();try{return await measured_${name}(...args);}finally{(globalThis as any).__metrics?.parses.push({name:'${name}',start,end:performance.now()});}}`;
      }
      return {contents,loader:'ts',resolveDir:dirname(args.path)};
    });
  }}]});
  bundles[variant]={entryBytes:Object.entries(result.metafile.outputs).find(([path])=>path.endsWith('/stdin.js'))[1].bytes,totalBytes:Object.values(result.metafile.outputs).reduce((sum,out)=>sum+out.bytes,0)};
}

// Node times include local module evaluation separately from preparation. This
// measures no browser rendering or network and is not a hosted latency result.
const built=await build({entryPoints:[join(web,'lib/text-import.ts')],bundle:true,write:false,format:'esm',platform:'node'});
const moduleUrl=`data:text/javascript;base64,${Buffer.from(built.outputFiles[0].text).toString('base64')}`;
const nodeSamples=[];
for(let i=0;i<10;i++){
  const begin=performance.now();const helper=await import(moduleUrl+`#cold-${i}`);const moduleMs=performance.now()-begin;
  const cold=await helper.prepareTextImport(new File(['some text'],'Pasted text.txt',{type:'text/plain'}));
  const warm=await helper.prepareTextImport(new File(['some text'],'Pasted text.txt',{type:'text/plain'}));
  assert.equal(cold.result.markdown,'some text');nodeSamples.push({moduleMs,cold:cold.timings,warm:warm.timings});
}

let active={delay:0,fail:null,requests:[],sessions:new Map(),records:new Map(),results:new Map()};
const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
const server=createServer(async(req,res)=>{
  try{
    const url=new URL(req.url,'http://localhost');
    if(url.pathname==='/blank'){res.setHeader('Content-Type','text/html');res.end('<!doctype html><title>Synthetic browser reset</title>');return;}
    if(url.pathname==='/'){
      res.setHeader('Content-Type','text/html');res.end(`<!doctype html><meta charset="utf-8"><div id="root"></div><script>
      window.__metrics={parses:[],network:[],workers:[],wasm:[],started:null,readableMs:null,renderObservationMs:null};
      const originalFetch=window.fetch;window.fetch=async(...args)=>{const start=performance.now();const response=await originalFetch(...args);const end=performance.now();window.__metrics.network.push({url:String(args[0]),method:args[1]?.method||'GET',start,end,status:response.status});return response;};
      const OriginalWorker=window.Worker;window.Worker=new Proxy(OriginalWorker,{construct(target,args){window.__metrics.workers.push(String(args[0]));return Reflect.construct(target,args);}});
      for(const key of ['instantiate','instantiateStreaming','compile','compileStreaming']){const original=WebAssembly[key];if(original)WebAssembly[key]=function(...args){window.__metrics.wasm.push(key);return original.apply(this,args);};}
      document.addEventListener('submit',()=>{const m=window.__metrics;m.started=performance.now();m.readableMs=null;m.renderObservationMs=null;m.parses=[];m.network=[];},true);
      new MutationObserver(()=>{const m=window.__metrics;if(m.started!==null&&m.readableMs===null&&document.querySelector('.reading')?.textContent==='some text'){m.readableMs=performance.now()-m.started;requestAnimationFrame(()=>requestAnimationFrame(()=>m.renderObservationMs=performance.now()-m.started));}}).observe(document.getElementById('root'),{subtree:true,childList:true,characterData:true});
      const start=performance.now();import('/${url.searchParams.get('variant')||'baseline'}/stdin.js').then(()=>window.__metrics.moduleMs=performance.now()-start);
      </script>`);return;
    }
    if(/^\/(baseline|candidate)\/[^/]+\.js$/.test(url.pathname)){
      res.setHeader('Content-Type','text/javascript');res.setHeader('Cache-Control','public, max-age=3600');res.end(await readFile(join(temporary,url.pathname)));return;
    }
    if(!url.pathname.startsWith('/api/')){res.statusCode=404;res.end();return;}
    const log={path:url.pathname,method:req.method,started:performance.now(),bodyReadMs:0,hashMs:0,fixturePersistenceMs:0,injectedDelayMs:0};
    active.requests.push(log);const begin=performance.now();const chunks=[];for await(const chunk of req)chunks.push(chunk);const bytes=Buffer.concat(chunks);log.bodyReadMs=performance.now()-begin;
    let value={},status=200;
    if(url.pathname==='/api/documents')value={documents:[...active.records.values()]};
    else if(url.pathname==='/api/uploads'){
      const metadata=JSON.parse(bytes);log.target=metadata.target;
      if(active.fail===metadata.target){active.fail=null;status=503;value={error:'Synthetic storage interruption'};}
      else{const session=String(active.sessions.size+1);active.sessions.set(session,{metadata,parts:[]});value={session,chunkSize:8*1024*1024};}
    }else if(url.pathname.startsWith('/api/uploads/')){
      const id=url.pathname.split('/').at(-1),session=active.sessions.get(id);assert(session);log.target=session.metadata.target;
      if(req.method==='PUT'){session.parts.push(bytes);value={partNumber:Number(url.searchParams.get('part')),etag:'synthetic'};}
      else if(req.method==='POST'){
        const start=performance.now(),contents=Buffer.concat(session.parts);
        if(session.metadata.target==='original'){
          const hashStart=performance.now(),sha256=createHash('sha256').update(contents).digest('hex');log.hashMs=performance.now()-hashStart;
          value={id:'document-'+id,title:session.metadata.name,kind:session.metadata.kind,original_name:session.metadata.name,mime:session.metadata.mime,sha256,bytes:contents.length,status:'uploaded',engine:'',created_at:new Date().toISOString(),source_url:null};active.records.set(value.id,value);
        }else{active.results.set(session.metadata.documentId,JSON.parse(contents));value={saved:true};}
        session.completed=value;log.fixturePersistenceMs=performance.now()-start-log.hashMs;
      }else if(req.method==='DELETE'){value={};}else value={completed:session.completed||null};
    }else{const id=url.pathname.split('/')[3];value={record:active.records.get(id),result:active.results.get(id)||null};}
    if(log.target==='original'&&active.delay){log.injectedDelayMs=active.delay;await sleep(active.delay);}
    log.ended=performance.now();res.statusCode=status;res.setHeader('Content-Type','application/json');res.setHeader('Server-Timing',`fixture-hash;dur=${log.hashMs}, fixture-persistence;dur=${log.fixturePersistenceMs}, injected-delay;dur=${log.injectedDelayMs}`);res.end(JSON.stringify(value));
  }catch(error){res.statusCode=500;res.end(JSON.stringify({error:String(error)}));}
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const origin=`http://127.0.0.1:${server.address().port}`;
let browser;
try{
  const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
  browser=await chromium.launch({headless:true,...(process.env.CHROMIUM_PATH?{executablePath:process.env.CHROMIUM_PATH}:{}),args:['--no-sandbox']});
  const samples=[],checks=[];
  async function run(variant,{delay=0,fail=null,warm=false,context=null,cancelDuringRead=false}={}){
    active={delay,fail,requests:[],sessions:new Map(),records:new Map(),results:new Map()};
    const ownContext=!context;context ||= await browser.newContext();const page=await context.newPage();const external=[];
    page.on('request',request=>{if(!request.url().startsWith(origin+'/'))external.push(request.url());});
    await page.goto(origin+'/blank');
    await page.evaluate(()=>new Promise((resolve,reject)=>{const request=indexedDB.deleteDatabase('tpe-private-workspace');request.onsuccess=()=>resolve();request.onerror=()=>reject(request.error);}));
    if(warm){await page.goto(origin+'/?variant='+variant);await page.locator('#source-paste').waitFor();}
    await page.goto(origin+'/?variant='+variant);await page.locator('#source-paste').waitFor();
    if(cancelDuringRead)await page.evaluate(()=>{const original=Blob.prototype.arrayBuffer;Blob.prototype.arrayBuffer=function(){const read=original.bind(this);return new Promise(resolve=>{window.__readHeld=true;window.__releaseRead=()=>resolve(read());});};});
    await page.locator('#source-paste').fill('some text');await page.getByRole('button',{name:'Import pasted source',exact:true}).click();
    if(cancelDuringRead){
      await page.waitForFunction(()=>window.__readHeld);
      await page.getByRole('button',{name:'Cancel Pasted text.txt',exact:true}).click();await page.evaluate(()=>window.__releaseRead());
      await page.locator('.queue-phase.phase-cancelled').waitFor();assert.equal(active.requests.filter(x=>x.path==='/api/uploads').length,0);
      checks.push('Cancel during text preparation aborts the active job before any upload.');await page.close();if(ownContext)await context.close();return;
    }
    if(delay&&variant==='candidate'){
      await page.locator('.reading').waitFor();assert.equal(await page.locator('.reader-heading [role=status]').textContent(),'Text ready. Saving…');
      assert.equal(await page.getByRole('button',{name:'Retry save',exact:true}).count(),0);
      assert.equal(active.results.size,0);checks.push('Exact readable output appears while original storage is pending, with saving status and no premature retry prompt.');
    }
    if(fail){
      await page.locator('.queue-phase.phase-failed').waitFor();assert.equal(await page.locator('.reading').textContent(),'some text');
      // Recovery storage remains the app's actual IndexedDB, not a mock.
      await page.waitForTimeout(300);await page.reload();await page.locator('.reading').waitFor();assert.equal(await page.locator('.reading').textContent(),'some text');
      await page.getByRole('button',{name:'Retry save',exact:true}).click();
    }
    await page.locator('.queue-phase.phase-saved').waitFor({timeout:30000});
    if(!fail)await page.waitForFunction(()=>window.__metrics.renderObservationMs!==null);
    const metrics=await page.evaluate(()=>({...window.__metrics,moduleResources:performance.getEntriesByType('resource').filter(x=>x.name.endsWith('.js')).map(x=>({path:new URL(x.name).pathname,duration:x.duration,transferSize:x.transferSize,encodedBodySize:x.encodedBodySize})),resources:performance.getEntriesByType('resource').filter(x=>x.name.includes('/api/uploads')).map(x=>({name:new URL(x.name).pathname,startTime:x.startTime,duration:x.duration,requestStart:x.requestStart,responseStart:x.responseStart,responseEnd:x.responseEnd,serverTiming:x.serverTiming.map(t=>({name:t.name,duration:t.duration}))}))}));
    assert.equal(await page.locator('.reading').textContent(),'some text');assert.deepEqual(metrics.workers,[]);assert.deepEqual(metrics.wasm,[]);assert.deepEqual(external,[]);
    // Export the actual retained Markdown from the actual component.
    await page.getByText('Download',{exact:true}).click();const downloadEvent=page.waitForEvent('download');await page.getByRole('button',{name:'Markdown',exact:true}).click();const download=await downloadEvent;assert.equal(await readFile(await download.path(),'utf8'),'some text');
    if(fail==='result')assert.equal(active.requests.filter(x=>x.path==='/api/uploads'&&x.target==='original').length,1);
    if(fail)checks.push(`${fail} save failure: exact readable/exportable result survives IndexedDB reload; retry succeeds${fail==='result'?' without duplicate original':''}`);
    else samples.push({variant,warm,delayPerOriginalRequestMs:delay,...metrics,serverRequests:active.requests});
    await page.close();if(ownContext)await context.close();
  }
  for(const variant of ['baseline','candidate']){
    for(let i=0;i<3;i++)await run(variant);
    const context=await browser.newContext();for(let i=0;i<3;i++)await run(variant,{warm:true,context});await context.close();
    await run(variant,{delay:3333});
  }
  await run('candidate',{fail:'original'});await run('candidate',{fail:'result'});await run('candidate',{cancelDuringRead:true});
  checks.push('All measured text imports create zero Workers and invoke zero WebAssembly compilation/instantiation APIs; exact Markdown downloads match input.');
  const report={recordedAt:new Date().toISOString(),baseline,hashes,runtime:{node:process.version,chromium:browser.version(),platform:process.platform},bundles,definitions:{cold:'New Chromium context; empty HTTP cache and IndexedDB. Timed import starts at form submit after hydration. Module download/evaluation is recorded separately.',warm:'Preloaded module in same browser context with local browser caches; timed import starts at form submit after navigation/hydration.',readableMs:'Form submit capture to MutationObserver finding exact text in actual React reader. Not a compositor paint timestamp.',renderObservationMs:'Form submit to second requestAnimationFrame after DOM text observed. Rendering opportunity, not device display/compositor proof.',network:'fetch dispatch to response headers plus Resource Timing response completion. Includes injected fixture/server work; do not sum overlapping server/client durations.',hashing:'Real SHA-256 of synthetic original bytes in local Node fixture server, inside original commit; not production Workers DigestStream cost.',persistence:'In-memory local fixture record/result update and acknowledgement. No R2/D1 or durable network storage measured.',delay:'3333 ms intentionally added to each of three original storage responses; demonstrates dependency, does not reproduce/attribute the reported hosted ten seconds.',pdfOcr:'Worker construction and WebAssembly API counters are observed, not assigned zero-duration guesses.'},limitations:['Private Site authentication unavailable in this task; no live Site timing or deployment.','Desktop headless Chromium only; no iPhone/iPad Safari measurements.','Baseline and candidate bundle actual workspace/upload-client code; authentication/server storage endpoints use synthetic local HTTP fixture.','Node cold means fresh module identity within one process; does not include OS process startup.','Suggested integration patch is applied only in temporary benchmark source; UX owner must integrate and revalidate.'],nodeSamples,browserSamples:samples,checks};
  console.log(JSON.stringify(report,null,2));
}finally{await browser?.close();await new Promise(resolve=>server.close(resolve));await rm(temporary,{recursive:true,force:true});}
