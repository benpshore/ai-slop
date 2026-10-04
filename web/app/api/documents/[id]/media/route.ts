import {owner,ownedRecord,storage,failure} from '@/lib/server';
export async function GET(request:Request,context:{params:Promise<{id:string}>}){try{
 const user=await owner(),{id}=await context.params,record=await ownedRecord(id,user);
 if(!['image','audio','video'].includes(String(record.kind)))return new Response('Not media',{status:415});
 const bucket=storage().bucket,key=`${id}/original`,head=await bucket.head(key);if(!head)return new Response('Not found',{status:404});
 const range=request.headers.get('range');let offset=0,end=head.size-1;
 if(range){const match=/^bytes=(\d*)-(\d*)$/.exec(range);if(!match||(!match[1]&&!match[2]))return new Response(null,{status:416,headers:{'Content-Range':`bytes */${head.size}`}});
  if(match[1]){offset=Number(match[1]);if(match[2])end=Math.min(Number(match[2]),end);}else offset=Math.max(0,head.size-Number(match[2]));
  if(!Number.isSafeInteger(offset)||!Number.isSafeInteger(end)||offset>end||offset>=head.size)return new Response(null,{status:416,headers:{'Content-Range':`bytes */${head.size}`}});
 }
 const object=await bucket.get(key,{range:{offset,length:end-offset+1}});if(!object)return new Response('Not found',{status:404});
 const headers=new Headers({'Content-Type':String(record.mime),'Content-Length':String(end-offset+1),'Accept-Ranges':'bytes','Cache-Control':'private, no-store','X-Content-Type-Options':'nosniff','Content-Security-Policy':"default-src 'none'; sandbox"});
 if(range)headers.set('Content-Range',`bytes ${offset}-${end}/${head.size}`);
 return new Response(object.body,{status:range?206:200,headers});
}catch(error){return failure(error);}}
