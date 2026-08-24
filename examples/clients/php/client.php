<?php
// ==============================================================================
// NovaDB PHP Client Example
// Connects to NovaDB using standard PDO PostgreSQL driver
// ==============================================================================

$host = getenv('NOVADB_HOST') ?: '127.0.0.1';
$port = getenv('NOVADB_PORT') ?: '5432';
$user = getenv('NOVADB_PG_USER') ?: 'admin';
$pass = getenv('NOVADB_PG_PASSWORD') ?: 'secret';
$db   = getenv('NOVADB_DB') ?: 'default';

$dsn = "pgsql:host={$host};port={$port};dbname={$db};sslmode=disable";

try {
    echo "Connecting to NovaDB at {$host}:{$port}...\n";
    $pdo = new PDO($dsn, $user, $pass, [
        PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION,
        PDO::ATTR_DEFAULT_FETCH_MODE => PDO::FETCH_ASSOC,
    ]);
    echo "Connected successfully to NovaDB!\n";

    // 1. Create table
    $pdo->exec("
        CREATE TABLE IF NOT EXISTS orders (
            id TEXT PRIMARY KEY,
            customer_email TEXT NOT NULL,
            amount REAL NOT NULL,
            metadata JSON,
            created_at TEXT NOT NULL
        );
    ");
    echo "Table `orders` verified.\n";

    // 2. Insert record using NovaDB uuid_v7() and now_iso()
    $stmt = $pdo->prepare("
        INSERT INTO orders (id, customer_email, amount, metadata, created_at)
        VALUES (uuid_v7(), :email, :amount, json(:meta), now_iso());
    ");
    $stmt->execute([
        ':email'  => 'user@example.com',
        ':amount' => 89.50,
        ':meta'   => json_encode(['source' => 'web_checkout', 'currency' => 'USD']),
    ]);
    echo "Inserted sample order with UUID v7.\n";

    // 3. Query records with string and date functions
    $stmt = $pdo->query("
        SELECT 
            id, 
            customer_email, 
            amount, 
            json_extract(metadata, '$.source') as order_source,
            created_at
        FROM orders 
        ORDER BY created_at DESC 
        LIMIT 5;
    ");
    $rows = $stmt->fetchAll();

    echo "Query results (" . count($rows) . " rows):\n";
    foreach ($rows as $row) {
        echo "  Order ID: {$row['id']} | Email: {$row['customer_email']} | Amount: \${$row['amount']} | Source: {$row['order_source']} | Date: {$row['created_at']}\n";
    }

    // 4. Aggregations
    $agg = $pdo->query("
        SELECT 
            COUNT(*) as total_orders, 
            AVG(amount) as avg_amount,
            string_agg(customer_email, ', ') as customer_list
        FROM orders;
    ")->fetch();

    echo "Summary: Total=" . $agg['total_orders'] . ", Avg=\$" . number_format((float)$agg['avg_amount'], 2) . "\n";

} catch (PDOException $e) {
    echo "Database Error: " . $e->getMessage() . "\n";
    exit(1);
}
