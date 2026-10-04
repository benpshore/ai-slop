import {owner,ownedRecord,boundedBody,failure} from '@/lib/server';
import {fetchPublicSource} from '@/lib/source-fetch';
import {storeAssetStream} from '@/lib/asset-storage';
export async function POST(request:Request,context:{params:Promise<{id:string}>}){try{
 const user=await owner(request),{id}=await context.params;await ownedRecord(id,user);
 const input=JSON.parse(new TextDecoder().decode(await boundedBody(request,16384)));
 if(typeof input.url!=='string')throw new Error('An image URL is required.');
 const {response,url}=await fetchPublicSource(input.url,'image/avif,image/webp,image/png,image/jpeg,image/gif,image/*',request.signal);
 const mime=(response.headers.get('content-type')||'').split(';')[0].trim().toLowerCase();
 if(!['image/png','image/jpeg','image/webp','image/avif','image/gif','image/bmp','image/tiff'].includes(mime)||!response.body){await response.body?.cancel();throw new Error('This image format cannot be displayed safely.');}
 const asset=crypto.randomUUID(),bytes=await storeAssetStream(`${id}/assets/${asset}`,response.body,mime);
 return Response.json({url:`/api/documents/${id}/assets/${asset}`,sourceUrl:url,mime,bytes},{status:201});
}catch(error){return failure(error);}}
