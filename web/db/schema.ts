import { index, integer, sqliteTable, text } from 'drizzle-orm/sqlite-core';
export const documents=sqliteTable('documents',{
 id:text('id').primaryKey(),owner:text('owner').notNull(),title:text('title').notNull(),kind:text('kind').notNull(),
 sourceUrl:text('source_url'),originalName:text('original_name').notNull(),mime:text('mime').notNull(),
 status:text('status').notNull(),engine:text('engine').notNull().default(''),sha256:text('sha256').notNull(),
 bytes:integer('bytes').notNull(),createdAt:text('created_at').notNull(),searchText:text('search_text').notNull().default(''),resultKey:text('result_key'),
},t=>[index('documents_owner_created').on(t.owner,t.createdAt)]);
export const documentDeletions=sqliteTable('document_deletions',{
 id:text('id').primaryKey(),owner:text('owner').notNull(),
});
