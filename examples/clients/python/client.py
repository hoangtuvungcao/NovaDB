#!/usr/bin/env python3
"""
NovaDB Python Example
Connects to NovaDB using standard PostgreSQL DB-API (psycopg2 or asyncpg)
"""

import os
import psycopg2
from psycopg2.extras import RealDictCursor

def main():
    host = os.getenv("NOVADB_HOST", "127.0.0.1")
    port = int(os.getenv("NOVADB_PORT", "5432"))
    user = os.getenv("NOVADB_PG_USER", "admin")
    password = os.getenv("NOVADB_PG_PASSWORD", "secret")
    dbname = os.getenv("NOVADB_DB", "default")

    print(f"Connecting to NovaDB at {host}:{port}/{dbname}...")
    conn = psycopg2.connect(
        host=host,
        port=port,
        user=user,
        password=password,
        dbname=dbname
    )
    conn.autocommit = True

    try:
        with conn.cursor(cursor_factory=RealDictCursor) as cur:
            # 1. Create table
            cur.execute("""
                CREATE TABLE IF NOT EXISTS server_metrics (
                    id TEXT PRIMARY KEY,
                    host TEXT NOT NULL,
                    cpu_percent REAL NOT NULL,
                    memory_mb INTEGER NOT NULL,
                    recorded_at TEXT NOT NULL
                );
            """)
            print("Table `server_metrics` ready.")

            # 2. Insert metrics with NovaDB built-in UUID v7 & ISO timestamps
            cur.execute("""
                INSERT INTO server_metrics (id, host, cpu_percent, memory_mb, recorded_at)
                VALUES (uuid_v7(), 'edge-node-01', 24.5, 4096, now_iso());
            """)
            print("Inserted metrics entry.")

            # 3. Query with string and date functions
            cur.execute("""
                SELECT 
                    id,
                    host,
                    cpu_percent,
                    memory_mb,
                    recorded_at,
                    date_part('hour', recorded_at) as hour_recorded
                FROM server_metrics
                ORDER BY recorded_at DESC
                LIMIT 5;
            """)
            rows = cur.fetchall()
            print("Recent metrics:")
            for row in rows:
                print(f"  [{row['recorded_at']}] {row['host']}: CPU={row['cpu_percent']}%, RAM={row['memory_mb']}MB")

            # 4. JSON aggregation test
            cur.execute("""
                SELECT json_object_agg(host, cpu_percent) as host_cpu_map
                FROM server_metrics;
            """)
            agg_result = cur.fetchone()
            print("Host CPU Map (JSON object):", agg_result['host_cpu_map'])

    finally:
        conn.close()
        print("Connection closed.")

if __name__ == "__main__":
    main()
