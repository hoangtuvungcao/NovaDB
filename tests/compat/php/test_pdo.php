<?php
// PDO (pdo_mysql, pdo_pgsql) mini-app. Emits RESULT|suite|case|STATUS|detail lines.
mb_internal_encoding('UTF-8');

function emit(string $suite, string $case, string $status, string $detail = ''): void {
    $detail = str_replace('|', '/', preg_replace('/\s+/', ' ', $detail));
    echo "RESULT|$suite|$case|$status|" . substr($detail, 0, 400) . "\n";
}
function runCase(string $suite, string $name, callable $fn, array $known): bool {
    $k = $known[$name] ?? ($known['*'] ?? null);
    try { $fn(); }
    catch (Throwable $e) {
        $m = get_class($e) . ': ' . $e->getMessage();
        emit($suite, $name, $k ? 'KNOWN-FAIL' : 'FAIL', ($k ? "$k || " : '') . $m);
        return false;
    }
    emit($suite, $name, 'PASS', ($k && !isset($known['*']) ) ? 'now passes; remove known marker' : '');
    return true;
}
function check($cond, string $msg = 'assertion failed'): void { if (!$cond) throw new RuntimeException($msg); }
function mkdb(string $id): void {
    $ctx = stream_context_create(['http' => ['method' => 'POST', 'timeout' => 10]]);
    $r = @file_get_contents("http://127.0.0.1:" . getenv('NOVA_HTTP_PORT') . "/v1/admin/databases/$id", false, $ctx);
    if ($r === false) throw new RuntimeException("mkdb $id failed");
}
$HOSTILE = ["'); DROP TABLE users; --", "Robert'); DROP TABLE users;--", "\" OR \"1\"=\"1", "a\\'b\\\\c\\nd",
            "x' UNION SELECT 1,2,3 --", "%_ ? :name \$1 /* c */ -- tail", "line1\nline2\r\n\ttab"];
$VIET = 'Nguyễn Văn Ạnh — Đặng Thị Hương 🙂 Tiếng Việt: ắằẳẵặ ếềểễệ ốồổỗộ ơờởỡợ ưừửữự';
$BLOB = implode('', array_map('chr', range(0, 255))) . "\x00\xff\x00'\"\;--";

