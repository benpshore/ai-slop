// Disposable local Workers bindings only: no deployed Site or user documents.
import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { createRequire, stripTypeScriptTypes } from 'node:module';

const requireWeb = createRequire(new URL('../web/package.json', import.meta.url));
const requireWrangler = createRequire(requireWeb.resolve('wrangler/package.json'));
const { Miniflare, Log, LogLevel } = requireWrangler('miniflare');
async function source(path) {
  return stripTypeScriptTypes((await readFile(new URL(path, import.meta.url), 'utf8')).replace(/^import[^\n]*\n/gm, ''));
}
const production = (await Promise.all([
  source('../web/lib/document-lifecycle.ts'), source('../web/lib/uploads.ts'),
  source('../web/lib/asset-storage.ts'), source('../web/app/api/documents/[id]/assets/route.ts'),
  source('../web/app/api/documents/[id]/route.ts'),
  source('../web/app/api/uploads/route.ts'),
])).map((text, index) => index === 3 ? text.replace('export async function POST(', 'async function capturedAsset(') : text).join('\n');

const fixture = `
import {env} from 'cloudflare:workers';
let holdPoint=null, holdReady=null, releaseHold=null, failDelete=false, listLimit=1000;
let counts={},fixtureOwner='fixture-owner';
function count(name){counts[name]=(counts[name]||0)+1;}
async function barrier(point){
 if(holdPoint!==point)return;
 holdPoint=null;
 const blocked=new Promise(resolve=>{releaseHold=resolve;});holdReady?.();await blocked;
}
function multipartProxy(upload){return new Proxy(upload,{get(target,key){
 if(key==='complete')return async(...args)=>{await barrier('multipart-complete');return target.complete(...args);};
 if(key==='abort')return async(...args)=>{count('abort');return target.abort(...args);};
 const value=Reflect.get(target,key,target);return typeof value==='function'?value.bind(target):value;
}});}
function statementProxy(statement,sql){return new Proxy(statement,{get(target,key){
 if(key==='bind')return(...args)=>statementProxy(target.bind(...args),sql);
 if(key==='run')return async(...args)=>{
  if(sql.startsWith('INSERT OR IGNORE INTO documents'))await barrier('original-insert');
  if(sql.startsWith('UPDATE documents'))await barrier('result-update');
  return target.run(...args);
 };
 const value=Reflect.get(target,key,target);return typeof value==='function'?value.bind(target):value;
}});}
function storage(){return {
 db:new Proxy(env.DB,{get(target,key){
  if(key==='prepare')return sql=>statementProxy(target.prepare(sql),sql);
  const value=Reflect.get(target,key,target);return typeof value==='function'?value.bind(target):value;
 }}),
 bucket:new Proxy(env.BUCKET,{get(target,key){
  if(key==='put')return async(...args)=>{count('put');if(String(args[0]).startsWith('uploads/'))await barrier('receipt-put');return target.put(...args);};
  if(key==='delete')return async(...args)=>{count('delete');if(failDelete){failDelete=false;throw Error('Injected storage cleanup failure');}return target.delete(...args);};
  if(key==='list')return async options=>{count('list');return target.list({...options,limit:listLimit});};
  if(key==='get')return async(...args)=>{count('get');return target.get(...args);};
  if(key==='createMultipartUpload')return async(...args)=>multipartProxy(await target.createMultipartUpload(...args));
  if(key==='resumeMultipartUpload')return(...args)=>multipartProxy(target.resumeMultipartUpload(...args));
  const value=Reflect.get(target,key,target);return typeof value==='function'?value.bind(target):value;
 }})
};}
async function ownedRecord(id,user){const row=await env.DB.prepare('SELECT * FROM documents WHERE id=? AND owner=?').bind(id,user).first();if(!row)throw new Response('Not found',{status:404});return row;}
async function owner(){return fixtureOwner;}
async function boundedBody(request){return new Uint8Array(await request.arrayBuffer());}
function failure(error){return error instanceof Response?error:new Response(error.message,{status:500});}
async function fetchPublicSource(){return {url:'https://fixture.invalid/image.png',response:new Response(new Uint8Array([137,80,78,71,13,10,26,10]),{headers:{'content-type':'image/png'}})};}
${production}
async function invoke(operation){
 if(operation.action==='delete'){await deleteDocument(operation.id,operation.user||fixtureOwner);return {deleted:true};}
 if(operation.action==='delete-route'){
  const response=await DELETE(new Request('https://fixture.test/api/documents/'+operation.id,{method:'DELETE',body:JSON.stringify(operation.body)}),{params:Promise.resolve({id:operation.id})});
  if(!response.ok)throw response;return {deleted:true};
 }
 if(operation.action==='session')return sessionFor(operation.id,operation.user||fixtureOwner);
 if(operation.action==='start-upload'){
  const response=await POST(new Request('https://fixture.test/api/uploads',{method:'POST',body:JSON.stringify({target:'asset',documentId:operation.id,bytes:16,name:'fixture.png',mime:'image/png'})}));
  if(!response.ok)throw response;return response.json();
 }
 if(operation.action==='captured-asset'){
  const response=await capturedAsset(new Request('https://fixture.test/api/documents/'+operation.id+'/assets',{method:'POST',body:JSON.stringify({url:'https://fixture.invalid/image.png'})}),{params:Promise.resolve({id:operation.id})});
  if(!response.ok)throw response;return response.json();
 }
 if(operation.action==='patch'){
  const response=await PATCH(new Request('https://fixture.test/api/documents/'+operation.id,{method:'PATCH',body:JSON.stringify(operation.result)}),{params:Promise.resolve({id:operation.id})});
  if(!response.ok)throw response;return response.json();
 }
 return finishUpload(operation.session,operation.parts);
}
async function outcome(operation){try{return {status:200,value:await invoke(operation)};}catch(error){return {status:error instanceof Response?error.status:500,message:error instanceof Response?await error.text():error.message};}}
export default {async fetch(request){
 const message=await request.json();counts={};fixtureOwner=message.user||'fixture-owner';listLimit=message.listLimit||1000;failDelete=Boolean(message.failDelete);
 if(message.action==='race'){
  const ready=new Promise(resolve=>{holdReady=resolve;});holdPoint=message.point;
  const pending=outcome(message.pending);await ready;
  const deleted=await outcome({action:'delete',id:message.id,user:fixtureOwner});
  releaseHold?.();const completed=await pending;
  return Response.json({deleted,completed,counts});
 }
 return Response.json({...await outcome(message),counts});
}};`;

