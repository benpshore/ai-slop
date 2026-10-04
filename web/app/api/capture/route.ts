import { owner, boundedBody, failure } from '@/lib/server';
import {fetchPublicSource} from '@/lib/source-fetch';
export async function POST(request:Request){try{
 await owner(request);const payload=JSON.parse(new TextDecoder().decode(await boundedBody(request,8192)));if(typeof payload.url!=='string')throw new Error('A URL is required.');
 const {response,url}=await fetchPublicSource(payload.url,'text/html,application/xhtml+xml,application/rss+xml,application/atom+xml,application/xml,text/xml,text/css,text/plain',request.signal);
 const type=response.headers.get('content-type')||'';
 if(!/html|xml|rss|atom|text\/(?:plain|css)/i.test(type)){await response.body?.cancel();throw new Error('This URL is not an HTML page, stylesheet, text source, or feed. Add the original file instead.');}
 return new Response(response.body,{headers:{'Content-Type':type,'X-TPE-Source-URL':url,'Cache-Control':'private, no-store','X-Content-Type-Options':'nosniff'}});
}catch(error){return failure(error);}}
