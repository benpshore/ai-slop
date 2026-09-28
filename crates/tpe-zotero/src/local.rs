//! Read-only access to a local Zotero 7 data directory.
//!
//! Zotero keeps `zotero.sqlite` open with an exclusive lock while it runs, so
//! the database is opened as a URI with `immutable=1` and
//! `SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_URI`: `SQLite` then takes no locks and
//! never writes. The price is that changes Zotero has not yet checkpointed
//! into the main file may be missing; re-open to see newer data.
//!
//! The queried subset (`items`, `itemTypes`, `fields`, `itemData`,
//! `itemDataValues`, `creators`, `creatorTypes`, `itemCreators`,
//! `itemAttachments`, `collections`, `collectionItems`, `deletedItems`) is
//! modelled on the well-known Zotero 7 table layout; it is not copied from
//! Zotero's schema file. Stored files live at `storage/<attachment key>/<name>`
//! for attachment paths of the form `storage:<name>`.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use tpe_common::PaperRecord;

use crate::error::ZError;
use crate::item::{ZCreator, record_from_fields};

/// File name of the Zotero database inside the data directory.
pub const DB_FILE: &str = "zotero.sqlite";
/// Environment variable that overrides the data directory search.
pub const DIR_ENV: &str = "TPE_ZOTERO_DIR";

/// `itemAttachments.linkMode`: file stored in `storage/`.
pub const LINK_MODE_IMPORTED_FILE: i64 = 0;
/// `itemAttachments.linkMode`: web snapshot stored in `storage/`.
pub const LINK_MODE_IMPORTED_URL: i64 = 1;
/// `itemAttachments.linkMode`: file linked from elsewhere on disk.
pub const LINK_MODE_LINKED_FILE: i64 = 2;
/// `itemAttachments.linkMode`: link to a URL, no file.
pub const LINK_MODE_LINKED_URL: i64 = 3;

/// The Zotero data directory: `$TPE_ZOTERO_DIR` when set, else `~/Zotero`
/// (the default on macOS and Linux), provided it contains `zotero.sqlite`.
pub fn find_zotero_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(DIR_ENV) {
        let dir = PathBuf::from(dir);
        return dir.join(DB_FILE).is_file().then_some(dir);
    }
    let home = std::env::var_os("HOME")?;
    find_zotero_dir_in(Path::new(&home))
}

/// `<home>/Zotero` when it contains `zotero.sqlite`.
pub fn find_zotero_dir_in(home: &Path) -> Option<PathBuf> {
    let dir = home.join("Zotero");
    dir.join(DB_FILE).is_file().then_some(dir)
}

/// `SQLite` URI that opens `path` read-only without locking
/// (`file:...?immutable=1`); `%`, `?` and `#` in the path are escaped.
pub fn immutable_uri(path: &Path) -> Result<String, ZError> {
    let text = path
        .to_str()
        .ok_or_else(|| ZError::NotFound(format!("non UTF-8 path: {}", path.display())))?;
    let mut encoded = String::with_capacity(text.len() + 16);
    for ch in text.chars() {
        match ch {
            '%' => encoded.push_str("%25"),
            '?' => encoded.push_str("%3F"),
            '#' => encoded.push_str("%23"),
            _ => encoded.push(ch),
        }
    }
    let scheme = if encoded.starts_with('/') {
        "file://"
    } else {
        "file:"
    };
    Ok(format!("{scheme}{encoded}?immutable=1"))
}

/// Where an attachment's file is on disk, when that can be known:
/// `storage:<name>` under `storage/<key>/` for stored files, an absolute
/// path for linked files. Relative linked paths (`attachments:...`, which
/// depend on a Zotero preference) and URL links give `None`.
pub fn resolve_attachment_path(
    data_dir: &Path,
    attachment_key: &str,
    link_mode: Option<i64>,
    stored_path: Option<&str>,
) -> Option<PathBuf> {
    let stored = stored_path?;
    if let Some(name) = stored.strip_prefix("storage:") {
        let mut components = Path::new(name).components();
        let single_normal =
            matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
        let key_ok =
            !attachment_key.is_empty() && attachment_key.bytes().all(|b| b.is_ascii_alphanumeric());
        return (single_normal && key_ok)
            .then(|| data_dir.join("storage").join(attachment_key).join(name));
    }
    if link_mode == Some(LINK_MODE_LINKED_FILE) && Path::new(stored).is_absolute() {
        return Some(PathBuf::from(stored));
    }
    None
}

