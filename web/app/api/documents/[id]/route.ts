import { owner, storage, boundedBody, ownedRecord, failure } from '@/lib/server';
type Context={params:Promise<{id:string}>};
export async function GET(request:Request,context:Context){try{const user=await owner();const {id}=await context.params;const record=await ownedRecord(id,user);const object=record.result_key?await storage().bucket.get(String(record.result_key)):null;return Response.json({record,result:object?await object.json():null},{headers:{'Cache-Control':'private, no-store'}});}catch(e){return failure(e);}}
export async function PATCH(request:Request,context:Context){try{
 const user=await owner(request),{id}=await context.params;const previous=await ownedRecord(id,user);
 const bytes=await boundedBody(request,4*1024*1024),result=JSON.parse(new TextDecoder().decode(bytes));
 if(!result||typeof result.title!=='string'||typeof result.text!=='string'||typeof result.engine!=='string'||!['ready','partial','failed'].includes(result.status)||!Array.isArray(result.links)||!Array.isArray(result.warnings))throw new Error('Invalid extraction result.');
 if(result.links.length>5000||result.warnings.length>1000||result.title.length>500||result.engine.length>200)throw new Error('Extraction result exceeds its limits.');
 if(result.links.some((link:unknown)=>!link||typeof link!=='object'||!('url' in link)||typeof link.url!=='string')||result.warnings.some((warning:unknown)=>typeof warning!=='string')||(result.html!==undefined&&typeof result.html!=='string')||(result.markdown!==undefined&&typeof result.markdown!=='string'))throw new Error('Invalid extraction content.');
 const {db,bucket}=storage(),key=`${id}/results/${crypto.randomUUID()}`;
 await bucket.put(key,JSON.stringify(result),{httpMetadata:{contentType:'application/json'}});
 try{await db.prepare('UPDATE documents SET title=?, status=?, engine=?, search_text=?, result_key=? WHERE id=? AND owner=?').bind(result.title,result.status,result.engine,result.text.slice(0,100000),key,id,user).run();}
 catch(e){try{await bucket.delete(key);}catch{}throw e;}
 if(previous.result_key)try{await bucket.delete(String(previous.result_key));}catch{}
 return Response.json({saved:true});
}catch(e){return failure(e);}}
