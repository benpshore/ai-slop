// Disposable real Workers bindings; no Site, private bucket or user data is used.
import assert from 'node:assert/strict';
import { createHash, randomUUID } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { createRequire, stripTypeScriptTypes } from 'node:module';
import { createDocumentStore } from '../web/lib/mcp.ts';

const requireWeb = createRequire(new URL('../web/package.json', import.meta.url));
const requireWrangler = createRequire(requireWeb.resolve('wrangler/package.json'));
const { Miniflare, Log, LogLevel } = requireWrangler('miniflare');
const lifecycle = await readFile(new URL('../web/lib/document-lifecycle.ts', import.meta.url), 'utf8');
let source = await readFile(new URL('../web/lib/uploads.ts', import.meta.url), 'utf8');
source = stripTypeScriptTypes(`${lifecycle}\n${source}`.replace(/^import[^\n]*\n/gm, ''));
const fixture = `
import {env} from 'cloudflare:workers';
let failReceipt=false,pauseHead=false,headReady=null,releaseHead=null;
function storage(){return {db:env.DB,bucket:new Proxy(env.BUCKET,{get(target,key){
 if(key==='put')return async(...args)=>{if(failReceipt&&String(args[0]).startsWith('uploads/')){failReceipt=false;throw new Error('simulated receipt write failure');}return target.put(...args);};
 if(key==='head')return async(...args)=>{const object=await target.head(...args);if(pauseHead){pauseHead=false;const held=new Promise(resolve=>{releaseHead=resolve;});headReady?.();await held;}return object;};
 const value=Reflect.get(target,key,target);return typeof value==='function'?value.bind(target):value;
 }})};}
async function ownedRecord(id,user){const row=await env.DB.prepare('SELECT * FROM documents WHERE id=? AND owner=?').bind(id,user).first();if(!row)throw new Response('Not found',{status:404});return row;}
${source}
export default {async fetch(request){const message=await request.json();try{
 if(message.action==='race'){
  const ready=new Promise(resolve=>{headReady=resolve;});pauseHead=true;
  const older=finishUpload(message.older.session,message.older.parts).then(()=>({status:200}),error=>({status:error instanceof Response?error.status:500,message:error instanceof Response?'conflict':error.message}));
  await ready;
  try{await finishUpload(message.newer.session,message.newer.parts);}finally{releaseHead?.();}
  return Response.json(await older);
 }
 if(message.action==='session')return Response.json(await sessionFor(message.id,message.user));
 failReceipt=Boolean(message.failReceipt);pauseHead=Boolean(message.pauseHead);
 return Response.json(await finishUpload(message.session,message.parts));
}catch(error){if(error instanceof Response)return error;return new Response(error.message,{status:500});}}};`;
const mf = new Miniflare({ modules: true, script: fixture, compatibilityDate: '2026-05-15', r2Buckets: ['BUCKET'], d1Databases: ['DB'], log: new Log(LogLevel.ERROR) });
try {
  const db = await mf.getD1Database('DB'), bucket = await mf.getR2Bucket('BUCKET');
  await db.exec('CREATE TABLE documents (id TEXT PRIMARY KEY,owner TEXT NOT NULL,title TEXT NOT NULL,kind TEXT NOT NULL,source_url TEXT,original_name TEXT NOT NULL,mime TEXT NOT NULL,status TEXT NOT NULL,engine TEXT NOT NULL,sha256 TEXT NOT NULL,bytes INTEGER NOT NULL,created_at TEXT NOT NULL,search_text TEXT NOT NULL,result_key TEXT)');
  await db.exec('CREATE TABLE document_deletions (id TEXT PRIMARY KEY, owner TEXT NOT NULL)');
  const post = message => mf.dispatchFetch('https://upload-fixture.test/', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(message) });
  const documentId = randomUUID(), owner = 'fixture-owner';
  async function upload(target, bytes, baseResultKey = null) {
    const id = randomUUID(), key = target === 'original' ? `${documentId}/original` : `${documentId}/results/${id}`;
    const multipart = await bucket.createMultipartUpload(key, { httpMetadata: { contentType: target === 'original' ? 'application/pdf' : 'application/json' } });
    const part = await multipart.uploadPart(1, bytes);
    const session = { owner, id, documentId, key, uploadId: multipart.uploadId, target, bytes: bytes.length, createdAt: '2026-10-04T00:00:00Z', name: 'fixture.pdf', kind: 'pdf', sourceUrl: null, mime: 'application/pdf', baseResultKey, ...(target === 'result' ? { summary: { title: `result ${id}`, engine: 'contract fixture', status: 'partial', searchText: 'saved fixture text' } } : {}) };
    await bucket.put(`uploads/${id}`, JSON.stringify(session));
    return { session, parts: [part] };
  }
  const encoder = new TextEncoder(), bytes = encoder.encode('%PDF-1.7\noriginal fixture bytes\n');
  const original = await upload('original', bytes);
  const saved = await post(original); assert.equal(saved.status, 200, await saved.clone().text());
  const receipt = await saved.json(); assert.equal(receipt.sha256, createHash('sha256').update(bytes).digest('hex'));
  assert.equal((await db.prepare('SELECT sha256 FROM documents WHERE id=?').bind(documentId).first()).sha256, receipt.sha256);
  const documentStore = createDocumentStore({ db, bucket });
  assert.equal((await documentStore.search(owner, '', 20)).length, 1, 'MCP query runs against actual D1 SQL');
  assert.equal((await documentStore.search('another-owner', '', 20)).length, 0);
  assert.equal((await documentStore.search(owner, '%_', 20)).length, 0, 'LIKE wildcard characters are literal search text');
  assert.equal((await post(original)).status, 200, 'original retry is idempotent');
  const foreign = await post({ action: 'session', id: original.session.id, user: 'another-owner' }); assert.equal(foreign.status, 404);
  const resultBytes = encoder.encode(JSON.stringify({ title: 'fixture', text: 'body', links: [], warnings: ['partial fixture'], status: 'partial', engine: 'contract fixture' }));
  const a = await upload('result', resultBytes); assert.equal((await post(a)).status, 200);
  const b = await upload('result', resultBytes, a.session.key); assert.equal((await post(b)).status, 200);
  const completedA = await (await post({ action: 'session', id: a.session.id, user: owner })).json();
  assert.equal((await post({ session: completedA, parts: a.parts })).status, 409, 'completed old result cannot replace a newer result');
  const c = await upload('result', resultBytes, b.session.key);
  assert.equal((await post({ ...c, failReceipt: true })).status, 500);
  assert.equal((await db.prepare('SELECT result_key FROM documents WHERE id=?').bind(documentId).first()).result_key, c.session.key, 'D1 committed despite simulated receipt failure');
  assert.equal((await post(c)).status, 200, 'same-result retry repairs a missing receipt');

  const stale = await upload('result', resultBytes, c.session.key);
  const newest = await upload('result', resultBytes, c.session.key);
  // Interleave both publishers in one fixture request, keeping test-only barrier
  // promises inside one Workers request context. Production functions/storage
  // are unchanged; the older publisher resumes only after the newer D1 commit.
  const race = await post({ action: 'race', older: stale, newer: newest });
  assert.equal(race.status, 200, await race.clone().text());
  assert.equal((await race.json()).status, 409, 'CAS rejects in-flight stale publication after a newer result commits');
  assert.equal((await db.prepare('SELECT result_key FROM documents WHERE id=?').bind(documentId).first()).result_key, newest.session.key);
  assert.ok(await bucket.head(newest.session.key), 'stale cleanup did not delete the selected result');
  assert.equal(new TextDecoder().decode(await (await bucket.get(`${documentId}/original`)).arrayBuffer()), new TextDecoder().decode(bytes));
  console.log('Workers upload contract passed: real R2 multipart, D1 CAS, DigestStream SHA-256, ownership, receipt-write recovery and in-flight stale publication.');
} finally { await mf.dispose(); }
