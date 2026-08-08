//! redb-backed metadata + markup-sidecar store.

use std::path::Path;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

use crate::script::model::ChatMessage;

use super::{DocMeta, LibraryError, PageTextStatus, SavedMarkup};

const DOCS: TableDefinition<&str, &[u8]> = TableDefinition::new("docs");
const ANNOTS: TableDefinition<&str, &[u8]> = TableDefinition::new("annots");
const CHATS: TableDefinition<&str, &[u8]> = TableDefinition::new("chats");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

const SCHEMA_VERSION: u64 = 1;

/// Key in [`META`] holding the layout of the tantivy index on disk. The index
/// is a cache that can always be rebuilt, so a change of layout is handled by
/// throwing it away rather than by migrating it.
const SEARCH_SCHEMA: &str = "search_schema";

pub struct MetaDb {
    db: Database,
}

fn db_err(e: impl std::fmt::Display) -> LibraryError {
    LibraryError::Db(e.to_string())
}

impl MetaDb {
    pub fn open(path: &Path) -> Result<Self, LibraryError> {
        let db = Database::create(path).map_err(db_err)?;
        let this = Self { db };
        // Make sure all tables exist and stamp the schema version.
        let tx = this.db.begin_write().map_err(db_err)?;
        {
            let _ = tx.open_table(DOCS).map_err(db_err)?;
            let _ = tx.open_table(ANNOTS).map_err(db_err)?;
            let _ = tx.open_table(CHATS).map_err(db_err)?;
            let mut meta = tx.open_table(META).map_err(db_err)?;
            if meta.get("schema").map_err(db_err)?.is_none() {
                meta.insert("schema", SCHEMA_VERSION).map_err(db_err)?;
            }
        }
        tx.commit().map_err(db_err)?;
        Ok(this)
    }

    pub fn put_doc(&self, meta: &DocMeta) -> Result<(), LibraryError> {
        let json = serde_json::to_vec(meta).map_err(db_err)?;
        let tx = self.db.begin_write().map_err(db_err)?;
        {
            let mut table = tx.open_table(DOCS).map_err(db_err)?;
            table
                .insert(meta.id.as_str(), json.as_slice())
                .map_err(db_err)?;
        }
        tx.commit().map_err(db_err)?;
        Ok(())
    }

