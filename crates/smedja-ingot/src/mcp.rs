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

/// Updates the cached tool list and refresh timestamp for a registered MCP server.
pub(crate) fn update_tools(
    conn: &rusqlite::Connection,
    name: &str,
    tools_json: &str,
) -> Result<(), crate::error::IngotError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    conn.execute(
        "UPDATE mcp_servers SET tools_json = ?1, last_refresh = ?2 WHERE name = ?3",
        rusqlite::params![tools_json, now, name],
    )?;
    Ok(())
}

/// Returns all registered MCP servers whose `last_refresh` is older than
/// `older_than_secs` seconds ago, or that have never been refreshed.
pub(crate) fn stale(
    conn: &rusqlite::Connection,
    older_than_secs: f64,
) -> Result<Vec<McpServer>, crate::error::IngotError> {
    let threshold = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        - older_than_secs;
    let mut stmt = conn.prepare(
        "SELECT id, name, url, transport, tools_json, last_refresh \
         FROM mcp_servers WHERE last_refresh < ?1 OR last_refresh IS NULL",
    )?;
    let rows: Result<Vec<_>, _> = stmt
        .query_map(rusqlite::params![threshold], |row| {
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

/// Returns all MCP servers with a non-empty `tools_json`, as `(name, tools_json)` pairs.
pub(crate) fn all_tools(
    conn: &rusqlite::Connection,
) -> Result<Vec<(String, String)>, crate::error::IngotError> {
    let mut stmt = conn.prepare(
        "SELECT name, tools_json FROM mcp_servers \
         WHERE tools_json IS NOT NULL AND tools_json != '' AND tools_json != '[]'",
    )?;
    let rows: Result<Vec<_>, _> = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect();
    Ok(rows?)
}

/// Looks up a single MCP server by its registered name.
pub(crate) fn by_name(
    conn: &rusqlite::Connection,
    name: &str,
) -> Result<Option<McpServer>, crate::error::IngotError> {
    match conn.query_row(
        "SELECT id, name, url, transport, tools_json, last_refresh \
         FROM mcp_servers WHERE name = ?1",
        rusqlite::params![name],
        |row| {
            Ok(McpServer {
                id: row.get(0)?,
                name: row.get(1)?,
                url: row.get(2)?,
                transport: row.get(3)?,
                tools_json: row.get(4)?,
                last_refresh: row.get(5)?,
            })
        },
    ) {
        Ok(server) => Ok(Some(server)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(crate::error::IngotError::Db(e)),
    }
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

    #[test]
    fn update_mcp_tools_sets_tools_json_and_refresh() {
        let mut ig = Ingot::open_in_memory().unwrap();
        ig.register_mcp_server(&server("svc")).unwrap();
        ig.update_mcp_tools("svc", r#"[{"name":"tool1"}]"#).unwrap();
        let list = ig.list_mcp_servers().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].tools_json, r#"[{"name":"tool1"}]"#);
        assert!(
            list[0].last_refresh > 0.0,
            "last_refresh must be non-zero after update"
        );
    }

    #[test]
    fn get_stale_servers_returns_old_entries() {
        let mut ig = Ingot::open_in_memory().unwrap();

        // Stale server: last_refresh = 0 (epoch start, always older than threshold).
        ig.register_mcp_server(&server("stale")).unwrap();

        // Fresh server: last_refresh = now.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        let fresh = McpServer {
            last_refresh: now,
            ..server("fresh")
        };
        ig.register_mcp_server(&fresh).unwrap();

        let stale = ig.get_stale_servers(3600.0).unwrap();
        assert_eq!(stale.len(), 1, "only the stale server should be returned");
        assert_eq!(stale[0].name, "stale");
    }

    #[test]
    fn get_all_mcp_tools_filters_empty() {
        let mut ig = Ingot::open_in_memory().unwrap();

        // Empty tools_json — must be excluded.
        ig.register_mcp_server(&server("empty")).unwrap();

        // Server with real tool data — must be included.
        let with_tools = McpServer {
            tools_json: r#"[{"name":"read"}]"#.into(),
            ..server("has-tools")
        };
        ig.register_mcp_server(&with_tools).unwrap();

        let tools = ig.get_all_mcp_tools().unwrap();
        assert_eq!(
            tools.len(),
            1,
            "only servers with non-empty tools_json should be returned"
        );
        assert_eq!(tools[0].0, "has-tools");
        assert_eq!(tools[0].1, r#"[{"name":"read"}]"#);
    }

    #[test]
    fn get_mcp_server_by_name_returns_server() {
        let mut ig = Ingot::open_in_memory().unwrap();
        ig.register_mcp_server(&server("target")).unwrap();
        let found = ig.get_mcp_server_by_name("target").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "target");
    }

    #[test]
    fn get_mcp_server_by_name_returns_none_when_missing() {
        let ig = Ingot::open_in_memory().unwrap();
        let found = ig.get_mcp_server_by_name("does-not-exist").unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn find_mcp_server_for_tool_returns_owning_server() {
        let mut ig = Ingot::open_in_memory().unwrap();
        let s = McpServer {
            tools_json: r#"[{"name":"echo","description":"Echo input","input_schema":{}}]"#
                .into(),
            ..server("echo-server")
        };
        ig.register_mcp_server(&s).unwrap();

        let found = ig.find_mcp_server_for_tool("echo").unwrap();
        assert!(found.is_some(), "must find the server that owns 'echo'");
        assert_eq!(found.unwrap().name, "echo-server");
    }

    #[test]
    fn find_mcp_server_for_tool_returns_none_when_tool_not_registered() {
        let mut ig = Ingot::open_in_memory().unwrap();
        let s = McpServer {
            tools_json: r#"[{"name":"echo","description":"Echo"}]"#.into(),
            ..server("echo-server")
        };
        ig.register_mcp_server(&s).unwrap();

        let found = ig.find_mcp_server_for_tool("read_file").unwrap();
        assert!(found.is_none(), "unregistered tool must return None");
    }

    #[test]
    fn find_mcp_server_for_tool_returns_none_on_empty_registry() {
        let ig = Ingot::open_in_memory().unwrap();
        let found = ig.find_mcp_server_for_tool("any_tool").unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn find_mcp_server_for_tool_matches_across_multiple_servers() {
        let mut ig = Ingot::open_in_memory().unwrap();
        ig.register_mcp_server(&McpServer {
            tools_json: r#"[{"name":"tool_a"}]"#.into(),
            ..server("server-a")
        })
        .unwrap();
        ig.register_mcp_server(&McpServer {
            tools_json: r#"[{"name":"tool_b"}]"#.into(),
            ..server("server-b")
        })
        .unwrap();

        let found_a = ig.find_mcp_server_for_tool("tool_a").unwrap();
        let found_b = ig.find_mcp_server_for_tool("tool_b").unwrap();

        assert_eq!(found_a.unwrap().name, "server-a");
        assert_eq!(found_b.unwrap().name, "server-b");
    }
}