/// One attachment of a local item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalAttachment {
    /// Attachment item key (also the `storage/` folder name).
    pub key: String,
    /// `itemAttachments.linkMode` (see the `LINK_MODE_*` constants).
    pub link_mode: Option<i64>,
    /// MIME type, e.g. `application/pdf`.
    pub content_type: Option<String>,
    /// The raw `itemAttachments.path` value.
    pub stored_path: Option<String>,
    /// Resolved file path (see [`resolve_attachment_path`]); not checked for existence.
    pub path: Option<PathBuf>,
}

impl LocalAttachment {
    /// True for attachments with content type `application/pdf`.
    pub fn is_pdf(&self) -> bool {
        self.content_type.as_deref() == Some("application/pdf")
    }
}

/// A regular (non-note, non-attachment) item from the local database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalItem {
    /// `items.itemID`.
    pub item_id: i64,
    /// Item key.
    pub key: String,
    /// `items.libraryID` (the personal library is usually 1).
    pub library_id: i64,
    /// Item type name, e.g. `journalArticle`.
    pub item_type: String,
    /// All fields by Zotero field name.
    pub fields: BTreeMap<String, String>,
    /// The mapped record (`source` = `zotero`, `source_id` = key).
    pub record: PaperRecord,
    /// Child attachments that are not in the trash.
    pub attachments: Vec<LocalAttachment>,
    /// Keys of the collections that contain the item.
    pub collections: Vec<String>,
}

impl LocalItem {
    /// Resolved paths of PDF attachments.
    pub fn pdf_paths(&self) -> Vec<&Path> {
        self.attachments
            .iter()
            .filter(|a| a.is_pdf())
            .filter_map(|a| a.path.as_deref())
            .collect()
    }
}

/// A collection from the local database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalCollection {
    /// `collections.collectionID`.
    pub collection_id: i64,
    /// Collection key.
    pub key: String,
    /// Display name.
    pub name: String,
    /// Parent `collectionID`.
    pub parent_id: Option<i64>,
    /// `collections.libraryID`.
    pub library_id: i64,
}

const ITEMS_SQL: &str = "SELECT i.itemID, i.key, i.libraryID, t.typeName \
     FROM items i JOIN itemTypes t ON t.itemTypeID = i.itemTypeID \
     WHERE t.typeName NOT IN ('attachment', 'note', 'annotation') \
       AND i.itemID NOT IN (SELECT itemID FROM deletedItems) \
     ORDER BY i.itemID";

const FIELDS_SQL: &str = "SELECT d.itemID, f.fieldName, v.value \
     FROM itemData d \
     JOIN fields f ON f.fieldID = d.fieldID \
     JOIN itemDataValues v ON v.valueID = d.valueID";

const CREATORS_SQL: &str = "SELECT ic.itemID, c.firstName, c.lastName, c.fieldMode, ct.creatorType \
     FROM itemCreators ic \
     JOIN creators c ON c.creatorID = ic.creatorID \
     LEFT JOIN creatorTypes ct ON ct.creatorTypeID = ic.creatorTypeID \
     ORDER BY ic.itemID, ic.orderIndex";

const ATTACHMENTS_SQL: &str = "SELECT a.parentItemID, i.key, a.linkMode, a.contentType, a.path \
     FROM itemAttachments a JOIN items i ON i.itemID = a.itemID \
     WHERE a.parentItemID IS NOT NULL \
       AND a.itemID NOT IN (SELECT itemID FROM deletedItems) \
     ORDER BY a.itemID";

