// ==============================================================================
// NovaDB Node.js / TypeScript Example
// Connects to NovaDB using standard PostgreSQL drivers (pg or postgres.js)
// ==============================================================================

import { Client } from 'pg';

async function main() {
    console.log('Connecting to NovaDB on PostgreSQL wire port (5432)...');

    const client = new Client({
        host: '127.0.0.1',
        port: 5432,
        database: 'default',
        user: process.env.NOVADB_PG_USER || 'admin',
        password: process.env.NOVADB_PG_PASSWORD || 'secret',
        ssl: false,
    });

    try {
        await client.connect();
        console.log('Connected to NovaDB!');

        // 1. Create table with modern types
        await client.query(`
            CREATE TABLE IF NOT EXISTS products (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                price REAL NOT NULL,
                tags JSON,
                created_at TEXT
            );
        `);
        console.log('Table `products` verified.');

        // 2. Insert records using NovaDB extended functions (uuid_v7, now_iso)
        await client.query(`
            INSERT INTO products (id, name, price, tags, created_at)
            VALUES (uuid_v7(), 'Quantum SSD 2TB', 149.99, json('["hardware","storage"]'), now_iso());
        `);
        console.log('Inserted sample product with UUID v7.');

        // 3. Query records with JSON and string functions
        const res = await client.query(`
            SELECT 
                id, 
                name, 
                price, 
                json_extract(tags, '$[0]') as primary_tag,
                char_length(name) as name_len,
                created_at
            FROM products 
            ORDER BY created_at DESC 
            LIMIT 5;
        `);

        console.log('Query results:');
        console.table(res.rows);

        // 4. Aggregations (STRING_AGG, JSON_AGG)
        const agg = await client.query(`
            SELECT 
                COUNT(*) as total_count,
                AVG(price) as avg_price,
                string_agg(name, ', ') as product_list
            FROM products;
        `);
        console.log('Aggregation summary:', agg.rows[0]);

    } catch (err) {
        console.error('Error executing query:', err.message);
    } finally {
        await client.end();
    }
}

main().catch(console.error);
