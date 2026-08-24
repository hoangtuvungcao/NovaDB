//! Role-based access control: create roles, grant/revoke roles to users,
//! grant/revoke table privileges to roles.

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::Result;

/// A role record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Role {
    pub role_name: String,
    pub description: Option<String>,
    pub created_at_ms: i64,
}

/// A grant record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grant {
    pub role_name: String,
    pub table_name: String,
    pub privilege: String,
}

/// Create a new role.
pub fn create_role(
    connection: &Connection,
    role_name: &str,
    description: Option<&str>,
) -> Result<()> {
    connection.execute(
        "INSERT INTO _novadb_roles (role_name, description) VALUES (?1, ?2)",
        params![role_name, description],
    )?;
    Ok(())
}

/// Drop a role.
pub fn drop_role(connection: &Connection, role_name: &str) -> Result<bool> {
    let deleted = connection.execute(
        "DELETE FROM _novadb_roles WHERE role_name = ?1",
        [role_name],
    )?;
    Ok(deleted > 0)
}

/// List all roles.
pub fn list_roles(connection: &Connection) -> Result<Vec<Role>> {
    let mut stmt = connection.prepare(
        "SELECT role_name, description, created_at_ms FROM _novadb_roles ORDER BY role_name",
    )?;
    let roles = stmt
        .query_map([], |row| {
            Ok(Role {
                role_name: row.get(0)?,
                description: row.get(1)?,
                created_at_ms: row.get(2)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(roles)
}

/// Grant a role to a user.
pub fn grant_role(
    connection: &Connection,
    username: &str,
    role_name: &str,
) -> Result<()> {
    connection.execute(
        "INSERT OR IGNORE INTO _novadb_user_roles (username, role_name) VALUES (?1, ?2)",
        params![username, role_name],
    )?;
    Ok(())
}

/// Revoke a role from a user.
pub fn revoke_role(
    connection: &Connection,
    username: &str,
    role_name: &str,
) -> Result<bool> {
    let deleted = connection.execute(
        "DELETE FROM _novadb_user_roles WHERE username = ?1 AND role_name = ?2",
        params![username, role_name],
    )?;
    Ok(deleted > 0)
}

/// Get all roles for a user.
pub fn user_roles(connection: &Connection, username: &str) -> Result<Vec<String>> {
    let mut stmt = connection.prepare(
        "SELECT role_name FROM _novadb_user_roles WHERE username = ?1 ORDER BY role_name",
    )?;
    let roles = stmt
        .query_map([username], |row| row.get(0))?
        .collect::<std::result::Result<Vec<String>, _>>()?;
    Ok(roles)
}

/// Grant a table privilege to a role.
pub fn grant_privilege(
    connection: &Connection,
    role_name: &str,
    table_name: &str,
    privilege: &str,
) -> Result<()> {
    connection.execute(
        "INSERT OR IGNORE INTO _novadb_grants (role_name, table_name, privilege)
         VALUES (?1, ?2, ?3)",
        params![role_name, table_name, privilege.to_uppercase()],
    )?;
    Ok(())
}

/// Revoke a table privilege from a role.
pub fn revoke_privilege(
    connection: &Connection,
    role_name: &str,
    table_name: &str,
    privilege: &str,
) -> Result<bool> {
    let deleted = connection.execute(
        "DELETE FROM _novadb_grants
         WHERE role_name = ?1 AND table_name = ?2 AND privilege = ?3",
        params![role_name, table_name, privilege.to_uppercase()],
    )?;
    Ok(deleted > 0)
}

/// Get all grants for a role.
pub fn role_grants(connection: &Connection, role_name: &str) -> Result<Vec<Grant>> {
    let mut stmt = connection.prepare(
        "SELECT role_name, table_name, privilege
         FROM _novadb_grants WHERE role_name = ?1
         ORDER BY table_name, privilege",
    )?;
    let grants = stmt
        .query_map([role_name], |row| {
            Ok(Grant {
                role_name: row.get(0)?,
                table_name: row.get(1)?,
                privilege: row.get(2)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(grants)
}

/// Check if a user has a specific privilege on a table.
/// Superusers always have access.
pub fn check_privilege(
    connection: &Connection,
    username: &str,
    table_name: &str,
    privilege: &str,
) -> Result<bool> {
    // Check if superuser
    let is_super: bool = connection.query_row(
        "SELECT COALESCE((SELECT is_superuser FROM _novadb_users WHERE username = ?1), 0)",
        [username],
        |row| row.get(0),
    )?;
    if is_super {
        return Ok(true);
    }

    // Check through role grants
    let has_access: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM _novadb_user_roles ur
            JOIN _novadb_grants g ON ur.role_name = g.role_name
            WHERE ur.username = ?1
              AND g.table_name = ?2
              AND (g.privilege = ?3 OR g.privilege = 'ALL')
        )",
        params![username, table_name, privilege.to_uppercase()],
        |row| row.get(0),
    )?;
    Ok(has_access)
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::auth::{bootstrap_auth_schema, users};

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        bootstrap_auth_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn create_and_list_roles() {
        let conn = setup();
        create_role(&conn, "analysts", Some("Data analysts")).unwrap();

        let roles = list_roles(&conn).unwrap();
        // 3 built-in + 1 custom
        assert!(roles.iter().any(|r| r.role_name == "analysts"));
        assert!(roles.iter().any(|r| r.role_name == "novadb_admin"));
    }

    #[test]
    fn grant_and_revoke_role() {
        let conn = setup();
        users::create_user(&conn, "alice", "pass", false).unwrap();
        grant_role(&conn, "alice", "novadb_readonly").unwrap();

        let roles = user_roles(&conn, "alice").unwrap();
        assert_eq!(roles, vec!["novadb_readonly"]);

        revoke_role(&conn, "alice", "novadb_readonly").unwrap();
        let roles = user_roles(&conn, "alice").unwrap();
        assert!(roles.is_empty());
    }

    #[test]
    fn grant_and_check_privilege() {
        let conn = setup();
        users::create_user(&conn, "bob", "pass", false).unwrap();
        create_role(&conn, "reader", None).unwrap();
        grant_role(&conn, "bob", "reader").unwrap();
        grant_privilege(&conn, "reader", "notes", "SELECT").unwrap();

        assert!(check_privilege(&conn, "bob", "notes", "SELECT").unwrap());
        assert!(!check_privilege(&conn, "bob", "notes", "DELETE").unwrap());
    }

    #[test]
    fn superuser_bypasses_privilege_check() {
        let conn = setup();
        users::create_user(&conn, "admin", "pass", true).unwrap();

        // No explicit grants, but superuser should pass
        assert!(check_privilege(&conn, "admin", "anything", "DELETE").unwrap());
    }

    #[test]
    fn all_privilege_grants_everything() {
        let conn = setup();
        users::create_user(&conn, "dev", "pass", false).unwrap();
        create_role(&conn, "owner", None).unwrap();
        grant_role(&conn, "dev", "owner").unwrap();
        grant_privilege(&conn, "owner", "data", "ALL").unwrap();

        assert!(check_privilege(&conn, "dev", "data", "SELECT").unwrap());
        assert!(check_privilege(&conn, "dev", "data", "INSERT").unwrap());
        assert!(check_privilege(&conn, "dev", "data", "DELETE").unwrap());
    }

    #[test]
    fn drop_role_cascades_grants() {
        let conn = setup();
        create_role(&conn, "temp_role", None).unwrap();
        grant_privilege(&conn, "temp_role", "table1", "SELECT").unwrap();

        assert!(drop_role(&conn, "temp_role").unwrap());

        // Grants should be cascade-deleted
        let grants = role_grants(&conn, "temp_role").unwrap();
        assert!(grants.is_empty());
    }
}
