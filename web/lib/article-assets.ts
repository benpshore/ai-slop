import type {DocumentRow,Extracted} from './types';
export async function retainArticleImages(record:DocumentRow,result:Extracted,signal?:AbortSignal,onProgress?:(done:number,total:number)=>void):Promise<Extracted>{
 if(!result.html)return result;
 const dom=new DOMParser().parseFromString(result.html,'text/html');
 const images=Array.from(dom.querySelectorAll('img'));let completed=0,next=0;
 const warnings=[...result.warnings],assets:Record<string,unknown>[]=[];
 // Network concurrency is bounded; every article image remains eligible.
 async function consume(){while(next<images.length){signal?.throwIfAborted();const image=images[next++],source=image.getAttribute('src')||'';
  try{
   if(source.startsWith(`/api/documents/${record.id}/assets/`))continue;
   if(!/^https?:\/\//i.test(source))throw new Error('No downloadable source was provided.');
   const response=await fetch(`/api/documents/${record.id}/assets`,{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({url:source}),signal});
   if(!response.ok){let reason=await response.text();try{reason=JSON.parse(reason).error||reason;}catch{}throw new Error(reason);}
   const asset=await response.json() as {url:string;sourceUrl:string;mime:string;bytes:number};
   assets.push({...asset,id:image.getAttribute('data-image-id'),alt:image.getAttribute('alt')});image.setAttribute('src',asset.url);image.removeAttribute('srcset');image.setAttribute('loading','lazy');image.setAttribute('decoding','async');
  }catch(error){if(signal?.aborted)throw error;warnings.push(`Image could not be retained: ${source} (${error instanceof Error?error.message:String(error)})`);image.remove();}
  finally{onProgress?.(++completed,images.length);}
 }}
 await Promise.all(Array.from({length:Math.min(3,images.length)},()=>consume()));
 return {...result,html:dom.body.innerHTML,warnings,metadata:{...result.metadata,retainedImages:assets}};
}
