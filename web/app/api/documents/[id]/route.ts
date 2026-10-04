import { owner, storage, ownedRecord, failure, jsonEnvelope, boundedBody } from '@/lib/server';
import {deleteDocument} from '@/lib/document-lifecycle';
type Context={params:Promise<{id:string}>};
export async function DELETE(request:Request,context:Context){try{
 const user=await owner(request),{id}=await context.params;
 const input=JSON.parse(new TextDecoder().decode(await boundedBody(request,1024)));
 if(input.confirmDocumentId!==id)throw new Response('Confirm the document to delete.',{status:400});
 await deleteDocument(id,user);return new Response(null,{status:204});
}catch(e){return failure(e);}}
export async function GET(request:Request,context:Context){try{const user=await owner();const {id}=await context.params;const record=await ownedRecord(id,user);const object=record.result_key?await storage().bucket.get(String(record.result_key)):null;return jsonEnvelope(record,object?.body??null);}catch(e){return failure(e);}}
export async function PATCH(request:Request,context:Context){try{
 const user=await owner(request),{id}=await context.params;const previous=await ownedRecord(id,user);
 const bytes=new Uint8Array(await request.arrayBuffer()),result=JSON.parse(new TextDecoder().decode(bytes));
 if(!result||typeof result.title!=='string'||typeof result.text!=='string'||typeof result.engine!=='string'||!['ready','partial','failed'].includes(result.status)||!Array.isArray(result.links)||!Array.isArray(result.warnings))throw new Error('Invalid extraction result.');
 if(result.links.some((link:unknown)=>!link||typeof link!=='object'||!('url' in link)||typeof link.url!=='string')||result.warnings.some((warning:unknown)=>typeof warning!=='string')||(result.html!==undefined&&typeof result.html!=='string')||(result.markdown!==undefined&&typeof result.markdown!=='string'))throw new Error('Invalid extraction content.');
 const {db,bucket}=storage(),key=`${id}/results/${crypto.randomUUID()}`;
 await bucket.put(key,JSON.stringify(result),{httpMetadata:{contentType:'application/json'}});
 try{const updated=await db.prepare('UPDATE documents SET title=?, status=?, engine=?, search_text=?, result_key=? WHERE id=? AND owner=? AND result_key IS ?').bind(result.title,result.status,result.engine,result.text.slice(0,100000),key,id,user,previous.result_key??null).run();if(!updated.meta.changes)throw new Response('A newer extraction is already saved. Reopen the document before replacing it.',{status:409});}
 catch(e){try{await bucket.delete(key);}catch{}throw e;}
 if(previous.result_key)try{await bucket.delete(String(previous.result_key));}catch{}
 return Response.json({saved:true});
}catch(e){return failure(e);}}