    pub fn get_doc(&self, id: &str) -> Result<Option<DocMeta>, LibraryError> {
        let tx = self.db.begin_read().map_err(db_err)?;
        let table = tx.open_table(DOCS).map_err(db_err)?;
        let Some(guard) = table.get(id).map_err(db_err)? else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_slice(guard.value()).map_err(db_err)?))
    }

    /// Read-modify-write the indexing state of one document in a single write
    /// transaction. A missing document is not an error (it may have been
    /// deleted while the indexer was working on it).
    pub fn update_text_status(
        &self,
        id: &str,
        statuses: &[PageTextStatus],
        error: Option<&str>,
    ) -> Result<(), LibraryError> {
        let tx = self.db.begin_write().map_err(db_err)?;
        {
            let mut table = tx.open_table(DOCS).map_err(db_err)?;
            let current: Option<DocMeta> = match table.get(id).map_err(db_err)? {
                Some(guard) => Some(serde_json::from_slice(guard.value()).map_err(db_err)?),
                None => None,
            };
            if let Some(mut meta) = current {
                meta.text_status = statuses.to_vec();
                meta.index_error = error.map(str::to_owned);
                let json = serde_json::to_vec(&meta).map_err(db_err)?;
                table.insert(id, json.as_slice()).map_err(db_err)?;
            }
        }
        tx.commit().map_err(db_err)?;
        Ok(())
    }

    /// Store the summary and automatic tags the assistant produced. Like
    /// [`Self::update_text_status`] this is a read-modify-write in one
    /// transaction, and a document deleted meanwhile is not an error.
    pub fn update_enrichment(
        &self,
        id: &str,
        summary: Option<&str>,
        auto_tags: &[String],
    ) -> Result<(), LibraryError> {
        let tx = self.db.begin_write().map_err(db_err)?;
        {
            let mut table = tx.open_table(DOCS).map_err(db_err)?;
            let current: Option<DocMeta> = match table.get(id).map_err(db_err)? {
                Some(guard) => Some(serde_json::from_slice(guard.value()).map_err(db_err)?),
                None => None,
            };
            if let Some(mut meta) = current {
                meta.summary = summary.map(str::to_owned);
                meta.auto_tags = auto_tags.to_vec();
                let json = serde_json::to_vec(&meta).map_err(db_err)?;
                table.insert(id, json.as_slice()).map_err(db_err)?;
            }
        }
        tx.commit().map_err(db_err)?;
        Ok(())
    }

    /// The layout of the tantivy index this library last wrote, if it has
    /// written one. `None` means "before the version was recorded", which is
    /// every index written by v0.3 and earlier.
    pub fn search_schema(&self) -> Result<Option<u64>, LibraryError> {
        let tx = self.db.begin_read().map_err(db_err)?;
        let table = tx.open_table(META).map_err(db_err)?;
        Ok(table
            .get(SEARCH_SCHEMA)
            .map_err(db_err)?
            .map(|guard| guard.value()))
    }

    pub fn set_search_schema(&self, version: u64) -> Result<(), LibraryError> {
        let tx = self.db.begin_write().map_err(db_err)?;
        {
            let mut table = tx.open_table(META).map_err(db_err)?;
            table.insert(SEARCH_SCHEMA, version).map_err(db_err)?;
        }
        tx.commit().map_err(db_err)?;
        Ok(())
    }

    pub fn list_docs(&self) -> Result<Vec<DocMeta>, LibraryError> {
        let tx = self.db.begin_read().map_err(db_err)?;
        let table = tx.open_table(DOCS).map_err(db_err)?;
        let mut out = Vec::new();
        for entry in table.iter().map_err(db_err)? {
            let (_, value) = entry.map_err(db_err)?;
            out.push(serde_json::from_slice(value.value()).map_err(db_err)?);
        }
        Ok(out)
    }

    pub fn delete_doc(&self, id: &str) -> Result<(), LibraryError> {
        let tx = self.db.begin_write().map_err(db_err)?;
        {
            let mut docs = tx.open_table(DOCS).map_err(db_err)?;
            docs.remove(id).map_err(db_err)?;
            let mut annots = tx.open_table(ANNOTS).map_err(db_err)?;
            annots.remove(id).map_err(db_err)?;
            let mut chats = tx.open_table(CHATS).map_err(db_err)?;
            chats.remove(id).map_err(db_err)?;
        }
        tx.commit().map_err(db_err)?;
        Ok(())
    }

    pub fn put_markup(&self, id: &str, markup: &SavedMarkup) -> Result<(), LibraryError> {
        let json = serde_json::to_vec(markup).map_err(db_err)?;
        let tx = self.db.begin_write().map_err(db_err)?;
        {
            let mut table = tx.open_table(ANNOTS).map_err(db_err)?;
            table.insert(id, json.as_slice()).map_err(db_err)?;
        }
        tx.commit().map_err(db_err)?;
        Ok(())
    }

    pub fn get_markup(&self, id: &str) -> Result<Option<SavedMarkup>, LibraryError> {
        let tx = self.db.begin_read().map_err(db_err)?;
        let table = tx.open_table(ANNOTS).map_err(db_err)?;
        let Some(guard) = table.get(id).map_err(db_err)? else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_slice(guard.value()).map_err(db_err)?))
    }

    /// Store one document's chat transcript. An empty conversation removes the
    /// record rather than writing an empty one, so clearing a chat forgets it.
    pub fn put_chat(&self, id: &str, messages: &[ChatMessage]) -> Result<(), LibraryError> {
        let tx = self.db.begin_write().map_err(db_err)?;
        {
            let mut table = tx.open_table(CHATS).map_err(db_err)?;
            if messages.is_empty() {
                table.remove(id).map_err(db_err)?;
            } else {
                let json = serde_json::to_vec(messages).map_err(db_err)?;
                table.insert(id, json.as_slice()).map_err(db_err)?;
            }
        }
        tx.commit().map_err(db_err)?;
        Ok(())
    }

    pub fn get_chat(&self, id: &str) -> Result<Vec<ChatMessage>, LibraryError> {
        let tx = self.db.begin_read().map_err(db_err)?;
        let table = tx.open_table(CHATS).map_err(db_err)?;
        let Some(guard) = table.get(id).map_err(db_err)? else {
            return Ok(Vec::new());
        };
        // A transcript written by a future version that this one cannot read is
        // not worth failing the document's whole open for.
        Ok(serde_json::from_slice(guard.value()).unwrap_or_default())
    }
}
