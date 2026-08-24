// ==============================================================================
// NovaDB C# / .NET Example
// Connects to NovaDB using standard Npgsql driver
// ==============================================================================

using System;
using Npgsql;

var host = Environment.GetEnvironmentVariable("NOVADB_HOST") ?? "127.0.0.1";
var port = Environment.GetEnvironmentVariable("NOVADB_PORT") ?? "5432";
var user = Environment.GetEnvironmentVariable("NOVADB_PG_USER") ?? "admin";
var pass = Environment.GetEnvironmentVariable("NOVADB_PG_PASSWORD") ?? "secret";
var db = Environment.GetEnvironmentVariable("NOVADB_DB") ?? "default";

var connString = $"Host={host};Port={port};Username={user};Password={pass};Database={db};SSL Mode=Disable";

Console.WriteLine($"Connecting to NovaDB at {host}:{port}...");

await using var dataSource = NpgsqlDataSource.Create(connString);
await using var conn = await dataSource.OpenConnectionAsync();

Console.WriteLine("Connected successfully to NovaDB!");

// 1. Create table
await using (var cmd = new NpgsqlCommand(@"
    CREATE TABLE IF NOT EXISTS telemetry (
        id TEXT PRIMARY KEY,
        device_name TEXT NOT NULL,
        temperature REAL NOT NULL,
        humidity REAL NOT NULL,
        recorded_at TEXT NOT NULL
    );", conn))
{
    await cmd.ExecuteNonQueryAsync();
    Console.WriteLine("Table `telemetry` ready.");
}

// 2. Insert record using uuid_v7() and now_iso()
await using (var cmd = new NpgsqlCommand(@"
    INSERT INTO telemetry (id, device_name, temperature, humidity, recorded_at)
    VALUES (uuid_v7(), 'sensor-node-east', 22.4, 65.2, now_iso());", conn))
{
    await cmd.ExecuteNonQueryAsync();
    Console.WriteLine("Inserted telemetry data with UUID v7.");
}

// 3. Query records
await using (var cmd = new NpgsqlCommand("SELECT id, device_name, temperature, humidity, recorded_at FROM telemetry ORDER BY recorded_at DESC LIMIT 5;", conn))
await using (var reader = await cmd.ExecuteReaderAsync())
{
    Console.WriteLine("Telemetry records:");
    while (await reader.ReadAsync())
    {
        Console.WriteLine($"  [{reader.GetString(4)}] Device={reader.GetString(1)} Temp={reader.GetDouble(2)}C Humidity={reader.GetDouble(3)}%");
    }
}