function app(string $suite, callable $connect, string $kind, array $known): void {
    global $HOSTILE, $VIET, $BLOB;
    $C = fn($n, $f) => runCase($suite, $n, $f, $known);
    $mysql = $kind === 'mysql';
    $pdo = null;
    if (!$C('connect+select1', function () use (&$pdo, $connect) { $pdo = $connect(); check($pdo->query('SELECT 1')->fetchColumn() == 1); })) return;
    $pk = $mysql ? 'INT AUTO_INCREMENT PRIMARY KEY' : 'INTEGER PRIMARY KEY';
    $blob = $mysql ? 'BLOB' : 'BYTEA';
    if (!$C('ddl: 3 tables with FK/unique/default ts/index', function () use ($pdo, $pk, $blob) {
        $pdo->exec("CREATE TABLE users (id $pk, email VARCHAR(255) NOT NULL UNIQUE, name VARCHAR(200) NOT NULL, profile TEXT, avatar $blob, created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP)");
        $pdo->exec("CREATE TABLE posts (id $pk, user_id INT NOT NULL REFERENCES users(id), title VARCHAR(300) NOT NULL, body TEXT, views INT NOT NULL DEFAULT 0)");
        $pdo->exec("CREATE TABLE comments (id $pk, post_id INT NOT NULL REFERENCES posts(id), user_id INT NOT NULL REFERENCES users(id), body TEXT NOT NULL)");
        $pdo->exec("CREATE INDEX ix_posts_user ON posts(user_id)");
    }, $known)) return;
    if (!$mysql) $C('SERIAL auto-increment', function () use ($pdo) {
        $pdo->exec("CREATE TABLE serial_probe (id SERIAL PRIMARY KEY, v TEXT)");
        $pdo->exec("INSERT INTO serial_probe (v) VALUES ('a')"); $pdo->exec("INSERT INTO serial_probe (v) VALUES ('b')");
        $r = $pdo->query("SELECT id FROM serial_probe ORDER BY v")->fetchAll(PDO::FETCH_COLUMN);
        check($r[0] !== null && (int)$r[1] === (int)$r[0] + 1, 'SERIAL ids: ' . json_encode($r));
    });
    $ids = [];
    $C('insert + auto-increment id (select back)', function () use ($pdo, &$ids) {
        $st = $pdo->prepare('INSERT INTO users (email, name, profile) VALUES (?, ?, ?)');
        $st->execute(['ann@example.com', 'Ann', json_encode(['k' => [1, 2]])]); $st->execute(['bob@example.com', 'Bob', null]);
        $r = $pdo->query('SELECT id FROM users ORDER BY email')->fetchAll(PDO::FETCH_COLUMN);
        check($r[0] !== null && $r[1] !== null, 'ids NULL: ' . json_encode($r));
        check((int)$r[1] === (int)$r[0] + 1, 'not sequential'); $ids['a'] = (int)$r[0]; $ids['b'] = (int)$r[1];
    });
    $C('lastInsertId()', function () use ($pdo, &$ids, $mysql) {
        $pdo->prepare('INSERT INTO users (email, name) VALUES (?, ?)')->execute(['li@x', 'LI']);
        try {
            $id = $pdo->lastInsertId($mysql ? null : 'users_id_seq');
            check((int)$id === $ids['b'] + 1, "lastInsertId=" . var_export($id, true));
        } finally { $pdo->exec("DELETE FROM users WHERE email='li@x'"); }
    });
    $C('result types (COUNT(*) is int via native types)', function () use ($pdo, $mysql) {
        $v = $pdo->query('SELECT COUNT(*) FROM users')->fetchColumn();
        check(is_int($v) || (!$mysql && false), 'COUNT(*) type ' . gettype($v) . ' ' . var_export($v, true));
    });
    $C('timestamp default', function () use ($pdo, $ids) {
        $st = $pdo->prepare('SELECT created_at FROM users WHERE id = ?'); $st->execute([$ids['a']]);
        $t = strtotime((string)$st->fetchColumn()); check($t && abs(time() - $t) < 86400 * 2, 'bad created_at');
    });
    $C('unique violation -> PDOException (SQLSTATE 23000/23505)', function () use ($pdo, $mysql) {
        try { $pdo->prepare('INSERT INTO users (email, name) VALUES (?, ?)')->execute(['ann@example.com', 'dup']); }
        catch (PDOException $e) { $s = $e->errorInfo[0] ?? $e->getCode(); check(in_array((string)$s, $mysql ? ['23000'] : ['23505'], true), "SQLSTATE $s: " . $e->getMessage()); return; }
        throw new RuntimeException('duplicate accepted');
    });
    $C('foreign key violation', function () use ($pdo, $mysql) {
        try { $pdo->prepare('INSERT INTO posts (user_id, title) VALUES (?, ?)')->execute([99999, 'orphan']); }
        catch (PDOException $e) { $s = (string)($e->errorInfo[0] ?? $e->getCode()); check(in_array($s, $mysql ? ['23000'] : ['23503'], true), "SQLSTATE $s: " . $e->getMessage()); return; }
        throw new RuntimeException('FK violation accepted');
    });
    $C('transaction commit + rollback', function () use ($pdo, $ids) {
        $pdo->beginTransaction(); $pdo->prepare('INSERT INTO posts (user_id, title) VALUES (?, ?)')->execute([$ids['a'], 'keep']); $pdo->commit();
        $pdo->beginTransaction(); $pdo->prepare('INSERT INTO posts (user_id, title) VALUES (?, ?)')->execute([$ids['a'], 'gone']); $pdo->rollBack();
        check($pdo->query('SELECT title FROM posts')->fetchAll(PDO::FETCH_COLUMN) === ['keep'], 'rows after rollback');
        check(!$pdo->inTransaction(), 'still in transaction');
    });
    $C('bulk insert (500 rows in one txn)', function () use ($pdo, $ids) {
        $pdo->beginTransaction(); $st = $pdo->prepare('INSERT INTO posts (user_id, title, body, views) VALUES (?, ?, ?, ?)');
        for ($i = 0; $i < 500; $i++) $st->execute([$ids['b'], sprintf('bulk-%04d', $i), str_repeat('x', $i % 30), $i]);
        $pdo->commit();
        $st = $pdo->prepare("SELECT COUNT(*), SUM(views) FROM posts WHERE title LIKE ?"); $st->execute(['bulk-%']); $r = $st->fetch(PDO::FETCH_NUM);
        check((int)$r[0] === 500 && (int)$r[1] === 124750, json_encode($r));
    });
    $C('pagination LIMIT/OFFSET with bound ints', function () use ($pdo) {
        $st = $pdo->prepare('SELECT title FROM posts WHERE title LIKE :p ORDER BY title LIMIT :l OFFSET :o');
        $st->bindValue(':p', 'bulk-%'); $st->bindValue(':l', 10, PDO::PARAM_INT); $st->bindValue(':o', 20, PDO::PARAM_INT); $st->execute();
        $r = $st->fetchAll(PDO::FETCH_COLUMN); check(count($r) === 10 && $r[0] === 'bulk-0020' && $r[9] === 'bulk-0029', json_encode($r));
    });
    $C('join + group by/having + aggregates', function () use ($pdo, $ids) {
        $pdo->prepare('INSERT INTO comments (post_id, user_id, body) SELECT id, ?, ? FROM posts WHERE title = ?')->execute([$ids['b'], 'hi', 'keep']);
        $r = $pdo->query('SELECT u.name, COUNT(p.id) n FROM users u LEFT JOIN posts p ON p.user_id = u.id GROUP BY u.id, u.name HAVING COUNT(p.id) >= 1 ORDER BY u.id')->fetchAll(PDO::FETCH_NUM);
        check($r[0][0] === 'Ann' && $r[1][0] === 'Bob' && (int)$r[1][1] === 500, json_encode($r));
        $j = $pdo->query('SELECT p.title, c.body, u.name FROM comments c JOIN posts p ON p.id = c.post_id JOIN users u ON u.id = c.user_id')->fetchAll(PDO::FETCH_NUM);
        check($j === [['keep', 'hi', 'Bob']], json_encode($j));
    });
    $C('hostile strings as bound params (named + positional)', function () use ($pdo, $HOSTILE) {
        $st = $pdo->prepare('INSERT INTO users (email, name, profile) VALUES (:e, :n, :p)');
        foreach ($HOSTILE as $i => $h) $st->execute([':e' => "h$i@x", ':n' => $h, ':p' => $h]);
        $got = $pdo->query("SELECT name FROM users WHERE email LIKE 'h%@x' ORDER BY id")->fetchAll(PDO::FETCH_COLUMN);
        check($got === $HOSTILE, json_encode($got));
        $st = $pdo->prepare('SELECT COUNT(*) FROM users WHERE name = ?'); $st->execute([$HOSTILE[0]]); check((int)$st->fetchColumn() === 1, 'count');
        check((int)$pdo->query('SELECT COUNT(*) FROM users')->fetchColumn() === 2 + count($HOSTILE), 'users table not intact');
    });
    $C('unicode Vietnamese + emoji', function () use ($pdo, $VIET) {
        $pdo->prepare('INSERT INTO users (email, name) VALUES (?, ?)')->execute(['v@x', $VIET]);
        $st = $pdo->prepare('SELECT name FROM users WHERE name = ?'); $st->execute([$VIET]); check($st->fetchColumn() === $VIET, 'mismatch');
    });
    $C('BLOB roundtrip (PARAM_LOB)', function () use ($pdo, $ids, $BLOB, $mysql) {
        $st = $pdo->prepare('UPDATE users SET avatar = ? WHERE id = ?'); $st->bindValue(1, $BLOB, PDO::PARAM_LOB); $st->bindValue(2, $ids['b'], PDO::PARAM_INT); $st->execute();
        $st = $pdo->prepare('SELECT avatar FROM users WHERE id = ?'); $st->execute([$ids['b']]); $v = $st->fetchColumn();
        if (is_resource($v)) $v = stream_get_contents($v);
        check($v === $BLOB, 'blob mismatch len=' . (is_string($v) ? strlen($v) : gettype($v)));
    });
    $C('rowCount() on update/delete', function () use ($pdo) {
        $st = $pdo->prepare('UPDATE posts SET views = views + 1 WHERE title LIKE ?'); $st->execute(['bulk-%']); check($st->rowCount() === 500, 'update rowCount ' . $st->rowCount());
        $st = $pdo->prepare('DELETE FROM posts WHERE title LIKE ?'); $st->execute(['bulk-%']); check($st->rowCount() === 500, 'delete rowCount ' . $st->rowCount());
    });
}

