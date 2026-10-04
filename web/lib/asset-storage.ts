import {storage} from './server';
// Bounded transport buffers; the total asset size is not capped by the app.
export async function storeAssetStream(key:string,body:ReadableStream<Uint8Array>,mime:string){
 const bucket=storage().bucket,upload=await bucket.createMultipartUpload(key,{httpMetadata:{contentType:mime}});
 const reader=body.getReader(),parts:R2UploadedPart[]=[];let buffers:Uint8Array[]=[],length=0,total=0;
 async function flush(){if(!length)return;const bytes=new Uint8Array(length);let offset=0;for(const part of buffers){bytes.set(part,offset);offset+=part.length;}
  parts.push(await upload.uploadPart(parts.length+1,bytes));buffers=[];length=0;
 }
 try{
  while(true){const {done,value}=await reader.read();if(done)break;buffers.push(value);length+=value.length;total+=value.length;if(length>=8*1024*1024)await flush();}
  await flush();if(!parts.length)throw new Error('The image source is empty.');await upload.complete(parts);return total;
 }catch(error){await reader.cancel(error).catch(()=>{});await upload.abort().catch(()=>{});throw error;}
 finally{reader.releaseLock();}
}
