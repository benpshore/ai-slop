import { owner, storage, failure } from '@/lib/server';
export async function GET(request:Request){try{const user=await owner();const q=(new URL(request.url).searchParams.get('q')||'').slice(0,200);const {results}=await storage().db.prepare('SELECT id,title,kind,source_url,original_name,status,engine,created_at,sha256,bytes FROM documents WHERE owner = ? AND (title LIKE ? OR search_text LIKE ?) ORDER BY created_at DESC LIMIT 200').bind(user,`%${q}%`,`%${q}%`).all();return Response.json({documents:results},{headers:{'Cache-Control':'private, no-store'}});}catch(e){return failure(e);}}
export async function POST(request:Request){try{
 const user=await owner(request);const bytes=new Uint8Array(await request.arrayBuffer());if(!bytes.length)throw new Error('The file is empty.');
 const query=new URL(request.url).searchParams;const name=(query.get('name')||'Document').replace(/[\r\n/\\]/g,'_').slice(0,200);
 const kind=query.get('kind')||'html';if(!['pdf','html','feed','json','image','css','text'].includes(kind))throw new Error('Unsupported document type.');
 if(kind==='pdf'&&!new TextDecoder().decode(bytes.slice(0,1024)).includes('%PDF-'))throw new Error('This file does not contain a PDF header.');
 const source=query.get('url')||null;if(source&&(!/^https?:\/\//i.test(source)||source.length>4000))throw new Error('Invalid source URL.');
 const sha=Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256',bytes))).map(x=>x.toString(16).padStart(2,'0')).join('');
 const id=crypto.randomUUID(),created=new Date().toISOString(),mime=kind==='pdf'?'application/pdf':kind==='json'?'application/json':kind==='feed'?'application/xml':kind==='css'?'text/css':kind==='text'?'text/plain':kind==='image'?(bytes[0]===0x89&&bytes[1]===0x50?'image/png':bytes[0]===0xff&&bytes[1]===0xd8?'image/jpeg':new TextDecoder().decode(bytes.slice(8,12))==='WEBP'?'image/webp':'application/octet-stream'):'text/html';
 const {db,bucket}=storage();await bucket.put(`${id}/original`,bytes,{httpMetadata:{contentType:mime}});
 try{await db.prepare('INSERT INTO documents (id,owner,title,kind,source_url,original_name,mime,status,engine,sha256,bytes,created_at,search_text) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)').bind(id,user,name,kind,source,name,mime,'uploaded','',sha,bytes.length,created,'').run();}catch(e){await bucket.delete(`${id}/original`);throw e;}
 return Response.json({id,title:name,kind,source_url:source,original_name:name,status:'uploaded',engine:'',created_at:created,sha256:sha,bytes:bytes.length},{status:201});
}catch(e){return failure(e);}}
