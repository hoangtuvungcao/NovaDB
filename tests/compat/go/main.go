// Go database/sql mini-app against NovaDB's MySQL gateway (go-sql-driver/mysql) and PostgreSQL gateway (pgx stdlib).
package main

import (
	"bytes"
	"database/sql"
	"encoding/json"
	"fmt"
	"net/http"
	"os"
	"regexp"
	"strings"
	"time"

	_ "github.com/go-sql-driver/mysql"
	_ "github.com/jackc/pgx/v5/stdlib"
)

var hostile = []string{"'); DROP TABLE users; --", "Robert'); DROP TABLE users;--", "\" OR \"1\"=\"1", "a\\'b\\\\c\\nd",
	"x' UNION SELECT 1,2,3 --", "%_ ? :name $1 /* c */ -- tail", "line1\nline2\r\n\ttab"}

const viet = "Nguyễn Văn Ạnh — Đặng Thị Hương 🙂 Tiếng Việt: ắằẳẵặ ếềểễệ ốồổỗộ ơờởỡợ ưừửữự"

var blob = func() []byte {
	b := make([]byte, 256)
	for i := range b {
		b[i] = byte(i)
	}
	return append(b, 0, 255, 0, '\'', '"', '\\', ';', '-', '-')
}()

func emit(suite, name, status, detail string) {
	detail = strings.ReplaceAll(strings.Join(strings.Fields(detail), " "), "|", "/")
	if len(detail) > 400 {
		detail = detail[:400]
	}
	fmt.Printf("RESULT|%s|%s|%s|%s\n", suite, name, status, detail)
}

func runCase(suite, name string, known map[string]string, fn func() error) bool {
	k, ok := known[name]
	if !ok {
		k, ok = known["*"]
	}
	err := fn()
	if err != nil {
		if ok {
			emit(suite, name, "KNOWN-FAIL", k+" || "+err.Error())
		} else {
			emit(suite, name, "FAIL", err.Error())
		}
		return false
	}
	if ok && known["*"] == "" {
		emit(suite, name, "PASS", "now passes; remove known marker")
	} else {
		emit(suite, name, "PASS", "")
	}
	return true
}

func mkdb(id string) {
	resp, err := http.Post("http://127.0.0.1:"+os.Getenv("NOVA_HTTP_PORT")+"/v1/admin/databases/"+id, "application/json", nil)
	if err != nil || resp.StatusCode >= 300 {
		panic(fmt.Sprintf("mkdb %s: %v", id, err))
	}
	resp.Body.Close()
}

var phRe = regexp.MustCompile(`\?`)

