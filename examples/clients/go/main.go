package main

import (
	"database/sql"
	"fmt"
	"log"
	"os"

	_ "github.com/lib/pq"
)

func main() {
	host := getEnv("NOVADB_HOST", "127.0.0.1")
	port := getEnv("NOVADB_PORT", "5432")
	user := getEnv("NOVADB_PG_USER", "admin")
	password := getEnv("NOVADB_PG_PASSWORD", "secret")
	dbname := getEnv("NOVADB_DB", "default")

	connStr := fmt.Sprintf("host=%s port=%s user=%s password=%s dbname=%s sslmode=disable",
		host, port, user, password, dbname)

	fmt.Printf("Connecting to NovaDB at %s:%s...\n", host, port)
	db, err := sql.Open("postgres", connStr)
	if err != nil {
		log.Fatalf("Failed to open connection: %v", err)
	}
	defer db.Close()

	if err := db.Ping(); err != nil {
		log.Fatalf("Failed to ping NovaDB: %v", err)
	}
	fmt.Println("Successfully connected to NovaDB!")

	// 1. Create table
	_, err = db.Exec(`
		CREATE TABLE IF NOT EXISTS tasks (
			id TEXT PRIMARY KEY,
			title TEXT NOT NULL,
			completed INTEGER DEFAULT 0,
			created_at TEXT NOT NULL
		);
	`)
	if err != nil {
		log.Fatalf("Failed to create table: %v", err)
	}

	// 2. Insert tasks
	_, err = db.Exec(`
		INSERT INTO tasks (id, title, completed, created_at)
		VALUES (uuid_v7(), 'Deploy NovaDB cluster', 1, now_iso());
	`)
	if err != nil {
		log.Fatalf("Failed to insert task: %v", err)
	}

	// 3. Query tasks
	rows, err := db.Query(`
		SELECT id, title, completed, created_at
		FROM tasks
		ORDER BY created_at DESC
		LIMIT 10;
	`)
	if err != nil {
		log.Fatalf("Failed to query tasks: %v", err)
	}
	defer rows.Close()

	fmt.Println("Tasks list:")
	for rows.Next() {
		var id, title, createdAt string
		var completed int
		if err := rows.Scan(&id, &title, &completed, &createdAt); err != nil {
			log.Fatalf("Failed to scan row: %v", err)
		}
		status := "[ ]"
		if completed == 1 {
			status = "[x]"
		}
		fmt.Printf("  %s %s: %s (at %s)\n", status, id, title, createdAt)
	}
}

func getEnv(key, fallback string) string {
	if val := os.Getenv(key); val != "" {
		return val
	}
	return fallback
}
