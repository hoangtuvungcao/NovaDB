// ==============================================================================
// NovaDB Java / Kotlin Example
// Connects to NovaDB using standard PostgreSQL JDBC driver (org.postgresql.Driver)
// ==============================================================================

import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.Statement;

public class NovaDbDemo {
    public static void main(String[] args) {
        String host = System.getenv().getOrDefault("NOVADB_HOST", "127.0.0.1");
        String port = System.getenv().getOrDefault("NOVADB_PORT", "5432");
        String user = System.getenv().getOrDefault("NOVADB_PG_USER", "admin");
        String pass = System.getenv().getOrDefault("NOVADB_PG_PASSWORD", "secret");
        String db   = System.getenv().getOrDefault("NOVADB_DB", "default");

        String url = String.format("jdbc:postgresql://%s:%s/%s?sslmode=disable", host, port, db);
        System.out.println("Connecting to NovaDB at " + url + "...");

        try (Connection conn = DriverManager.getConnection(url, user, pass)) {
            System.out.println("Connected successfully to NovaDB!");

            // 1. Create table
            try (Statement stmt = conn.createStatement()) {
                stmt.execute(
                    "CREATE TABLE IF NOT EXISTS accounts (" +
                    "  id TEXT PRIMARY KEY," +
                    "  username TEXT NOT NULL," +
                    "  balance REAL NOT NULL," +
                    "  created_at TEXT NOT NULL" +
                    ")"
                );
                System.out.println("Table `accounts` verified.");

                // 2. Insert record
                stmt.execute(
                    "INSERT INTO accounts (id, username, balance, created_at) " +
                    "VALUES (uuid_v7(), 'developer_01', 5000.0, now_iso())"
                );
                System.out.println("Inserted account record with UUID v7.");

                // 3. Query records
                try (ResultSet rs = stmt.executeQuery("SELECT id, username, balance, created_at FROM accounts ORDER BY created_at DESC LIMIT 5")) {
                    System.out.println("Query results:");
                    while (rs.next()) {
                        System.out.printf("  ID: %s | User: %s | Balance: $%.2f | Date: %s%n",
                            rs.getString("id"),
                            rs.getString("username"),
                            rs.getDouble("balance"),
                            rs.getString("created_at")
                        );
                    }
                }
            }
        } catch (Exception e) {
            System.err.println("Database Error: " + e.getMessage());
            e.printStackTrace();
        }
    }
}
