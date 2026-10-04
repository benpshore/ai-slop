import {storage, ownedRecord} from './server';
import type {UploadSession} from './uploads';

// Keep only the owner and reserved ID after deletion. A late original commit
// must never recreate an explicitly deleted document.
export async function assertDocumentNotDeleted(id: string) {
  const row = await storage().db.prepare('SELECT id FROM document_deletions WHERE id = ?').bind(id).first();
  if (row) throw new Response('Document deleted', {status: 410});
}

export async function guardUpload(session: UploadSession) {
  await assertDocumentNotDeleted(session.documentId);
  if (session.target !== 'original' || session.completed) await ownedRecord(session.documentId, session.owner);
}

export async function discardDeletedUpload(session: UploadSession) {
  const {db, bucket} = storage();
  const deleted = await db.prepare('SELECT id FROM document_deletions WHERE id = ? AND owner = ?').bind(session.documentId, session.owner).first();
  if (deleted) {
    await bucket.delete([session.key, `uploads/${session.id}`]);
  }
}

export async function deleteDocument(id: string, user: string) {
  if (!/^[a-f0-9-]{36}$/.test(id)) throw new Response('Not found', {status: 404});
  const {db, bucket} = storage();
  const prior = await db.prepare('SELECT id FROM document_deletions WHERE id = ? AND owner = ?').bind(id, user).first();
  if (!prior) await ownedRecord(id, user);
  // D1 batch is transactional: block future original inserts and remove all
  // searchable metadata together. Retrying an interrupted cleanup is safe.
  await db.batch([
    db.prepare('INSERT OR IGNORE INTO document_deletions (id, owner) SELECT id, owner FROM documents WHERE id = ? AND owner = ?').bind(id, user),
    db.prepare('DELETE FROM documents WHERE id = ? AND owner = ?').bind(id, user),
  ]);
  try {
    // Legacy receipts live outside the document prefix. Stream their listing;
    // never remove an object until both its owner and document ID match.
    let cursor: string | undefined;
    do {
      const page = await bucket.list({prefix: 'uploads/', cursor});
      for (const object of page.objects) {
        const receipt = await bucket.get(object.key);
        if (!receipt) continue;
        let session: UploadSession;
        try { session = await receipt.json<UploadSession>(); } catch { continue; }
        if (session.owner !== user || session.documentId !== id || !session.key.startsWith(`${id}/`)) continue;
        if (!session.completed) {
          // R2 abort is idempotent for an already-completed/aborted upload.
          await bucket.resumeMultipartUpload(session.key, session.uploadId).abort();
        }
        await bucket.delete(object.key);
      }
      cursor = page.truncated ? page.cursor : undefined;
    } while (cursor);
    do {
      const page = await bucket.list({prefix: `${id}/`, cursor});
      if (page.objects.length) await bucket.delete(page.objects.map(object => object.key));
      cursor = page.truncated ? page.cursor : undefined;
    } while (cursor);
  } catch {
    throw new Response('The document is removed from your library. Storage cleanup is incomplete; retry Delete to finish.', {status: 503});
  }
}