const MEMBERSHIP_SQL: &str = "SELECT ci.itemID, c.key \
     FROM collectionItems ci JOIN collections c ON c.collectionID = ci.collectionID \
     ORDER BY ci.itemID, c.key";

const COLLECTIONS_SQL: &str = "SELECT collectionID, key, collectionName, parentCollectionID, libraryID \
     FROM collections ORDER BY collectionID";

/// A read-only view of a Zotero data directory.
#[derive(Debug)]
pub struct LocalLibrary {
    conn: Connection,
    data_dir: PathBuf,
}

impl LocalLibrary {
    /// Open `<data_dir>/zotero.sqlite` read-only and immutable.
    pub fn open(data_dir: &Path) -> Result<Self, ZError> {
        let db = data_dir.join(DB_FILE);
        if !db.is_file() {
            return Err(ZError::NotFound(db.display().to_string()));
        }
        let uri = immutable_uri(&db)?;
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(uri.as_str(), flags)?;
        Ok(Self {
            conn,
            data_dir: data_dir.to_path_buf(),
        })
    }

    /// Open the directory found by [`find_zotero_dir`].
    pub fn open_default() -> Result<Self, ZError> {
        let dir = find_zotero_dir()
            .ok_or_else(|| ZError::NotFound("no Zotero data directory".to_string()))?;
        Self::open(&dir)
    }

    /// The data directory this library reads.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// All regular items not in the trash, with fields, mapped record,
    /// attachments and collection membership.
    pub fn items(&self) -> Result<Vec<LocalItem>, ZError> {
        let mut fields = self.load_fields()?;
        let mut creators = self.load_creators()?;
        let mut attachments = self.load_attachments()?;
        let mut membership = self.load_membership()?;
        let mut stmt = self.conn.prepare(ITEMS_SQL)?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (item_id, key, library_id, item_type) = row?;
            let item_fields = fields.remove(&item_id).unwrap_or_default();
            let item_creators = creators.remove(&item_id).unwrap_or_default();
            let record = record_from_fields(&item_type, &item_fields, &item_creators, Some(&key));
            out.push(LocalItem {
                item_id,
                key,
                library_id,
                item_type,
                fields: item_fields,
                record,
                attachments: attachments.remove(&item_id).unwrap_or_default(),
                collections: membership.remove(&item_id).unwrap_or_default(),
            });
        }
        Ok(out)
    }

    /// All collections.
    pub fn collections(&self) -> Result<Vec<LocalCollection>, ZError> {
        let mut stmt = self.conn.prepare(COLLECTIONS_SQL)?;
        let rows = stmt.query_map([], |row| {
            Ok(LocalCollection {
                collection_id: row.get(0)?,
                key: row.get(1)?,
                name: row.get(2)?,
                parent_id: row.get(3)?,
                library_id: row.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    fn load_fields(&self) -> Result<BTreeMap<i64, BTreeMap<String, String>>, ZError> {
        let mut stmt = self.conn.prepare(FIELDS_SQL)?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                value_text(row.get_ref(2)?),
            ))
        })?;
        let mut out: BTreeMap<i64, BTreeMap<String, String>> = BTreeMap::new();
        for row in rows {
            let (item_id, name, value) = row?;
            if let Some(value) = value {
                out.entry(item_id).or_default().insert(name, value);
            }
        }
        Ok(out)
    }

    fn load_creators(&self) -> Result<BTreeMap<i64, Vec<ZCreator>>, ZError> {
        let mut stmt = self.conn.prepare(CREATORS_SQL)?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        let mut out: BTreeMap<i64, Vec<ZCreator>> = BTreeMap::new();
        for row in rows {
            let (item_id, first, last, field_mode, creator_type) = row?;
            // fieldMode 1 = single-field name, stored in `lastName`.
            let creator = if field_mode == Some(1) {
                ZCreator {
                    creator_type: creator_type.unwrap_or_default(),
                    first_name: None,
                    last_name: None,
                    name: last,
                }
            } else {
                ZCreator {
                    creator_type: creator_type.unwrap_or_default(),
                    first_name: first,
                    last_name: last,
                    name: None,
                }
            };
            out.entry(item_id).or_default().push(creator);
        }
        Ok(out)
    }

    fn load_attachments(&self) -> Result<BTreeMap<i64, Vec<LocalAttachment>>, ZError> {
        let mut stmt = self.conn.prepare(ATTACHMENTS_SQL)?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        let mut out: BTreeMap<i64, Vec<LocalAttachment>> = BTreeMap::new();
        for row in rows {
            let (parent_id, key, link_mode, content_type, stored_path) = row?;
            let path =
                resolve_attachment_path(&self.data_dir, &key, link_mode, stored_path.as_deref());
            out.entry(parent_id).or_default().push(LocalAttachment {
                key,
                link_mode,
                content_type,
                stored_path,
                path,
            });
        }
        Ok(out)
    }

    fn load_membership(&self) -> Result<BTreeMap<i64, Vec<String>>, ZError> {
        let mut stmt = self.conn.prepare(MEMBERSHIP_SQL)?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out: BTreeMap<i64, Vec<String>> = BTreeMap::new();
        for row in rows {
            let (item_id, key) = row?;
            out.entry(item_id).or_default().push(key);
        }
        Ok(out)
    }
}