const mf = new Miniflare({ modules: true, script: fixture, compatibilityDate: '2026-05-15', r2Buckets: ['BUCKET'], d1Databases: ['DB'], log: new Log(LogLevel.ERROR) });
const passed = [];
try {
  const db = await mf.getD1Database('DB'), bucket = await mf.getR2Bucket('BUCKET');
  await db.exec('CREATE TABLE documents (id TEXT PRIMARY KEY,owner TEXT NOT NULL,title TEXT NOT NULL,kind TEXT NOT NULL,source_url TEXT,original_name TEXT NOT NULL,mime TEXT NOT NULL,status TEXT NOT NULL,engine TEXT NOT NULL,sha256 TEXT NOT NULL,bytes INTEGER NOT NULL,created_at TEXT NOT NULL,search_text TEXT NOT NULL,result_key TEXT)');
  await db.exec('CREATE TABLE document_deletions (id TEXT PRIMARY KEY,owner TEXT NOT NULL)');
  const post = async message => {
    const response = await mf.dispatchFetch('https://lifecycle-fixture.test/', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(message) });
    assert.equal(response.status, 200, await response.clone().text());
    return response.json();
  };
  const encoder = new TextEncoder(), owner = 'fixture-owner';
  const result = {title:'Fixture result',text:'Fixture content',links:[],warnings:[],status:'partial',engine:'Lifecycle fixture'};
  async function upload(documentId, target = 'original', sessionOwner = owner) {
    const id = randomUUID(), key = `${documentId}/${target === 'original' ? 'original' : `${target === 'asset' ? 'assets' : 'results'}/${id}`}`;
    const bytes = encoder.encode(target === 'original' ? '%PDF-1.7\nBenign local fixture\n' : target === 'result' ? JSON.stringify(result) : 'benign image fixture');
    const multipart = await bucket.createMultipartUpload(key), part = await multipart.uploadPart(1, bytes);
    const current = await db.prepare('SELECT result_key FROM documents WHERE id=?').bind(documentId).first();
    const session = {owner:sessionOwner,id,documentId,key,uploadId:multipart.uploadId,target,bytes:bytes.length,createdAt:'2026-10-04T00:00:00Z',name:'fixture.pdf',kind:'pdf',sourceUrl:null,mime:'application/pdf',baseResultKey:current?.result_key??null,...(target === 'result' ? {summary:{title:result.title,status:result.status,engine:result.engine,searchText:result.text}} : {})};
    await bucket.put(`uploads/${id}`, JSON.stringify(session));
    return {session,parts:[part]};
  }
  async function document(sessionOwner = owner) {
    const id = randomUUID(), original = await upload(id, 'original', sessionOwner);
    assert.equal((await post(original)).status, 200);
    return {id,original};
  }
  async function assertDeleted(id, sessionIds = []) {
    assert.equal(await db.prepare('SELECT id FROM documents WHERE id=?').bind(id).first(), null);
    assert.ok(await db.prepare('SELECT id FROM document_deletions WHERE id=?').bind(id).first());
    assert.equal((await bucket.list({prefix:`${id}/`})).objects.length, 0, 'No deleted document objects remain');
    for (const id of sessionIds) assert.equal(await bucket.head(`uploads/${id}`), null, 'No deleted document receipt remains');
    for(const object of (await bucket.list({prefix:'uploads/'})).objects){
      const receipt=await (await bucket.get(object.key)).json();
      assert.notEqual(receipt.documentId,id,'No late-created receipt remains for the deleted document');
    }
  }
  const neighbor = await document('neighbor-owner');
  const target = await document(), targetResult = await upload(target.id, 'result'), targetAsset = await upload(target.id, 'asset');
  assert.equal((await post(targetResult)).status, 200);
  assert.equal((await post(targetAsset)).status, 200);
  const foreign = await post({action:'delete',id:target.id,user:'foreign-owner'});
  assert.equal(foreign.status, 404);assert.deepEqual(foreign.counts, {}, 'Foreign-owner rejection performs no R2 operations');
  assert.ok(await bucket.head(`${target.id}/original`));
  passed.push('foreign_owner_no_r2_access');
  for(const body of [{},{confirmDocumentId:neighbor.id}]){
    const unconfirmed=await post({action:'delete-route',id:target.id,body});
    assert.equal(unconfirmed.status,400);assert.deepEqual(unconfirmed.counts,{});
  }
  assert.ok(await db.prepare('SELECT id FROM documents WHERE id=?').bind(target.id).first());
  passed.push('route_requires_exact_document_confirmation');
  const completedOriginal = JSON.parse(await (await bucket.get(`uploads/${target.original.session.id}`)).text());
  const completedAsset = JSON.parse(await (await bucket.get(`uploads/${targetAsset.session.id}`)).text());
  for(let index=0;index<7;index++)await bucket.put(`${target.id}/assets/extra-${index}`, 'fixture');
  const deleted = await post({action:'delete',id:target.id,listLimit:2});
  assert.equal(deleted.status, 200, deleted.message);assert.ok(deleted.counts.list > 2, 'Cleanup traverses multiple storage pages');
  await assertDeleted(target.id, [target.original.session.id,targetResult.session.id,targetAsset.session.id]);
  assert.ok(await bucket.head(`${neighbor.id}/original`));
  assert.ok(await bucket.head(`uploads/${neighbor.original.session.id}`));
  assert.ok(await db.prepare('SELECT id FROM documents WHERE id=?').bind(neighbor.id).first());
  assert.equal((await post({action:'delete',id:target.id})).status, 200, 'Authorized repeat deletion is idempotent');
  assert.equal((await post({action:'delete',id:target.id,user:'foreign-owner'})).status, 404);
  for(const session of [target.original.session,completedOriginal,completedAsset])assert.equal((await post({session,parts:target.original.parts})).status, 410, 'Late original/asset completion cannot revive a deleted document');
  passed.push('paginated_cleanup_owner_and_document_scope','completed_receipts_cannot_resurrect');

  const originalRace = await document();
  const originalOutcome = await post({action:'race',id:originalRace.id,point:'original-insert',pending:originalRace.original});
  assert.equal(originalOutcome.deleted.status, 200, originalOutcome.deleted.message);
  assert.equal(originalOutcome.completed.status, 404, originalOutcome.completed.message);
  await assertDeleted(originalRace.id,[originalRace.original.session.id]);
  passed.push('inflight_original_insert_cannot_resurrect');

  for (const target of ['result','asset']) {
    const entry = await document(), pending = await upload(entry.id, target);
    const raced = await post({action:'race',id:entry.id,point:'receipt-put',pending});
    assert.equal(raced.deleted.status, 200, raced.deleted.message);
    assert.equal(raced.completed.status, 410, raced.completed.message);
    await assertDeleted(entry.id,[entry.original.session.id,pending.session.id]);
    passed.push(`late_${target}_receipt_is_removed`);
  }
  const patchEntry = await document();
  const patchOutcome = await post({action:'race',id:patchEntry.id,point:'result-update',pending:{action:'patch',id:patchEntry.id,result}});
  assert.equal(patchOutcome.deleted.status, 200, patchOutcome.deleted.message);assert.equal(patchOutcome.completed.status, 409);
  await assertDeleted(patchEntry.id,[patchEntry.original.session.id]);
  passed.push('legacy_patch_cannot_republish_deleted_record');
  const captureEntry = await document();
  const captureOutcome = await post({action:'race',id:captureEntry.id,point:'multipart-complete',pending:{action:'captured-asset',id:captureEntry.id}});
  assert.equal(captureOutcome.deleted.status, 200, captureOutcome.deleted.message);assert.equal(captureOutcome.completed.status, 404);
  await assertDeleted(captureEntry.id,[captureEntry.original.session.id]);
  passed.push('late_captured_asset_is_removed');
  const creationEntry = await document();
  const creationOutcome = await post({action:'race',id:creationEntry.id,point:'receipt-put',pending:{action:'start-upload',id:creationEntry.id}});
  assert.equal(creationOutcome.deleted.status,200,creationOutcome.deleted.message);assert.equal(creationOutcome.completed.status,410);
  assert.equal(creationOutcome.counts.abort,1,'Late session creation aborts its multipart upload');
  await assertDeleted(creationEntry.id,[creationEntry.original.session.id]);
  passed.push('late_session_creation_is_aborted_and_removed');

  const retryEntry = await document();
  const interrupted = await post({action:'delete',id:retryEntry.id,failDelete:true});
  assert.equal(interrupted.status, 503);assert.match(interrupted.message,/cleanup is incomplete/i);
  assert.equal(await db.prepare('SELECT id FROM documents WHERE id=?').bind(retryEntry.id).first(), null, 'Cleanup failure does not restore searchable metadata');
  assert.ok(await db.prepare('SELECT id FROM document_deletions WHERE id=?').bind(retryEntry.id).first());
  assert.equal((await post(retryEntry.original)).status, 410, 'Cleanup retry window cannot restore original');
  assert.equal((await post({action:'delete',id:retryEntry.id})).status, 200);
  await assertDeleted(retryEntry.id,[retryEntry.original.session.id]);
  passed.push('partial_cleanup_remains_deleted_and_retryable');
  console.log(JSON.stringify({scope:'disposable local Miniflare D1/R2, synthetic fixtures only',checks:passed}));
} finally { await mf.dispose(); }