$user = getenv('NOVA_USER'); $pass = getenv('NOVA_PASS'); $myp = getenv('NOVA_MYSQL_PORT'); $pgp = getenv('NOVA_PG_PORT');
foreach (['pdo_my_emul', 'pdo_my_native', 'pdo_pg', 'pdo_pg2'] as $d) mkdb($d);
$mk = fn($emul, $db) => fn() => new PDO("mysql:host=127.0.0.1;port=$myp;dbname=$db;charset=utf8mb4", $user, $pass,
    [PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION, PDO::ATTR_EMULATE_PREPARES => $emul, PDO::MYSQL_ATTR_SSL_VERIFY_SERVER_CERT => false]);
app('php-pdo:mysql(emulated-prepares)', $mk(true, 'pdo_my_emul'), 'mysql', []);
app('php-pdo:mysql(native-prepares)', $mk(false, 'pdo_my_native'), 'mysql', []);
$pgKnown = []; // PG-1..PG-8 fixed
$pgOpts = [PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION];
app('php-pdo:postgresql(native-prepares)', fn() => new PDO("pgsql:host=127.0.0.1;port=$pgp;dbname=pdo_pg;sslmode=disable", $user, $pass, $pgOpts), 'pg',
    []);
app('php-pdo:postgresql(emulated-prepares)', fn() => new PDO("pgsql:host=127.0.0.1;port=$pgp;dbname=pdo_pg2;sslmode=disable", $user, $pass,
    $pgOpts + [PDO::ATTR_EMULATE_PREPARES => true]), 'pg', $pgKnown);