func app(suite, driver, dsn, kind string, known map[string]string) {
	C := func(n string, f func() error) bool { return runCase(suite, n, known, f) }
	pg := kind == "pg"
	var db *sql.DB
	rebind := func(q string) string {
		if !pg {
			return q
		}
		i := 0
		return phRe.ReplaceAllStringFunc(q, func(string) string { i++; return fmt.Sprintf("$%d", i) })
	}
	exec := func(q string, a ...any) (sql.Result, error) { return db.Exec(rebind(q), a...) }
	if !C("connect+select1", func() error {
		var err error
		db, err = sql.Open(driver, dsn)
		if err != nil {
			return err
		}
		db.SetMaxOpenConns(1)
		db.SetConnMaxLifetime(time.Minute)
		var one int
		if err = db.QueryRow("SELECT 1").Scan(&one); err != nil {
			return err
		}
		if one != 1 {
			return fmt.Errorf("got %d", one)
		}
		return nil
	}) {
		return
	}
	defer db.Close()
	pk, blobT := "INT AUTO_INCREMENT PRIMARY KEY", "BLOB"
	if pg {
		pk, blobT = "INTEGER PRIMARY KEY", "BYTEA"
	}
	if !C("ddl: 3 tables with FK/unique/default ts/index", func() error {
		for _, q := range []string{
			"CREATE TABLE users (id " + pk + ", email VARCHAR(255) NOT NULL UNIQUE, name VARCHAR(200) NOT NULL, profile TEXT, avatar " + blobT + ", created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP)",
			"CREATE TABLE posts (id " + pk + ", user_id INT NOT NULL REFERENCES users(id), title VARCHAR(300) NOT NULL, body TEXT, views INT NOT NULL DEFAULT 0)",
			"CREATE TABLE comments (id " + pk + ", post_id INT NOT NULL REFERENCES posts(id), user_id INT NOT NULL REFERENCES users(id), body TEXT NOT NULL)",
			"CREATE INDEX ix_posts_user ON posts(user_id)"} {
			if _, err := db.Exec(q); err != nil {
				return err
			}
		}
		return nil
	}) {
		return
	}
	if pg {
		C("SERIAL auto-increment", func() error {
			db.Exec("CREATE TABLE serial_probe (id SERIAL PRIMARY KEY, v TEXT)")
			exec("INSERT INTO serial_probe (v) VALUES (?)", "a")
			exec("INSERT INTO serial_probe (v) VALUES (?)", "b")
			var a, b sql.NullInt64
			rows, err := db.Query("SELECT id FROM serial_probe ORDER BY v")
			if err != nil {
				return err
			}
			defer rows.Close()
			rows.Next()
			rows.Scan(&a)
			rows.Next()
			rows.Scan(&b)
			if !a.Valid || !b.Valid || b.Int64 != a.Int64+1 {
				return fmt.Errorf("SERIAL ids: %v %v", a, b)
			}
			return nil
		})
	}
	var ia, ib int64
	C("insert + auto-increment id (select back)", func() error {
		if _, err := exec("INSERT INTO users (email, name, profile) VALUES (?, ?, ?)", "ann@example.com", "Ann", `{"k":[1,2]}`); err != nil {
			return err
		}
		if _, err := exec("INSERT INTO users (email, name) VALUES (?, ?)", "bob@example.com", "Bob"); err != nil {
			return err
		}
		var a, b sql.NullInt64
		rows, err := db.Query("SELECT id FROM users ORDER BY email")
		if err != nil {
			return err
		}
		rows.Next()
		rows.Scan(&a)
		rows.Next()
		rows.Scan(&b)
		rows.Close()
		if !a.Valid || !b.Valid || b.Int64 != a.Int64+1 {
			return fmt.Errorf("ids: %v %v", a, b)
		}
		ia, ib = a.Int64, b.Int64
		return nil
	})
	C("LastInsertId()", func() error {
		res, err := exec("INSERT INTO users (email, name) VALUES (?, ?)", "li@x", "LI")
		if err != nil {
			return err
		}
		defer exec("DELETE FROM users WHERE email = ?", "li@x")
		id, err := res.LastInsertId()
		if err != nil {
			return err
		}
		if id != ib+1 {
			return fmt.Errorf("LastInsertId=%d want %d", id, ib+1)
		}
		return nil
	})
	C("scan COUNT(*) into int64", func() error {
		var n int64
		return db.QueryRow("SELECT COUNT(*) FROM users").Scan(&n)
	})
	C("timestamp default (scan into string / time)", func() error {
		var s string
		if err := db.QueryRow(rebind("SELECT CAST(created_at AS CHAR) FROM users WHERE id = ?"), ia).Scan(&s); err != nil {
			return err
		}
		if !strings.HasPrefix(s, fmt.Sprint(time.Now().UTC().Year())) {
			return fmt.Errorf("created_at=%q", s)
		}
		return nil
	})
	C("unique violation -> driver error", func() error {
		_, err := exec("INSERT INTO users (email, name) VALUES (?, ?)", "ann@example.com", "dup")
		if err == nil {
			return fmt.Errorf("duplicate accepted")
		}
		m := err.Error()
		if !(strings.Contains(m, "1062") || strings.Contains(m, "23505")) {
			return fmt.Errorf("unexpected code: %v", err)
		}
		return nil
	})
	C("foreign key violation -> driver error", func() error {
		_, err := exec("INSERT INTO posts (user_id, title) VALUES (?, ?)", 99999, "orphan")
		if err == nil {
			return fmt.Errorf("FK violation accepted")
		}
		m := err.Error()
		if !(strings.Contains(m, "1452") || strings.Contains(m, "23503")) {
			return fmt.Errorf("unexpected code: %v", err)
		}
		return nil
	})
	C("transaction commit + rollback", func() error {
		tx, err := db.Begin()
		if err != nil {
			return err
		}
		tx.Exec(rebind("INSERT INTO posts (user_id, title) VALUES (?, ?)"), ia, "keep")
		if err = tx.Commit(); err != nil {
			return err
		}
		tx, _ = db.Begin()
		tx.Exec(rebind("INSERT INTO posts (user_id, title) VALUES (?, ?)"), ia, "gone")
		if err = tx.Rollback(); err != nil {
			return err
		}
		var n int
		db.QueryRow("SELECT COUNT(*) FROM posts").Scan(&n)
		if n != 1 {
			return fmt.Errorf("posts=%d want 1", n)
		}
		return nil
	})
	C("bulk insert (500 rows, prepared stmt in txn)", func() error {
		// First try an explicit prepared statement. If the gateway cannot do that, remember the error, fall back to
		// plain Exec so later cases still have data, and report the original failure at the end.
		var firstErr error
		insert := func(usePrepared bool) error {
			tx, err := db.Begin()
			if err != nil {
				return err
			}
			var st *sql.Stmt
			if usePrepared {
				if st, err = tx.Prepare(rebind("INSERT INTO posts (user_id, title, body, views) VALUES (?, ?, ?, ?)")); err != nil {
					tx.Rollback()
					return err
				}
			}
			for i := 0; i < 500; i++ {
				title, body := fmt.Sprintf("bulk-%04d", i), strings.Repeat("x", i%30)
				if usePrepared {
					_, err = st.Exec(ib, title, body, i)
				} else {
					_, err = tx.Exec(rebind("INSERT INTO posts (user_id, title, body, views) VALUES (?, ?, ?, ?)"), ib, title, body, i)
				}
				if err != nil {
					tx.Rollback()
					return err
				}
			}
			if st != nil {
				st.Close()
			}
			return tx.Commit()
		}
		if firstErr = insert(true); firstErr != nil {
			if err := insert(false); err != nil {
				return fmt.Errorf("prepared: %v; fallback exec: %v", firstErr, err)
			}
		}
		var n, s int64
		if err := db.QueryRow(rebind("SELECT COUNT(*), SUM(views) FROM posts WHERE title LIKE ?"), "bulk-%").Scan(&n, &s); err != nil {
			return err
		}
		if n != 500 || s != 124750 {
			return fmt.Errorf("n=%d s=%d", n, s)
		}
		return firstErr
	})
	C("pagination LIMIT/OFFSET with params", func() error {
		rows, err := db.Query(rebind("SELECT title FROM posts WHERE title LIKE ? ORDER BY title LIMIT ? OFFSET ?"), "bulk-%", 10, 20)
		if err != nil {
			return err
		}
		defer rows.Close()
		var t []string
		for rows.Next() {
			var s string
			rows.Scan(&s)
			t = append(t, s)
		}
		if len(t) != 10 || t[0] != "bulk-0020" || t[9] != "bulk-0029" {
			return fmt.Errorf("%v", t)
		}
		return nil
	})
	C("join + group by/having + aggregates", func() error {
		if _, err := exec("INSERT INTO comments (post_id, user_id, body) SELECT id, ?, ? FROM posts WHERE title = ?", ib, "hi", "keep"); err != nil {
			return err
		}
		rows, err := db.Query("SELECT u.name, COUNT(p.id) FROM users u LEFT JOIN posts p ON p.user_id = u.id GROUP BY u.id, u.name HAVING COUNT(p.id) >= 1 ORDER BY u.id")
		if err != nil {
			return err
		}
		defer rows.Close()
		var out []string
		for rows.Next() {
			var n string
			var c int64
			if err = rows.Scan(&n, &c); err != nil {
				return err
			}
			out = append(out, fmt.Sprintf("%s:%d", n, c))
		}
		if strings.Join(out, ",") != "Ann:1,Bob:500" {
			return fmt.Errorf("%v", out)
		}
		return nil
	})
	C("hostile strings as bound params", func() error {
		for i, h := range hostile {
			if _, err := exec("INSERT INTO users (email, name, profile) VALUES (?, ?, ?)", fmt.Sprintf("h%d@x", i), h, h); err != nil {
				return err
			}
		}
		rows, err := db.Query("SELECT name FROM users WHERE email LIKE 'h%@x' ORDER BY id")
		if err != nil {
			return err
		}
		defer rows.Close()
		var got []string
		for rows.Next() {
			var s string
			rows.Scan(&s)
			got = append(got, s)
		}
		if len(got) != len(hostile) {
			return fmt.Errorf("got %d rows", len(got))
		}
		for i := range got {
			if got[i] != hostile[i] {
				return fmt.Errorf("row %d: %q != %q", i, got[i], hostile[i])
			}
		}
		var n int
		db.QueryRow("SELECT COUNT(*) FROM users").Scan(&n)
		if n != 2+len(hostile) {
			return fmt.Errorf("users table not intact: %d", n)
		}
		return nil
	})
	C("unicode Vietnamese + emoji", func() error {
		j, _ := json.Marshal(map[string]string{"v": viet})
		if _, err := exec("INSERT INTO users (email, name, profile) VALUES (?, ?, ?)", "v@x", viet, string(j)); err != nil {
			return err
		}
		var s string
		if err := db.QueryRow(rebind("SELECT name FROM users WHERE name = ?"), viet).Scan(&s); err != nil {
			return err
		}
		if s != viet {
			return fmt.Errorf("mismatch %q", s)
		}
		return nil
	})
	C("BLOB roundtrip", func() error {
		if _, err := exec("UPDATE users SET avatar = ? WHERE id = ?", blob, ib); err != nil {
			return err
		}
		var b []byte
		if err := db.QueryRow(rebind("SELECT avatar FROM users WHERE id = ?"), ib).Scan(&b); err != nil {
			return err
		}
		if !bytes.Equal(b, blob) {
			return fmt.Errorf("blob mismatch len=%d want %d", len(b), len(blob))
		}
		return nil
	})
	C("RowsAffected() on update/delete", func() error {
		r, err := exec("UPDATE posts SET views = views + 1 WHERE title LIKE ?", "bulk-%")
		if err != nil {
			return err
		}
		if n, _ := r.RowsAffected(); n != 500 {
			return fmt.Errorf("update affected %d", n)
		}
		r, err = exec("DELETE FROM posts WHERE title LIKE ?", "bulk-%")
		if err != nil {
			return err
		}
		if n, _ := r.RowsAffected(); n != 500 {
			return fmt.Errorf("delete affected %d", n)
		}
		return nil
	})
}

