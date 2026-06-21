//! MCP server registry persistence.

use serde::{Deserialize, Serialize};

/// A registered MCP server entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServer {
    /// Unique identifier for this MCP server.
    pub id: String,
    /// Human-readable name for this server.
    pub name: String,
    /// Base URL of the MCP server.
    pub url: String,
    /// Transport protocol (`http` or `stdio`).
    pub transport: String,
    /// JSON-serialised list of tool descriptors exposed by this server.
    pub tools_json: String,
    /// Unix epoch timestamp of the last tool-list refresh.
    pub last_refresh: f64,
}

pub(crate) fn insert(
    conn: &rusqlite::Connection,
    server: &McpServer,
) -> Result<(), crate::error::IngotError> {
    conn.execute(
        "INSERT OR REPLACE INTO mcp_servers \
         (id, name, url, transport, tools_json, last_refresh) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            server.id,
            server.name,
            server.url,
            server.transport,
            server.tools_json,
            server.last_refresh,
        ],
    )?;
    Ok(())
}

pub(crate) fn list(
    conn: &rusqlite::Connection,
) -> Result<Vec<McpServer>, crate::error::IngotError> {
    let mut stmt = conn.prepare(
        "SELECT id, name, url, transport, tools_json, last_refresh \
         FROM mcp_servers ORDER BY name ASC",
    )?;
    let rows: Result<Vec<_>, _> = stmt
        .query_map([], |row| {
            Ok(McpServer {
                id: row.get(0)?,
                name: row.get(1)?,
                url: row.get(2)?,
                transport: row.get(3)?,
                tools_json: row.get(4)?,
                last_refresh: row.get(5)?,
            })
        })?
        .collect();
    Ok(rows?)
}

pub(crate) fn remove(
    conn: &rusqlite::Connection,
    id: &str,
) -> Result<(), crate::error::IngotError> {
    conn.execute(
        "DELETE FROM mcp_servers WHERE id = ?1",
        rusqlite::params![id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Ingot;

    fn server(name: &str) -> McpServer {
        McpServer {
            id: name.into(),
            name: name.into(),
            url: "http://localhost".into(),
            transport: "http".into(),
            tools_json: "[]".into(),
            last_refresh: 0.0,
        }
    }

    #[test]
    fn register_and_list() {
        let mut ig = Ingot::open_in_memory().unwrap();
        ig.register_mcp_server(&server("fs")).unwrap();
        ig.register_mcp_server(&server("gh")).unwrap();
        let list = ig.list_mcp_servers().unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn list_is_sorted_by_name() {
        let mut ig = Ingot::open_in_memory().unwrap();
        ig.register_mcp_server(&server("zebra")).unwrap();
        ig.register_mcp_server(&server("alpha")).unwrap();
        let list = ig.list_mcp_servers().unwrap();
        assert_eq!(list[0].name, "alpha");
        assert_eq!(list[1].name, "zebra");
    }

    #[test]
    fn remove_server() {
        let mut ig = Ingot::open_in_memory().unwrap();
        ig.register_mcp_server(&server("x")).unwrap();
        ig.remove_mcp_server("x").unwrap();
        assert!(ig.list_mcp_servers().unwrap().is_empty());
    }

    #[test]
    fn replace_on_duplicate_id() {
        let mut ig = Ingot::open_in_memory().unwrap();
        let mut s = server("tool");
        ig.register_mcp_server(&s).unwrap();
        s.url = "http://updated".into();
        ig.register_mcp_server(&s).unwrap();
        let list = ig.list_mcp_servers().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].url, "http://updated");
    }
}
