import {owner,ownedRecord,storage,failure} from '@/lib/server';
export async function GET(request:Request,context:{params:Promise<{id:string;asset:string}>}){try{
 const user=await owner(),{id,asset}=await context.params;await ownedRecord(id,user);
 if(!/^[a-f0-9-]{36}$/.test(asset))return new Response('Not found',{status:404});
 const object=await storage().bucket.get(`${id}/assets/${asset}`);if(!object)return new Response('Not found',{status:404});
 return new Response(object.body,{headers:{'Content-Type':object.httpMetadata?.contentType||'application/octet-stream','Cache-Control':'private, no-store','X-Content-Type-Options':'nosniff','Content-Security-Policy':"default-src 'none'; sandbox"}});
}catch(error){return failure(error);}}
