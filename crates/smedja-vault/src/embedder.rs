//! Embedder identity storage for the [`Vault`].
//!
//! The identity/embedder half of the vault: persisting and reading the single
//! [`EmbedderIdentity`] stored in the `vault_meta` table. Once set, it gates the
//! dimension of every embedding accepted by [`Vault::insert`].

use crate::error::VaultError;
use crate::vault::{EmbedderIdentity, Vault};

impl Vault {
    /// Stores the embedder identity, overwriting any previously stored value.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] if the database write fails.
    #[must_use = "check the Result to confirm the embedder identity was stored"]
    pub fn set_embedder_identity(&mut self, identity: &EmbedderIdentity) -> Result<(), VaultError> {
        // Build the JSON with serde_json so a model name containing quotes,
        // backslashes, or control characters is escaped correctly. Hand-rolled
        // string interpolation would emit invalid JSON that get_embedder_identity
        // could never parse back.
        let json = serde_json::json!({
            "model": identity.model,
            "dimensions": identity.dimensions,
        })
        .to_string();
        self.conn.execute(
            "INSERT OR REPLACE INTO vault_meta (id, meta_key, meta_val) VALUES (1, 'embedder', ?1)",
            rusqlite::params![json],
        )?;
        Ok(())
    }

    /// Returns the stored embedder identity, or `None` if none has been set.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] if the query fails, or [`VaultError::Json`] if
    /// the stored JSON is malformed.
    #[must_use = "check the Result and use the returned identity"]
    pub fn get_embedder_identity(&self) -> Result<Option<EmbedderIdentity>, VaultError> {
        let result: rusqlite::Result<String> = self.conn.query_row(
            "SELECT meta_val FROM vault_meta WHERE id = 1 AND meta_key = 'embedder'",
            [],
            |row| row.get(0),
        );
        match result {
            Ok(json_str) => {
                let v: serde_json::Value = serde_json::from_str(&json_str)?;
                let model = v["model"].as_str().unwrap_or_default().to_string();
                let dimensions =
                    usize::try_from(v["dimensions"].as_u64().unwrap_or(0)).unwrap_or(0);
                Ok(Some(EmbedderIdentity { model, dimensions }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(VaultError::Db(e)),
        }
    }
}
