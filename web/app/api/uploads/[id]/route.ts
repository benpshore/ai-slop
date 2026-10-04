import {owner,storage,boundedBody,failure} from '@/lib/server';
import {sessionFor,finishUpload} from '@/lib/uploads';
type Context={params:Promise<{id:string}>};
export async function GET(request:Request,context:Context){try{
 const user=await owner(),{id}=await context.params,session=await sessionFor(id,user);
 return Response.json({completed:session.completed??null},{headers:{'Cache-Control':'private, no-store'}});
}catch(error){return failure(error);}}
export async function PUT(request:Request,context:Context){try{
 const user=await owner(request),{id}=await context.params,session=await sessionFor(id,user);
 if(session.completed)throw new Error('This upload is already saved.');
 const part=Number(new URL(request.url).searchParams.get('part'));
 if(!Number.isInteger(part)||part<1||part>10000||!request.body)throw new Error('Invalid upload part.');
 const stored=await storage().bucket.resumeMultipartUpload(session.key,session.uploadId).uploadPart(part,request.body);
 return Response.json(stored);
}catch(error){return failure(error);}}
export async function POST(request:Request,context:Context){try{
 const user=await owner(request),{id}=await context.params,session=await sessionFor(id,user);
 const {parts}=JSON.parse(new TextDecoder().decode(await boundedBody(request,2*1024*1024)));
 if(!Array.isArray(parts)||!parts.length||parts.length>10000||parts.some((p:unknown,i:number)=>!p||typeof p!=='object'||!('partNumber'in p)||p.partNumber!==i+1||!('etag'in p)||typeof p.etag!=='string'))throw new Error('Invalid upload receipt.');
 return Response.json(await finishUpload(session,parts));
}catch(error){return failure(error);}}
export async function DELETE(request:Request,context:Context){try{
 const user=await owner(request),{id}=await context.params,session=await sessionFor(id,user);
 if(session.completed)return Response.json({saved:true});
 await storage().bucket.resumeMultipartUpload(session.key,session.uploadId).abort();
 await storage().bucket.delete(`uploads/${id}`);return new Response(null,{status:204});
}catch(error){return failure(error);}}
