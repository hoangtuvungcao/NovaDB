# NovaDB: Multi-Language Client Integration Guide

Because NovaDB exposes the standard **PostgreSQL Wire Protocol v3** on port `5432` and an **HTTP REST API** on port `8787`, you can use official PostgreSQL drivers and HTTP clients across all modern programming languages.

---

## 1. PHP (PDO_PGSQL)

Standard PHP applications (Laravel, Symfony, WordPress, Slim) connect directly via PDO:

```php
<?php
$host = '127.0.0.1';
$port = '5432';
$dbname = 'default';
$user = 'admin';
$password = 'secret';

$dsn = "pgsql:host=$host;port=$port;dbname=$dbname;sslmode=disable";

try {
    $pdo = new PDO($dsn, $user, $password, [
        PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION,
        PDO::ATTR_DEFAULT_FETCH_MODE => PDO::FETCH_ASSOC,
    ]);

    // Execute query with NovaDB extended functions
    $stmt = $pdo->query("SELECT uuid_v7() as id, now_iso() as created_at, 'NovaDB' as name");
    $row = $stmt->fetch();
    echo "Generated ID: " . $row['id'] . "\n";
    echo "Timestamp: " . $row['created_at'] . "\n";

} catch (PDOException $e) {
    echo "Connection failed: " . $e->getMessage() . "\n";
}
```

---

## 2. Rust (`tokio-postgres`)

High-performance asynchronous Rust client:

```rust
use tokio_postgres::{NoTls, Error};

#[tokio::main]
async fn main() -> Result<(), Error> {
    let (client, connection) = tokio_postgres::connect(
        "host=127.0.0.1 port=5432 user=admin password=secret dbname=default",
        NoTls,
    ).await?;

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {}", e);
        }
    });

    let row = client
        .query_one("SELECT uuid_v7()::text, now_iso()::text", &[])
        .await?;

    let id: &str = row.get(0);
    let time: &str = row.get(1);
    println!("ID: {}, Time: {}", id, time);

    Ok(())
}
```

---

## 3. Node.js & TypeScript (`pg`)

Node.js, Express, NestJS, and Next.js applications:

```typescript
import { Client } from 'pg';

const client = new Client({
  host: '127.0.0.1',
  port: 5432,
  database: 'default',
  user: 'admin',
  password: 'secret',
  ssl: false,
});

async function main() {
  await client.connect();
  const res = await client.query('SELECT uuid_v7() as id, now_iso() as created_at');
  console.log('Row:', res.rows[0]);
  await client.end();
}

main().catch(console.error);
```

---

## 4. Python (`psycopg2` / `psycopg3`)

Django, FastAPI, Flask, and SQLAlchemy applications:

```python
import psycopg2

conn = psycopg2.connect(
    host="127.0.0.1",
    port=5432,
    dbname="default",
    user="admin",
    password="secret",
    sslmode="disable"
)

with conn.cursor() as cur:
    cur.execute("SELECT uuid_v7(), now_iso(), vector_cosine_distance('[1.0, 0.0]', '[0.0, 1.0]')")
    row = cur.fetchone()
    print(f"UUID: {row[0]}, Time: {row[1]}, Distance: {row[2]}")

conn.close()
```

---

## 5. Go (`database/sql` + `github.com/lib/pq`)

```go
package main

import (
	"database/sql"
	"fmt"
	"log"

	_ "github.com/lib/pq"
)

func main() {
	connStr := "host=127.0.0.1 port=5432 user=admin password=secret dbname=default sslmode=disable"
	db, err := sql.Open("postgres", connStr)
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()

	var id, now string
	var dist float64
	err = db.QueryRow("SELECT uuid_v7(), now_iso(), vector_cosine_distance('[1.0, 0.0]', '[0.0, 1.0]')").Scan(&id, &now, &dist)
	if err != nil {
		log.Fatal(err)
	}

	fmt.Printf("UUID v7: %s\nTime: %s\nDistance: %f\n", id, now, dist)
}
```

---

## 6. Java & Kotlin (JDBC)

Spring Boot, Quarkus, and Micronaut:

```java
import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.ResultSet;
import java.sql.Statement;

public class NovaDbDemo {
    public static void main(String[] args) {
        String url = "jdbc:postgresql://127.0.0.1:5432/default?sslmode=disable";
        try (Connection conn = DriverManager.getConnection(url, "admin", "secret");
             Statement stmt = conn.createStatement();
             ResultSet rs = stmt.executeQuery("SELECT uuid_v7() as id, now_iso() as time")) {
            if (rs.next()) {
                System.out.println("ID: " + rs.getString("id"));
                System.out.println("Time: " + rs.getString("time"));
            }
        } catch (Exception e) {
            e.printStackTrace();
        }
    }
}
```

---

## 7. C# / .NET (`Npgsql`)

ASP.NET Core, Entity Framework Core:

```csharp
using Npgsql;

await using var conn = new NpgsqlConnection(
    "Host=127.0.0.1;Port=5432;Username=admin;Password=secret;Database=default;SSL Mode=Disable"
);
await conn.OpenAsync();

await using var cmd = new NpgsqlCommand("SELECT uuid_v7(), now_iso()", conn);
await using var reader = await cmd.ExecuteReaderAsync();

if (await reader.ReadAsync())
{
    Console.WriteLine($"ID: {reader.GetString(0)}");
    Console.WriteLine($"Time: {reader.GetString(1)}");
}
```

---

## 8. Ruby (`pg`)

Ruby on Rails and Sinatra:

```ruby
require 'pg'

conn = PG.connect(
  host: '127.0.0.1',
  port: 5432,
  user: 'admin',
  password: 'secret',
  dbname: 'default',
  sslmode: 'disable'
)

res = conn.exec("SELECT uuid_v7(), now_iso()")
res.each do |row|
  puts "ID: #{row['uuid_v7']}, Time: #{row['now_iso']}"
end
conn.close
```

---

## 9. C / C++ (`libpq`)

```c
#include <stdio.h>
#include <libpq-fe.h>

int main() {
    PGconn *conn = PQconnectdb("host=127.0.0.1 port=5432 user=admin password=secret dbname=default sslmode=disable");
    if (PQstatus(conn) != CONNECTION_OK) {
        fprintf(stderr, "Connection failed: %s\n", PQerrorMessage(conn));
        PQfinish(conn);
        return 1;
    }

    PGresult *res = PQexec(conn, "SELECT uuid_v7(), now_iso()");
    if (PQresultStatus(res) == PGRES_TUPLES_OK) {
        printf("ID: %s, Time: %s\n", PQgetvalue(res, 0, 0), PQgetvalue(res, 0, 1));
    }
    PQclear(res);
    PQfinish(conn);
    return 0;
}
```

---

## 10. cURL & HTTP REST API

```bash
# Execute SQL query via REST
curl -X POST http://127.0.0.1:8787/v1/admin/databases/default/query \
  -H "Authorization: Bearer dev-secret" \
  -H "Content-Type: application/json" \
  -d '{"sql": "SELECT uuid_v7() as id, now_iso() as created_at"}'
```
