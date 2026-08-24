/*
 * NovaDB C / C++ Client Example
 * Connects to NovaDB using standard libpq library (PostgreSQL C API)
 * 
 * Compile:
 *   gcc -o client client.c -lpq
 */

#include <stdio.h>
#include <stdlib.h>
#include <libpq-fe.h>

int main(void) {
    const char *conninfo = "host=127.0.0.1 port=5432 user=admin password=secret dbname=default sslmode=disable";
    printf("Connecting to NovaDB using libpq...\n");

    PGconn *conn = PQconnectdb(conninfo);
    if (PQstatus(conn) != CONNECTION_OK) {
        fprintf(stderr, "Connection failed: %s\n", PQerrorMessage(conn));
        PQfinish(conn);
        return 1;
    }
    printf("Connected successfully to NovaDB!\n");

    /* 1. Create Table */
    PGresult *res = PQexec(conn,
        "CREATE TABLE IF NOT EXISTS sensors ("
        "  id TEXT PRIMARY KEY,"
        "  sensor_type TEXT NOT NULL,"
        "  value REAL NOT NULL,"
        "  created_at TEXT NOT NULL"
        ");"
    );
    if (PQresultStatus(res) != PGRES_COMMAND_OK) {
        fprintf(stderr, "CREATE TABLE failed: %s\n", PQerrorMessage(conn));
        PQclear(res);
        PQfinish(conn);
        return 1;
    }
    PQclear(res);
    printf("Table `sensors` verified.\n");

    /* 2. Insert record */
    res = PQexec(conn,
        "INSERT INTO sensors (id, sensor_type, value, created_at) "
        "VALUES (uuid_v7(), 'pressure', 101.325, now_iso());"
    );
    if (PQresultStatus(res) != PGRES_COMMAND_OK) {
        fprintf(stderr, "INSERT failed: %s\n", PQerrorMessage(conn));
        PQclear(res);
        PQfinish(conn);
        return 1;
    }
    PQclear(res);
    printf("Inserted sensor reading with UUID v7.\n");

    /* 3. Query records */
    res = PQexec(conn, "SELECT id, sensor_type, value, created_at FROM sensors ORDER BY created_at DESC LIMIT 5;");
    if (PQresultStatus(res) != PGRES_TUPLES_OK) {
        fprintf(stderr, "SELECT failed: %s\n", PQerrorMessage(conn));
        PQclear(res);
        PQfinish(conn);
        return 1;
    }

    int nrows = PQntuples(res);
    printf("Query returned %d row(s):\n", nrows);
    for (int i = 0; i < nrows; i++) {
        printf("  [%s] ID=%s Type=%s Value=%s\n",
            PQgetvalue(res, i, 3),
            PQgetvalue(res, i, 0),
            PQgetvalue(res, i, 1),
            PQgetvalue(res, i, 2)
        );
    }

    PQclear(res);
    PQfinish(conn);
    printf("Connection closed.\n");
    return 0;
}
