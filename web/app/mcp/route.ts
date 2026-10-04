import { createDocumentStore, createMcpHandler, mcpGet } from '@/lib/mcp';
import { owner, storage } from '@/lib/server';

// Sites supplies authenticated identity at its hosting boundary. Never accept
// user IDs, bearer tokens or alternate identity headers from tool arguments.
export const POST = createMcpHandler({
  owner: request => owner(request),
  store: () => createDocumentStore(storage()),
});
export const GET = mcpGet;
export const DELETE = mcpGet;
