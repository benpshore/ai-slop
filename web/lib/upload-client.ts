import type {DocumentRow, Extracted} from './types';
export type UploadOptions={kind?:string;sourceUrl?:string;signal?:AbortSignal;onProgress?:(fraction:number)=>void};
type AssetReceipt={url:string;id:string};
async function responseJson<T>(response:Response):Promise<T>{
 if(!response.ok){let message=await response.text();try{message=JSON.parse(message).error||message;}catch{}
  throw new Error(message||`Storage returned HTTP ${response.status}.`);}
 return response.json() as Promise<T>;
}
export async function detectFileKind(file:File):Promise<string>{
 const bytes=new Uint8Array(await file.slice(0,4096).arrayBuffer());
 const prefix=new TextDecoder().decode(bytes).replace(/^\uFEFF/,'').trimStart();
 const zip=bytes[0]===0x50&&bytes[1]===0x4b&&[3,5,7].includes(bytes[2]);
 if(zip&&/\.(?:docx|pptx|xlsx|odt|ods|odp|pages|numbers|key)$/i.test(file.name))return 'office';
 if(zip){const {detectOffice}=await import('./office');if(await detectOffice(file))return 'office';}
 if((bytes[0]===0x1f&&bytes[1]===0x8b)||zip||new TextDecoder().decode(bytes.subarray(257,262))==='ustar'||/\.tar$/i.test(file.name))return 'archive';
 // Actual content wins over names (including downloaded PHP and renamed HTML).
 if(/^<!doctype\s+html|^<(?:html|head|body|article|main|div|section|p|h[1-6])(?:\s|>)/i.test(prefix))return 'html';
 if(prefix.slice(0,1024).includes('%PDF-'))return 'pdf';
 if((bytes[0]===0x89&&bytes[1]===0x50&&bytes[2]===0x4e&&bytes[3]===0x47)||(bytes[0]===0xff&&bytes[1]===0xd8&&bytes[2]===0xff)||/^GIF8[79]a|^BM|^RIFF[\s\S]{4}WEBP/.test(prefix)||(bytes[0]===0x49&&bytes[1]===0x49&&bytes[2]===42)||(bytes[0]===0x4d&&bytes[1]===0x4d&&bytes[3]===42))return 'image';
 if(/<(?:\w+:)?(?:rss|feed|RDF)(?:\s|>)/i.test(prefix))return 'feed';
 if(/^<\?xml\s/i.test(prefix))return 'xml';
 if(/^[\[{]/.test(prefix))return 'json';
 if(/^<\?php(?:\s|$)/i.test(prefix))return 'text';
 if(file.type==='text/css'||/\.css$/i.test(file.name))return 'css';
 if(file.type==='text/plain'||/\.(?:txt|md|markdown)$/i.test(file.name))return 'text';
 if(/<\??[a-z!]/i.test(prefix)||/html|xhtml/.test(file.type)||/\.(?:html?|php)$/i.test(file.name))return 'html';
 if(/^audio\//.test(file.type)||/\.(?:mp3|m4a|aac|wav|flac|ogg|opus|aiff)$/i.test(file.name))return 'audio';
 if(/^video\//.test(file.type)||/\.(?:mp4|mov|mkv|webm|avi|m4v)$/i.test(file.name))return 'video';
 return 'file';
}
async function multipart(file:Blob,metadata:Record<string,unknown>,options:UploadOptions):Promise<DocumentRow|{saved:true}|AssetReceipt>{
 options.signal?.throwIfAborted();
 const started=await responseJson<{session:string;chunkSize:number}>(await fetch('/api/uploads',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({...metadata,bytes:file.size}),signal:options.signal}));
 const parts:{partNumber:number;etag:string}[]=[];let committing=false;
 try{
  for(let offset=0,partNumber=1;offset<file.size;offset+=started.chunkSize,partNumber++){
   options.signal?.throwIfAborted();
   const part=await responseJson<{partNumber:number;etag:string}>(await fetch(`/api/uploads/${started.session}?part=${partNumber}`,{method:'PUT',headers:{'Content-Type':'application/octet-stream'},body:file.slice(offset,offset+started.chunkSize),signal:options.signal}));
   parts.push(part);options.onProgress?.(Math.min(1,(offset+started.chunkSize)/file.size));
  }
  options.signal?.throwIfAborted();committing=true;
  // Once committing, wait for the authoritative saved state even if Cancel is tapped.
  const commit=()=>fetch(`/api/uploads/${started.session}`,{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({parts})});
  let committed:Response;
  try{committed=await commit();}catch{
   // A lost response does not mean a lost save. Finalization is idempotent.
   const state=await responseJson<{completed:DocumentRow|{saved:true}|AssetReceipt|null}>(await fetch(`/api/uploads/${started.session}`));
   if(state.completed)return state.completed;
   committed=await commit();
  }
  return await responseJson<DocumentRow|{saved:true}|AssetReceipt>(committed);
 }catch(error){if(!committing)await fetch(`/api/uploads/${started.session}`,{method:'DELETE',keepalive:true}).catch(()=>{});throw error;}
}
export async function uploadOriginal(file:File,options:UploadOptions={}):Promise<DocumentRow>{
 const kind=await detectFileKind(file);
 return await multipart(file,{target:'original',kind,name:file.name,sourceUrl:options.sourceUrl||null,mime:file.type},options) as DocumentRow;
}
export async function saveExtracted(record:DocumentRow,result:Extracted,options:UploadOptions={}):Promise<void>{
 const bytes=new Blob([JSON.stringify(result)],{type:'application/json'});
 await multipart(bytes,{target:'result',documentId:record.id,summary:{title:result.title,status:result.status,engine:result.engine,searchText:result.text.slice(0,100000)}},options);
}
export async function uploadAssetFile(record:DocumentRow,file:File,options:UploadOptions={}):Promise<AssetReceipt>{
 return await multipart(file,{target:'asset',documentId:record.id,mime:file.type,name:file.name},options) as AssetReceipt;
}
export async function captureSource(url:string,signal?:AbortSignal):Promise<{file:File;url:string;contentType:string;decodedSource:string}>{
 const response=await fetch('/api/capture',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({url}),signal});
 if(!response.ok){await responseJson(response);throw new Error('Capture failed.');}
 const contentType=response.headers.get('content-type')||'text/html';
 const finalUrl=response.headers.get('x-tpe-source-url')||url;
 const blob=await response.blob();const decodedSource=await decodeSource(blob,contentType,signal);
 const feed=/rss|atom/i.test(contentType)||/<(?:\w+:)?(?:rss|feed|RDF)(?:\s|>)/i.test(decodedSource.slice(0,4096));
 const extension=feed?'xml':/text\/css/i.test(contentType)?'css':/text\/plain/i.test(contentType)?'txt':'html';
 return {file:new File([blob],`${new URL(finalUrl).hostname}.${extension}`,{type:contentType}),url:finalUrl,contentType,decodedSource};
}

export async function decodeSource(blob:Blob,contentType=blob.type,signal?:AbortSignal):Promise<string>{
 const prefix=new Uint8Array(await blob.slice(0,2048).arrayBuffer());
 const ascii=new TextDecoder().decode(prefix);
 const bom=prefix[0]===0xff&&prefix[1]===0xfe?'utf-16le':prefix[0]===0xfe&&prefix[1]===0xff?'utf-16be':prefix[0]===0xef&&prefix[1]===0xbb&&prefix[2]===0xbf?'utf-8':null;
 const encoding=bom||contentType.match(/charset\s*=\s*["']?([^\s;"']+)/i)?.[1]||ascii.match(/(?:charset|encoding)\s*=\s*["']?([^\s;"'/>]+)/i)?.[1]||'utf-8';
 let decoder:TextDecoder;try{decoder=new TextDecoder(encoding);}catch{decoder=new TextDecoder();}
 const reader=blob.stream().getReader(),decoded:string[]=[];
 try{while(true){const {done,value}=await reader.read();if(done)break;signal?.throwIfAborted();decoded.push(decoder.decode(value,{stream:true}));}decoded.push(decoder.decode());}finally{await reader.cancel().catch(()=>{});reader.releaseLock();}
 return decoded.join('');
}
