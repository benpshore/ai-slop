import {owner,storage,boundedBody,ownedRecord,failure} from '@/lib/server';
import {guardUpload,discardDeletedUpload} from '@/lib/document-lifecycle';
import type {UploadSession} from '@/lib/uploads';
export async function POST(request:Request){try{
 const user=await owner(request);
 const input=JSON.parse(new TextDecoder().decode(await boundedBody(request,1024*1024)));
 if(!Number.isSafeInteger(input.bytes)||input.bytes<=0)throw new Error('The file is empty or its length is invalid.');
 if(!['original','result','asset'].includes(input.target))throw new Error('Invalid upload target.');
 if(input.target==='original'&&!['pdf','html','feed','json','image','css','text','archive','office','audio','video','xml','file'].includes(input.kind))throw new Error('Unsupported document type.');
 let baseResultKey:string|null=null;
 if(input.target==='asset')await ownedRecord(input.documentId,user);
 if(input.target==='result'){
  const previous=await ownedRecord(input.documentId,user);baseResultKey=typeof previous.result_key==='string'?previous.result_key:null;const s=input.summary;
  if(!s||typeof s.title!=='string'||typeof s.engine!=='string'||typeof s.searchText!=='string'||!['ready','partial','failed'].includes(s.status))throw new Error('Invalid extraction summary.');
 }
 const id=crypto.randomUUID(),documentId=input.target==='original'?crypto.randomUUID():input.documentId;
 const key=input.target==='original'?`${documentId}/original`:input.target==='asset'?`${documentId}/assets/${id}`:`${documentId}/results/${id}`;
 const name=String(input.name||'Document').replace(/[\r\n/\\]/g,'_');
 const sourceUrl=input.sourceUrl||null;if(sourceUrl&&!/^https?:\/\//i.test(sourceUrl))throw new Error('Invalid source URL.');
 const mime=input.target==='result'?'application/json':input.target==='asset'?input.mime:input.kind==='pdf'?'application/pdf':input.kind==='html'&&/^text\/html(?:;.*)?$/i.test(input.mime)?input.mime:input.kind==='html'?'text/html':input.kind==='feed'&&/^(?:text|application)\/(?:xml|rss\+xml|atom\+xml)(?:;.*)?$/i.test(input.mime)?input.mime:input.kind==='feed'?'application/xml':input.kind==='json'?'application/json':input.kind==='css'?'text/css':input.kind==='image'&&/^image\/(?:png|jpeg|webp|gif|bmp|tiff|avif)$/.test(input.mime)?input.mime:['audio','video'].includes(input.kind)&&/^(?:audio|video)\/[a-z0-9.+-]+$/i.test(input.mime)?input.mime:'application/octet-stream';
 if(input.target==='asset'&&!/^image\/(?:png|jpeg|webp|gif|bmp|tiff|avif)$/.test(input.mime))throw new Error('Unsupported embedded image type.');
 const upload=await storage().bucket.createMultipartUpload(key,{httpMetadata:{contentType:mime}});
 const session:UploadSession={id,owner:user,documentId,key,uploadId:upload.uploadId,target:input.target,bytes:input.bytes,createdAt:new Date().toISOString(),name,kind:input.kind||'json',sourceUrl,mime,...(input.target==='result'?{summary:input.summary,baseResultKey}:{})};
 try{await storage().bucket.put(`uploads/${id}`,JSON.stringify(session));await guardUpload(session);}catch(error){await upload.abort();await discardDeletedUpload(session);throw error;}
 // R2 allows 10,000 parts. This is transport sizing, not a file acceptance cap.
 const chunkSize=Math.max(8*1024*1024,Math.ceil(input.bytes/10000));
 return Response.json({session:id,chunkSize},{status:201});
}catch(error){return failure(error);}}
