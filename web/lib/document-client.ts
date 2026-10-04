/** Call only after an explicit user confirmation for this document. */
export async function deleteStoredDocument(id: string, options: {confirmDocumentId: string}): Promise<void> {
  if (options.confirmDocumentId !== id) throw new Error('Confirm the document to delete.');
  const response = await fetch(`/api/documents/${encodeURIComponent(id)}`, {
    method: 'DELETE', headers: {'Content-Type': 'application/json'},
    body: JSON.stringify(options),
  });
  if (!response.ok) throw new Error(await response.text() || 'Deletion failed. Retry to finish removing this document.');
}
