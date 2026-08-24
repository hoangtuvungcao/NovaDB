//! Authentication and authorization module for NovaDB.
//!
//! Provides user management, role-based access control, and password hashing.

pub mod users;
pub mod roles;

use rusqlite::Connection;

/// Initialize the auth schema tables if they don't exist.
pub fn bootstrap_auth_schema(connection: &Connection) -> crate::Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS _novadb_users (
            username TEXT PRIMARY KEY NOT NULL,
            password_hash TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL DEFAULT (strftime('%s','now') * 1000),
            is_active INTEGER NOT NULL DEFAULT 1,
            is_superuser INTEGER NOT NULL DEFAULT 0,
            failed_logins INTEGER NOT NULL DEFAULT 0,
            last_login_ms INTEGER
        );
        CREATE TABLE IF NOT EXISTS _novadb_roles (
            role_name TEXT PRIMARY KEY NOT NULL,
            description TEXT,
            created_at_ms INTEGER NOT NULL DEFAULT (strftime('%s','now') * 1000)
        );
        CREATE TABLE IF NOT EXISTS _novadb_user_roles (
            username TEXT NOT NULL REFERENCES _novadb_users(username) ON DELETE CASCADE,
            role_name TEXT NOT NULL REFERENCES _novadb_roles(role_name) ON DELETE CASCADE,
            granted_at_ms INTEGER NOT NULL DEFAULT (strftime('%s','now') * 1000),
            PRIMARY KEY (username, role_name)
        );
        CREATE TABLE IF NOT EXISTS _novadb_grants (
            role_name TEXT NOT NULL REFERENCES _novadb_roles(role_name) ON DELETE CASCADE,
            table_name TEXT NOT NULL,
            privilege TEXT NOT NULL CHECK (privilege IN ('SELECT','INSERT','UPDATE','DELETE','ALL')),
            granted_at_ms INTEGER NOT NULL DEFAULT (strftime('%s','now') * 1000),
            PRIMARY KEY (role_name, table_name, privilege)
        );
        -- Built-in roles
        INSERT OR IGNORE INTO _novadb_roles (role_name, description)
        VALUES ('novadb_admin', 'Full administrative access'),
               ('novadb_readonly', 'Read-only access to all tables'),
               ('novadb_readwrite', 'Read and write access to all tables');
        ",
    )?;
    Ok(())
}
