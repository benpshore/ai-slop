import assert from 'node:assert/strict';
import {createRequire} from 'node:module';
const require=createRequire(import.meta.url);
const {build}=require(require.resolve('esbuild',{paths:[require.resolve('vite')]}));
const built=await build({entryPoints:['lib/upload-client.ts'],bundle:true,write:false,format:'esm',platform:'node'});
const {detectFileKind,uploadOriginal,saveExtracted}=await import(`data:text/javascript;base64,${Buffer.from(built.outputFiles[0].text).toString('base64')}`);
for(const [content,name,mime,kind] of [
 ['<!DOCTYPE html><html><body>Readable article</body></html>','renamed.pdf','application/pdf','html'],
 ['%PDF-1.7\n','renamed.html','text/html','pdf'],
 ['<?php echo "text";','source.php','','text'],
 ['<html>PHP-generated article</html>','index.php','','html'],
 ['body { color: red; }','style.css','','css'],
 ['<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"/>','feed.xml','','feed']
])assert.equal(await detectFileKind(new File([content],name,{type:mime})),kind);
const record={id:'record',title:'Large page',kind:'html',source_url:null,original_name:'large.html',status:'uploaded',engine:'',created_at:new Date().toISOString(),sha256:'abc',bytes:9*1024*1024};
const calls=[];const realFetch=globalThis.fetch;
try{
 globalThis.fetch=async(url,options={})=>{
  calls.push({url,method:options.method,bytes:options.body?.size});
  if(url==='/api/uploads')return Response.json({session:'session',chunkSize:8*1024*1024});
  if(options.method==='PUT')return Response.json({partNumber:Number(new URL(url,'https://test').searchParams.get('part')),etag:'verified'});
  if(options.method==='POST')return Response.json(record);
  throw new Error('Unexpected request');
 };
 const large=new File(['<!DOCTYPE html>',new Uint8Array(9*1024*1024)],'large.html',{type:'text/html'});
 let progress=0;assert.equal((await uploadOriginal(large,{onProgress:value=>progress=value})).id,'record');
 assert.equal(progress,1);assert.deepEqual(calls.filter(c=>c.method==='PUT').map(c=>c.bytes),[8*1024*1024,1024*1024+15]);
 assert.equal(calls.filter(c=>c.method==='POST').length,2);
 calls.length=0;const abort=new AbortController();
 globalThis.fetch=async(url,options={})=>{
  calls.push({url,method:options.method});
  if(url==='/api/uploads')return Response.json({session:'cancelled',chunkSize:8*1024*1024});
  if(options.method==='PUT'){abort.abort();return Response.json({partNumber:1,etag:'verified'});}
  if(options.method==='DELETE')return new Response(null,{status:204});
  throw new Error('An aborted transfer attempted to commit');
 };
 await assert.rejects(uploadOriginal(large,{signal:abort.signal}),{name:'AbortError'});
 assert.equal(calls.at(-1).method,'DELETE');
 calls.length=0;let commits=0;
 globalThis.fetch=async(url,options={})=>{
  calls.push({url,method:options.method});
  if(url==='/api/uploads')return Response.json({session:'recovery',chunkSize:8*1024*1024});
  if(options.method==='PUT')return Response.json({partNumber:1,etag:'verified'});
  if(options.method==='POST'){commits++;throw new TypeError('Response connection lost after commit');}
  return Response.json({completed:{saved:true}});
 };
 await saveExtracted(record,{title:'Result',text:'saved',links:[],warnings:[],engine:'HTML extractor',status:'partial'});
 assert.equal(commits,1);assert.equal(calls.at(-1).method,undefined);
 console.log('Import routing, >8 MiB multipart transfer, cancellation, and lost-save-response recovery passed.');
}finally{globalThis.fetch=realFetch;}