/// Text form of an `itemDataValues.value` cell (the column has no declared
/// type, so numbers may be stored as integers or reals).
fn value_text(value: ValueRef<'_>) -> Option<String> {
    match value {
        ValueRef::Text(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Integer(n) => Some(n.to_string()),
        ValueRef::Real(x) => Some(x.to_string()),
        ValueRef::Null | ValueRef::Blob(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Table subset modelled on the Zotero 7 schema (column names and keys as
    /// Zotero uses them; constraints trimmed to what the reader relies on).
    const FAKE_SCHEMA: &str = "
        CREATE TABLE itemTypes (itemTypeID INTEGER PRIMARY KEY, typeName TEXT,
            templateItemTypeID INT, display INT DEFAULT 1);
        CREATE TABLE fields (fieldID INTEGER PRIMARY KEY, fieldName TEXT, fieldFormatID INT);
        CREATE TABLE items (itemID INTEGER PRIMARY KEY, itemTypeID INT NOT NULL,
            dateAdded TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            dateModified TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            clientDateModified TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            libraryID INT NOT NULL, key TEXT NOT NULL, version INT NOT NULL DEFAULT 0,
            synced INT NOT NULL DEFAULT 0, UNIQUE (libraryID, key));
        CREATE TABLE itemDataValues (valueID INTEGER PRIMARY KEY, value UNIQUE);
        CREATE TABLE itemData (itemID INT, fieldID INT, valueID,
            PRIMARY KEY (itemID, fieldID));
        CREATE TABLE creators (creatorID INTEGER PRIMARY KEY, firstName TEXT, lastName TEXT,
            fieldMode INT, UNIQUE (lastName, firstName, fieldMode));
        CREATE TABLE creatorTypes (creatorTypeID INTEGER PRIMARY KEY, creatorType TEXT);
        CREATE TABLE itemCreators (itemID INT NOT NULL, creatorID INT NOT NULL,
            creatorTypeID INT NOT NULL DEFAULT 1, orderIndex INT NOT NULL DEFAULT 0,
            PRIMARY KEY (itemID, creatorID, creatorTypeID, orderIndex));
        CREATE TABLE itemAttachments (itemID INTEGER PRIMARY KEY, parentItemID INT,
            linkMode INT, contentType TEXT, charsetID INT, path TEXT,
            syncState INT DEFAULT 0, storageModTime INT, storageHash TEXT);
        CREATE TABLE collections (collectionID INTEGER PRIMARY KEY, collectionName TEXT NOT NULL,
            clientDateModified TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            parentCollectionID INT DEFAULT NULL, libraryID INT NOT NULL, key TEXT NOT NULL,
            version INT NOT NULL DEFAULT 0, synced INT NOT NULL DEFAULT 0,
            UNIQUE (libraryID, key));
        CREATE TABLE collectionItems (collectionID INT NOT NULL, itemID INT NOT NULL,
            orderIndex INT NOT NULL DEFAULT 0, PRIMARY KEY (collectionID, itemID));
        CREATE TABLE deletedItems (itemID INTEGER PRIMARY KEY,
            dateDeleted DEFAULT CURRENT_TIMESTAMP NOT NULL);
    ";

    const FAKE_DATA: &str = "
        INSERT INTO itemTypes (itemTypeID, typeName) VALUES
            (1, 'journalArticle'), (2, 'attachment'), (3, 'preprint'), (4, 'note');
        INSERT INTO fields (fieldID, fieldName) VALUES
            (1, 'title'), (2, 'date'), (3, 'DOI'), (4, 'publicationTitle'), (5, 'extra'),
            (6, 'abstractNote'), (7, 'url'), (8, 'repository'), (9, 'volume');
        INSERT INTO items (itemID, itemTypeID, libraryID, key) VALUES
            (1, 1, 1, 'ABCD2345'), (2, 2, 1, 'PDFK2345'), (3, 3, 1, 'PREP2345'),
            (4, 1, 1, 'DELE2345'), (5, 2, 1, 'LINK2345'), (6, 4, 1, 'NOTE2345'),
            (7, 2, 1, 'TRSH2345');
        INSERT INTO itemDataValues (valueID, value) VALUES
            (1, 'Local Title'), (2, '2019-03-00 March 2019'), (3, '10.1000/LOCAL.1'),
            (4, 'Journal of Local Tests'), (5, 'PMID: 555'), (6, 'A preprint'),
            (7, '2024-01-15 2024-01-15'), (8, 'arXiv'), (9, 'arXiv: 2401.01234'),
            (10, 12), (11, 'Deleted item');
        INSERT INTO itemData (itemID, fieldID, valueID) VALUES
            (1, 1, 1), (1, 2, 2), (1, 3, 3), (1, 4, 4), (1, 5, 5), (1, 9, 10),
            (3, 1, 6), (3, 2, 7), (3, 8, 8), (3, 5, 9),
            (4, 1, 11);
        INSERT INTO creatorTypes (creatorTypeID, creatorType) VALUES (1, 'author'), (2, 'editor');
        INSERT INTO creators (creatorID, firstName, lastName, fieldMode) VALUES
            (1, 'Ada', 'Lovelace', 0), (2, '', 'Local Lab', 1), (3, 'Ed', 'Itor', 0);
        INSERT INTO itemCreators (itemID, creatorID, creatorTypeID, orderIndex) VALUES
            (1, 2, 1, 1), (1, 1, 1, 0), (1, 3, 2, 2), (3, 1, 1, 0);
        INSERT INTO itemAttachments (itemID, parentItemID, linkMode, contentType, path) VALUES
            (2, 1, 0, 'application/pdf', 'storage:paper.pdf'),
            (5, 3, 2, 'application/pdf', '/elsewhere/preprint.pdf'),
            (7, 1, 0, 'application/pdf', 'storage:trashed.pdf');
        INSERT INTO collections (collectionID, collectionName, parentCollectionID, libraryID, key)
            VALUES (1, 'Reading', NULL, 1, 'COLL2345'), (2, 'Sub', 1, 1, 'SUBC2345');
        INSERT INTO collectionItems (collectionID, itemID) VALUES (2, 1), (1, 1);
        INSERT INTO deletedItems (itemID) VALUES (4), (7);
    ";

    fn fake_library() -> (tempfile::TempDir, LocalLibrary) {
        let dir = tempfile::tempdir().unwrap();
        {
            let conn = Connection::open(dir.path().join(DB_FILE)).unwrap();
            conn.execute_batch(FAKE_SCHEMA).unwrap();
            conn.execute_batch(FAKE_DATA).unwrap();
        }
        let library = LocalLibrary::open(dir.path()).unwrap();
        (dir, library)
    }

    #[test]
    fn local_items_map_to_records() {
        let (dir, library) = fake_library();
        let items = library.items().unwrap();
        let keys: Vec<&str> = items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, vec!["ABCD2345", "PREP2345"]);

        let article = &items[0];
        assert_eq!(article.item_type, "journalArticle");
        assert_eq!(article.record.title, "Local Title");
        assert_eq!(article.record.authors, vec!["Ada Lovelace", "Local Lab"]);
        assert_eq!(article.record.year, Some(2019));
        assert_eq!(article.record.doi.as_deref(), Some("10.1000/local.1"));
        assert_eq!(
            article.record.venue.as_deref(),
            Some("Journal of Local Tests")
        );
        assert_eq!(article.record.pmid.as_deref(), Some("555"));
        assert_eq!(article.record.source_id.as_deref(), Some("ABCD2345"));
        assert_eq!(article.fields.get("volume").map(String::as_str), Some("12"));
        assert_eq!(article.collections, vec!["COLL2345", "SUBC2345"]);
        assert_eq!(
            article.pdf_paths(),
            vec![
                dir.path()
                    .join("storage")
                    .join("PDFK2345")
                    .join("paper.pdf")
                    .as_path()
            ]
        );

        let preprint = &items[1];
        assert_eq!(preprint.record.arxiv_id.as_deref(), Some("2401.01234"));
        assert_eq!(preprint.record.venue.as_deref(), Some("arXiv"));
        assert_eq!(preprint.record.year, Some(2024));
        assert_eq!(
            preprint.pdf_paths(),
            vec![Path::new("/elsewhere/preprint.pdf")]
        );
    }

    #[test]
    fn local_collections_are_listed() {
        let (_dir, library) = fake_library();
        let collections = library.collections().unwrap();
        assert_eq!(collections.len(), 2);
        assert_eq!(collections[1].name, "Sub");
        assert_eq!(collections[1].parent_id, Some(1));
        assert_eq!(collections[0].parent_id, None);
    }

    #[test]
    fn open_is_read_only() {
        let (_dir, library) = fake_library();
        assert!(library.conn.execute_batch("DELETE FROM items").is_err());
    }

    #[test]
    fn missing_database_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            LocalLibrary::open(dir.path()),
            Err(ZError::NotFound(_))
        ));
        assert_eq!(find_zotero_dir_in(dir.path()), None);
        std::fs::create_dir(dir.path().join("Zotero")).unwrap();
        std::fs::write(dir.path().join("Zotero").join(DB_FILE), b"").unwrap();
        assert_eq!(
            find_zotero_dir_in(dir.path()),
            Some(dir.path().join("Zotero"))
        );
    }

    #[test]
    fn uri_escapes_reserved_characters() {
        assert_eq!(
            immutable_uri(Path::new("/a b/z?#%.sqlite")).unwrap(),
            "file:///a b/z%3F%23%25.sqlite?immutable=1"
        );
        assert_eq!(
            immutable_uri(Path::new("rel/zotero.sqlite")).unwrap(),
            "file:rel/zotero.sqlite?immutable=1"
        );
    }

    #[test]
    fn attachment_paths_resolve_conservatively() {
        let base = Path::new("/data");
        assert_eq!(
            resolve_attachment_path(base, "KEYK2345", Some(0), Some("storage:a.pdf")),
            Some(PathBuf::from("/data/storage/KEYK2345/a.pdf"))
        );
        assert_eq!(
            resolve_attachment_path(base, "KEYK2345", Some(0), Some("storage:../x.pdf")),
            None
        );
        assert_eq!(
            resolve_attachment_path(base, "KEYK2345", Some(2), Some("attachments:p/a.pdf")),
            None
        );
        assert_eq!(
            resolve_attachment_path(base, "KEYK2345", Some(3), Some("https://x.org")),
            None
        );
        assert_eq!(
            resolve_attachment_path(base, "KEYK2345", Some(0), None),
            None
        );
    }
}
