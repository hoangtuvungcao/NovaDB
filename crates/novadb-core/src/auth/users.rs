//! User management: create, authenticate, list, alter, and drop users.

use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};

use crate::Result;

/// A database user record.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct User {
    pub username: String,
    pub is_active: bool,
    pub is_superuser: bool,
    pub failed_logins: i64,
    pub last_login_ms: Option<i64>,
    pub created_at_ms: i64,
}

/// Create a new user with a hashed password.
pub fn create_user(
    connection: &Connection,
    username: &str,
    password: &str,
    is_superuser: bool,
) -> Result<()> {
    let hash = hash_password(username, password);
    connection.execute(
        "INSERT INTO _novadb_users (username, password_hash, is_superuser)
         VALUES (?1, ?2, ?3)",
        params![username, hash, is_superuser],
    )?;
    Ok(())
}

/// Authenticate a user by username and password.
/// Returns the user on success, or None on failure.
pub fn authenticate(
    connection: &Connection,
    username: &str,
    password: &str,
) -> Result<Option<User>> {
    let hash = hash_password(username, password);
    let result = connection.query_row(
        "SELECT username, is_active, is_superuser, failed_logins, last_login_ms, created_at_ms
         FROM _novadb_users
         WHERE username = ?1 AND password_hash = ?2 AND is_active = 1",
        params![username, hash],
        |row| {
            Ok(User {
                username: row.get(0)?,
                is_active: row.get(1)?,
                is_superuser: row.get(2)?,
                failed_logins: row.get(3)?,
                last_login_ms: row.get(4)?,
                created_at_ms: row.get(5)?,
            })
        },
    );

    match result {
        Ok(user) => {
            // Reset failed logins and update last login
            connection.execute(
                "UPDATE _novadb_users SET failed_logins = 0,
                 last_login_ms = strftime('%s','now') * 1000
                 WHERE username = ?1",
                [username],
            )?;
            Ok(Some(user))
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            // Increment failed logins if user exists
            connection.execute(
                "UPDATE _novadb_users SET failed_logins = failed_logins + 1
                 WHERE username = ?1",
                [username],
            )?;
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}

/// List all users.
pub fn list_users(connection: &Connection) -> Result<Vec<User>> {
    let mut stmt = connection.prepare(
        "SELECT username, is_active, is_superuser, failed_logins, last_login_ms, created_at_ms
         FROM _novadb_users ORDER BY username",
    )?;
    let users = stmt
        .query_map([], |row| {
            Ok(User {
                username: row.get(0)?,
                is_active: row.get(1)?,
                is_superuser: row.get(2)?,
                failed_logins: row.get(3)?,
                last_login_ms: row.get(4)?,
                created_at_ms: row.get(5)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(users)
}

/// Change a user's password.
pub fn change_password(
    connection: &Connection,
    username: &str,
    new_password: &str,
) -> Result<bool> {
    let hash = hash_password(username, new_password);
    let changed = connection.execute(
        "UPDATE _novadb_users SET password_hash = ?1 WHERE username = ?2",
        params![hash, username],
    )?;
    Ok(changed > 0)
}

/// Deactivate a user (soft delete).
pub fn deactivate_user(connection: &Connection, username: &str) -> Result<bool> {
    let changed = connection.execute(
        "UPDATE _novadb_users SET is_active = 0 WHERE username = ?1",
        [username],
    )?;
    Ok(changed > 0)
}

/// Activate a previously deactivated user.
pub fn activate_user(connection: &Connection, username: &str) -> Result<bool> {
    let changed = connection.execute(
        "UPDATE _novadb_users SET is_active = 1 WHERE username = ?1",
        [username],
    )?;
    Ok(changed > 0)
}

/// Drop a user entirely.
pub fn drop_user(connection: &Connection, username: &str) -> Result<bool> {
    let deleted = connection.execute(
        "DELETE FROM _novadb_users WHERE username = ?1",
        [username],
    )?;
    Ok(deleted > 0)
}

/// Hash a password with the username as salt.
/// Uses SHA-256 with username+password concatenation.
/// In production, use SCRAM-SHA-256 or argon2.
fn hash_password(username: &str, password: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("novadb:{username}:{password}").as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::auth::bootstrap_auth_schema;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        bootstrap_auth_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn create_and_authenticate_user() {
        let conn = setup();
        create_user(&conn, "alice", "secret123", false).unwrap();

        let user = authenticate(&conn, "alice", "secret123")
            .unwrap()
            .expect("should authenticate");
        assert_eq!(user.username, "alice");
        assert!(!user.is_superuser);
        assert!(user.is_active);
    }

    #[test]
    fn wrong_password_fails() {
        let conn = setup();
        create_user(&conn, "bob", "correct", false).unwrap();

        let result = authenticate(&conn, "bob", "wrong").unwrap();
        assert!(result.is_none());

        // Failed login count should increment
        let users = list_users(&conn).unwrap();
        let bob = users.iter().find(|u| u.username == "bob").unwrap();
        assert_eq!(bob.failed_logins, 1);
    }

    #[test]
    fn deactivated_user_cannot_login() {
        let conn = setup();
        create_user(&conn, "charlie", "pass", false).unwrap();
        deactivate_user(&conn, "charlie").unwrap();

        let result = authenticate(&conn, "charlie", "pass").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn change_password_works() {
        let conn = setup();
        create_user(&conn, "dave", "old_pass", false).unwrap();
        change_password(&conn, "dave", "new_pass").unwrap();

        assert!(authenticate(&conn, "dave", "old_pass").unwrap().is_none());
        assert!(authenticate(&conn, "dave", "new_pass").unwrap().is_some());
    }

    #[test]
    fn list_users_returns_all() {
        let conn = setup();
        create_user(&conn, "alice", "a", false).unwrap();
        create_user(&conn, "bob", "b", true).unwrap();

        let users = list_users(&conn).unwrap();
        assert_eq!(users.len(), 2);
        assert_eq!(users[0].username, "alice");
        assert_eq!(users[1].username, "bob");
        assert!(users[1].is_superuser);
    }

    #[test]
    fn drop_user_removes_completely() {
        let conn = setup();
        create_user(&conn, "ephemeral", "x", false).unwrap();
        assert!(drop_user(&conn, "ephemeral").unwrap());
        assert_eq!(list_users(&conn).unwrap().len(), 0);
    }

    #[test]
    fn superuser_flag_is_persisted() {
        let conn = setup();
        create_user(&conn, "admin", "admin", true).unwrap();
        let user = authenticate(&conn, "admin", "admin").unwrap().unwrap();
        assert!(user.is_superuser);
    }
}
