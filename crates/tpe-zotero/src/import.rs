//! Body of `POST http://127.0.0.1:<port>/zotero/import`, sent by the Zotero
//! plugin in `integrations/zotero-plugin`. The app track implements the
//! server; this module fixes the wire format and maps it to records.
//!
//! The request carries an `X-TPE-Token` header whose value must equal the
//! token configured in both the plugin preferences and the app.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tpe_common::PaperRecord;

use crate::error::ZError;
use crate::item::ZItem;

/// Value of [`ImportRequest::schema`] understood by this crate.
pub const IMPORT_SCHEMA: &str = "tpe.zotero.import/1";
/// Header carrying the shared token.
pub const TOKEN_HEADER: &str = "X-TPE-Token";
/// Default port of the app's local server.
pub const DEFAULT_PORT: u16 = 47821;

/// The whole request body.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImportRequest {
    /// Always [`IMPORT_SCHEMA`].
    pub schema: String,
    /// Plugin version string.
    #[serde(default)]
    pub plugin_version: Option<String>,
    /// `Zotero.version` of the sending client.
    #[serde(default)]
    pub zotero_version: Option<String>,
    /// Selected regular items.
    pub items: Vec<ImportItem>,
}

/// One selected item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImportItem {
    /// Item key.
    pub key: String,
    /// Local `libraryID`.
    pub library_id: i64,
    /// `item.toJSON()` from Zotero: the same editable JSON as the Web API's
    /// `data` property (`itemType`, `title`, `creators`, `DOI`, `extra`, ...).
    pub data: Map<String, Value>,
    /// Child attachments.
    #[serde(default)]
    pub attachments: Vec<ImportAttachment>,
}

/// One attachment of a selected item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportAttachment {
    /// Attachment item key.
    pub key: String,
    /// Attachment title.
    #[serde(default)]
    pub title: Option<String>,
    /// MIME type.
    #[serde(default)]
    pub content_type: Option<String>,
    /// Zotero link mode (0 imported file, 1 imported URL, 2 linked file, 3 linked URL).
    #[serde(default)]
    pub link_mode: Option<i64>,
    /// Absolute file path when the file exists locally, else `None`.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// URL for linked-URL attachments.
    #[serde(default)]
    pub url: Option<String>,
}

impl ImportRequest {
    /// Parse and check the schema tag.
    pub fn parse(body: &str) -> Result<Self, ZError> {
        let request: Self = serde_json::from_str(body)?;
        if request.schema != IMPORT_SCHEMA {
            return Err(ZError::Parse(format!(
                "unsupported import schema {:?}",
                request.schema
            )));
        }
        Ok(request)
    }
}

impl ImportItem {
    /// The item as a [`ZItem`] (version unknown locally, so 0).
    pub fn to_zitem(&self) -> ZItem {
        ZItem {
            key: self.key.clone(),
            version: 0,
            item_type: self
                .data
                .get("itemType")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            data: self.data.clone(),
        }
    }

    /// Map to a [`PaperRecord`] with the same rules as the Web API items.
    pub fn to_record(&self) -> PaperRecord {
        self.to_zitem().to_record()
    }

    /// Paths of attachments with content type `application/pdf`.
    pub fn pdf_paths(&self) -> Vec<&Path> {
        self.attachments
            .iter()
            .filter(|a| a.content_type.as_deref() == Some("application/pdf"))
            .filter_map(|a| a.path.as_deref())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shape produced by integrations/zotero-plugin/bootstrap.js.
    const BODY: &str = r#"{
      "schema": "tpe.zotero.import/1",
      "plugin_version": "0.1.0",
      "zotero_version": "7.0.11",
      "items": [{
        "key": "ABCD2345",
        "library_id": 1,
        "data": {
          "key": "ABCD2345", "version": 0, "itemType": "journalArticle",
          "title": "Plugin Paper",
          "creators": [{"creatorType": "author", "firstName": "Ada", "lastName": "Lovelace"}],
          "date": "2022", "DOI": "10.1000/PLUG", "publicationTitle": "J",
          "extra": "arXiv: 2201.00001", "tags": [], "collections": [], "relations": {}
        },
        "attachments": [
          {"key": "PDFK2345", "title": "Full Text PDF", "content_type": "application/pdf",
           "link_mode": 0, "path": "/Users/a/Zotero/storage/PDFK2345/p.pdf", "url": null},
          {"key": "LINK2345", "title": "Site", "content_type": null,
           "link_mode": 3, "path": null, "url": "https://example.org"}
        ]
      }]
    }"#;

    #[test]
    fn plugin_body_parses_and_maps() {
        let request = ImportRequest::parse(BODY).unwrap();
        assert_eq!(request.items.len(), 1);
        let item = &request.items[0];
        let record = item.to_record();
        assert_eq!(record.title, "Plugin Paper");
        assert_eq!(record.doi.as_deref(), Some("10.1000/plug"));
        assert_eq!(record.arxiv_id.as_deref(), Some("2201.00001"));
        assert_eq!(record.year, Some(2022));
        assert_eq!(
            item.pdf_paths(),
            vec![Path::new("/Users/a/Zotero/storage/PDFK2345/p.pdf")]
        );
    }

    #[test]
    fn wrong_schema_is_rejected() {
        let body = BODY.replace("tpe.zotero.import/1", "other/9");
        assert!(matches!(ImportRequest::parse(&body), Err(ZError::Parse(_))));
    }
}
