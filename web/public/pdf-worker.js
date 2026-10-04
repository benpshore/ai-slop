import init,{WasmPdfDocument} from '/vendor/pdf-oxide/pdf_oxide.js';
const plain=value=>value instanceof Map?Object.fromEntries([...value].map(([k,v])=>[k,plain(v)])):Array.isArray(value)?value.map(plain):value;
self.onmessage=async event=>{
 let doc;
 try{
  await init({module_or_path:'/vendor/pdf-oxide/pdf_oxide_bg.wasm'});
  doc=new WasmPdfDocument(new Uint8Array(event.data.bytes));
  const count=doc.pageCount();if(!count)throw new Error('The PDF contains no readable pages.');
  const pages=[],links=[],warnings=['Browser extraction uses PDF Oxide 0.3.77 (Rust/WASM). Mapping completeness is unverified; OCR and native fallback engines are not run here.'];
  for(let index=0;index<count;index++){
   let text='';try{text=doc.extractText(index,null);}catch(error){warnings.push(`Page ${index+1}: ${String(error)}`);}
   let annotations=[];try{annotations=plain(doc.getAnnotations(index));}catch(error){warnings.push(`Page ${index+1} links: ${String(error)}`);}
   for(const annotation of annotations){if(typeof annotation.action_uri==='string')links.push({url:annotation.action_uri,label:annotation.contents||'',page:index+1,rect:annotation.rect,kind:'embedded PDF link'});}
   if(!text.trim())warnings.push(`Page ${index+1}: no text extracted; this may be a blank page, an image, or a decoding failure.`);
   pages.push({page:index+1,text,mediaBox:Array.from(doc.pageMediaBox(index)),rotation:doc.pageRotation(index),annotations});
   self.postMessage({progress:index+1,total:count});
  }
  const text=pages.map(p=>p.text).join('\n\n');
  self.postMessage({result:{title:event.data.name,text,markdown:pages.map(p=>`## Page ${p.page}\n\n${p.text}`).join('\n\n'),pages,links,warnings,engine:'pdf-oxide-wasm 0.3.77',status:'partial',metadata:{pageCount:count,capturedAt:new Date().toISOString(),execution:'dedicated browser worker'}}});
 }catch(error){self.postMessage({error:String(error)});}finally{doc?.free();self.close();}
};