func main() {
	user, pass := os.Getenv("NOVA_USER"), os.Getenv("NOVA_PASS")
	myp, pgp := os.Getenv("NOVA_MYSQL_PORT"), os.Getenv("NOVA_PG_PORT")
	for _, d := range []string{"go_mysql", "go_pg_ext", "go_pg_simple"} {
		mkdb(d)
	}
	app("go-drivers:mysql(go-sql-driver)", "mysql", fmt.Sprintf("%s:%s@tcp(127.0.0.1:%s)/go_mysql?charset=utf8mb4&parseTime=false&interpolateParams=false", user, pass, myp), "mysql", map[string]string{})
	pgKnown := map[string]string{
		"LastInsertId()": "PGX-1: pgx/stdlib does not implement LastInsertId (real PostgreSQL behaves the same); use INSERT ... RETURNING",
	}
	app("go-drivers:postgresql(pgx,extended)", "pgx", fmt.Sprintf("postgres://%s:%s@127.0.0.1:%s/go_pg_ext?sslmode=disable", user, pass, pgp), "pg",
		pgKnown)
	app("go-drivers:postgresql(pgx,simple_protocol)", "pgx", fmt.Sprintf("postgres://%s:%s@127.0.0.1:%s/go_pg_simple?sslmode=disable&default_query_exec_mode=simple_protocol", user, pass, pgp), "pg", pgKnown)
}
