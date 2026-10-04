import { env } from 'cloudflare:workers';
import { getChatGPTUser } from '@/app/chatgpt-auth';

export async function owner(request?: Request) {
  const user = await getChatGPTUser();
  if (!user) throw new Response('Sign in required', {status:401});
  if (request && !['GET','HEAD'].includes(request.method)) {
    const origin=request.headers.get('origin');
    if (origin && origin!==new URL(request.url).origin) throw new Response('Cross-origin writes are not allowed',{status:403});
  }
  return user.userId;
}
export function storage() {
  if (!env.DB || !env.BUCKET) throw new Error('Document storage is unavailable. Please retry.');
  return {db:env.DB,bucket:env.BUCKET};
}
export async function boundedBody(request: Request|Response, limit: number) {
  if (Number(request.headers.get('content-length')||0)>limit) throw new Response('Input exceeds the size limit',{status:413});
  if (!request.body) return new Uint8Array();
  const reader=request.body.getReader(); const chunks:Uint8Array[]=[]; let size=0;
  try { while(true){const {done,value}=await reader.read();if(done)break;size+=value.length;if(size>limit){await reader.cancel();throw new Response('Input exceeds the size limit',{status:413});}chunks.push(value);} }
  finally {reader.releaseLock();}
  const body=new Uint8Array(size);let offset=0;for(const chunk of chunks){body.set(chunk,offset);offset+=chunk.length;}return body;
}
export function failure(error: unknown) {
  if(error instanceof Response) return error;
  console.error(error instanceof Error ? error.message : 'Request failed');
  return Response.json({error:error instanceof Error?error.message:'Request failed. Please retry.'},{status:400});
}
export async function ownedRecord(id:string,user:string){
  if(!/^[a-f0-9-]{36}$/.test(id)) throw new Response('Not found',{status:404});
  const row=await storage().db.prepare('SELECT * FROM documents WHERE id = ? AND owner = ?').bind(id,user).first<Record<string,unknown>>();
  if(!row)throw new Response('Not found',{status:404});return row;
}
// Stream the immutable result object without materializing it in Worker memory.
export function jsonEnvelope(record:Record<string,unknown>,body:ReadableStream<Uint8Array>|null){
 const encoder=new TextEncoder(),reader=body?.getReader();let stage=0;
 const stream=new ReadableStream<Uint8Array>({
  async pull(controller){
   if(stage===0){stage=1;controller.enqueue(encoder.encode(`{"record":${JSON.stringify(record)},"result":`));return;}
   if(stage===1){
    if(reader){const next=await reader.read();if(!next.done){controller.enqueue(next.value);return;}}
    else controller.enqueue(encoder.encode('null'));
    stage=2;
   }
   controller.enqueue(encoder.encode('}'));controller.close();reader?.releaseLock();
  },
  cancel(reason){return reader?.cancel(reason);}
 });
 return new Response(stream,{headers:{'Content-Type':'application/json','Cache-Control':'private, no-store'}});
}
