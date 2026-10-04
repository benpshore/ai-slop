import {storage,ownedRecord} from './server';
import type {DocumentRow} from './types';
export type UploadSession={owner:string;id:string;documentId:string;key:string;uploadId:string;target:'original'|'result'|'asset';bytes:number;createdAt:string;name:string;kind:string;sourceUrl:string|null;mime:string;baseResultKey?:string|null;completed?:DocumentRow|{saved:true}|{url:string;id:string};summary?:{title:string;status:string;engine:string;searchText:string}};
export async function sessionFor(id:string,user:string):Promise<UploadSession>{
 if(!/^[a-f0-9-]{36}$/.test(id))throw new Response('Not found',{status:404});
 const item=await storage().bucket.get(`uploads/${id}`);
 if(!item)throw new Response('Upload session not found',{status:404});
 const session=await item.json<UploadSession>();
 if(session.owner!==user)throw new Response('Not found',{status:404});
 return session;
}
export async function finishUpload(session:UploadSession,parts:R2UploadedPart[]){
 if(session.completed){
  if(session.target==='result'&&(await ownedRecord(session.documentId,session.owner)).result_key!==session.key)throw new Response('A newer extraction is already saved. Reopen the document to view it.',{status:409});
  return session.completed;
 }
 const {bucket,db}=storage();
 let object=await bucket.head(session.key);
 if(!object){try{object=await bucket.resumeMultipartUpload(session.key,session.uploadId).complete(parts);}catch(error){object=await bucket.head(session.key);if(!object)throw error;}}
 if(object.size!==session.bytes)throw new Error('The upload is incomplete. Retry without removing the original.');
 if(session.target==='asset'){
  await ownedRecord(session.documentId,session.owner);session.completed={url:`/api/documents/${session.documentId}/assets/${session.id}`,id:session.id};await bucket.put(`uploads/${session.id}`,JSON.stringify(session));return session.completed;
 }
 if(session.target==='result'){
  await ownedRecord(session.documentId,session.owner);const summary=session.summary!;
  const update=await db.prepare('UPDATE documents SET title=?,status=?,engine=?,search_text=?,result_key=? WHERE id=? AND owner=? AND result_key IS ?').bind(summary.title,summary.status,summary.engine,summary.searchText,session.key,session.documentId,session.owner,session.baseResultKey??null).run();
  if(!update.meta.changes){
   const current=await ownedRecord(session.documentId,session.owner);
   if(current.result_key!==session.key)throw new Response('A newer extraction is already saved. Reopen the document before replacing it.',{status:409});
  }else if(session.baseResultKey&&session.baseResultKey!==session.key)try{await bucket.delete(session.baseResultKey);}catch{}
  session.completed={saved:true};await bucket.put(`uploads/${session.id}`,JSON.stringify(session));return session.completed;
 }
 const original=await bucket.get(session.key);if(!original)throw new Error('The uploaded original is unavailable.');
 const digest=new (crypto as Crypto & {DigestStream:typeof DigestStream}).DigestStream('SHA-256');await original.body.pipeTo(digest);
 const sha=Array.from(new Uint8Array(await digest.digest)).map(byte=>byte.toString(16).padStart(2,'0')).join('');
 await db.prepare('INSERT OR IGNORE INTO documents (id,owner,title,kind,source_url,original_name,mime,status,engine,sha256,bytes,created_at,search_text) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)').bind(session.documentId,session.owner,session.name,session.kind,session.sourceUrl,session.name,session.mime,'uploaded','',sha,session.bytes,session.createdAt,'').run();
 session.completed={id:session.documentId,title:session.name,kind:session.kind,source_url:session.sourceUrl,original_name:session.name,status:'uploaded',engine:'',created_at:session.createdAt,sha256:sha,bytes:session.bytes};
 await bucket.put(`uploads/${session.id}`,JSON.stringify(session));return session.completed;
}
