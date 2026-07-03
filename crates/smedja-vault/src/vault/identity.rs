//! Embedder identity: the single model whose vectors this vault stores.

use super::Vault;
use crate::error::VaultError;

/// Identity of the embedding model whose vectors are stored in this vault.
///
/// Once set, [`Vault::insert`](super::Vault::insert) rejects embeddings whose
/// dimension does not match `dimensions`.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbedderIdentity {
    /// Name or identifier of the embedding model.
    pub model: String,
    /// Number of dimensions produced by the model.
    pub dimensions: usize,
}

impl Vault {
    /// Stores the embedder identity, overwriting any previously stored value.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] if the database write fails.
    #[must_use = "check the Result to confirm the embedder identity was stored"]
    pub fn set_embedder_identity(&mut self, identity: &EmbedderIdentity) -> Result<(), VaultError> {
        let json = format!(
            r#"{{"model":"{}","dimensions":{}}}"#,
            identity.model, identity.dimensions
        );
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

#[cfg(test)]
mod tests {
    use super::super::entry;
    use super::*;

    #[test]
    fn embedder_mismatch_returns_error() {
        let mut vault = Vault::open_in_memory().unwrap();
        vault
            .set_embedder_identity(&EmbedderIdentity {
                model: "test-model".to_string(),
                dimensions: 2,
            })
            .unwrap();

        // Attempt to insert with 3-dimensional embedding → mismatch.
        let mut e = entry("x", vec![1.0_f32, 0.0, 0.0]);
        e.namespace = "default".to_string();
        let err = vault.insert(&e).unwrap_err();
        assert!(
            matches!(err, VaultError::EmbedderMismatch { .. }),
            "expected EmbedderMismatch, got {err:?}"
        );
    }

    #[test]
    fn embedder_identity_round_trip() {
        let mut vault = Vault::open_in_memory().unwrap();
        assert!(vault.get_embedder_identity().unwrap().is_none());

        vault
            .set_embedder_identity(&EmbedderIdentity {
                model: "text-embedding-3-small".to_string(),
                dimensions: 1536,
            })
            .unwrap();

        let stored = vault.get_embedder_identity().unwrap().unwrap();
        assert_eq!(stored.model, "text-embedding-3-small");
        assert_eq!(stored.dimensions, 1536);
    }
}
