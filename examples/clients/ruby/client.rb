#!/usr/bin/env ruby
# ==============================================================================
# NovaDB Ruby Example
# Connects to NovaDB using standard pg gem
# ==============================================================================

require 'pg'
require 'json'

host = ENV['NOVADB_HOST'] || '127.0.0.1'
port = (ENV['NOVADB_PORT'] || 5432).to_i
user = ENV['NOVADB_PG_USER'] || 'admin'
pass = ENV['NOVADB_PG_PASSWORD'] || 'secret'
db   = ENV['NOVADB_DB'] || 'default'

puts "Connecting to NovaDB at #{host}:#{port}..."
conn = PG.connect(host: host, port: port, user: user, password: pass, dbname: db, sslmode: 'disable')

begin
  puts "Connected successfully to NovaDB!"

  # 1. Create table
  conn.exec(<<-SQL)
    CREATE TABLE IF NOT EXISTS articles (
      id TEXT PRIMARY KEY,
      title TEXT NOT NULL,
      author TEXT NOT NULL,
      views INTEGER DEFAULT 0,
      created_at TEXT NOT NULL
    );
  SQL
  puts "Table `articles` ready."

  # 2. Insert article
  conn.exec(<<-SQL)
    INSERT INTO articles (id, title, author, views, created_at)
    VALUES (uuid_v7(), 'Getting Started with NovaDB', 'Ruby Developer', 120, now_iso());
  SQL
  puts "Inserted article with UUID v7."

  # 3. Query articles
  res = conn.exec("SELECT id, title, author, views, created_at FROM articles ORDER BY created_at DESC LIMIT 5")
  puts "Articles list (#{res.ntuples} rows):"
  res.each do |row|
    puts "  [#{row['created_at']}] #{row['title']} by #{row['author']} (views: #{row['views']})"
  end

ensure
  conn.close if conn
  puts "Connection closed."
end
