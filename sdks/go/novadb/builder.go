package novadb

import (
	"context"
	"fmt"
	"reflect"
	"sort"
	"strings"
)

// Values is a column -> value map used by Where, Insert and Update.
// Columns are processed in sorted order so the generated SQL is deterministic.
type Values map[string]any

type condition struct {
	sql  string
	args []any
}

func sortedKeys(m map[string]any) []string {
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	return keys
}

func compileWhere(where Values) ([]condition, error) {
	var out []condition
	for _, col := range sortedKeys(where) {
		id, err := QuoteIdentifier(col)
		if err != nil {
			return nil, err
		}
		v := where[col]
		if v == nil {
			out = append(out, condition{sql: id + " IS NULL"})
			continue
		}
		rv := reflect.ValueOf(v)
		if (rv.Kind() == reflect.Slice || rv.Kind() == reflect.Array) && rv.Type().Elem().Kind() != reflect.Uint8 {
			if rv.Len() == 0 {
				out = append(out, condition{sql: "1 = 0"})
				continue
			}
			args := make([]any, rv.Len())
			marks := make([]string, rv.Len())
			for i := range args {
				args[i] = rv.Index(i).Interface()
				marks[i] = "?"
			}
			out = append(out, condition{sql: id + " IN (" + strings.Join(marks, ", ") + ")", args: args})
			continue
		}
		out = append(out, condition{sql: id + " = ?", args: []any{v}})
	}
	return out, nil
}

func whereClause(conds []condition) (string, []any) {
	if len(conds) == 0 {
		return "", nil
	}
	parts := make([]string, len(conds))
	var args []any
	for i, c := range conds {
		parts[i] = c.sql
		args = append(args, c.args...)
	}
	return " WHERE " + strings.Join(parts, " AND "), args
}

// TableQuery is a tiny SELECT/INSERT/UPDATE/DELETE builder bound to one table.
type TableQuery struct {
	db      *Client
	table   string
	columns []string
	conds   []condition
	order   []string
	limit   int
	offset  int
	err     error
}

// Table starts a query builder for name.
func (c *Client) Table(name string) *TableQuery {
	q := &TableQuery{db: c, table: name, limit: -1, offset: -1}
	if _, err := QuoteIdentifier(name); err != nil {
		q.err = err
	}
	return q
}

func (q *TableQuery) fail(err error) *TableQuery {
	if q.err == nil {
		q.err = err
	}
	return q
}

// Select limits the returned columns (default *).
func (q *TableQuery) Select(columns ...string) *TableQuery {
	for _, c := range columns {
		if _, err := QuoteIdentifier(c); err != nil {
			return q.fail(err)
		}
	}
	q.columns = append(q.columns, columns...)
	return q
}

// Where adds AND-ed equality conditions (nil -> IS NULL, slices -> IN).
func (q *TableQuery) Where(where Values) *TableQuery {
	conds, err := compileWhere(where)
	if err != nil {
		return q.fail(err)
	}
	q.conds = append(q.conds, conds...)
	return q
}

// WhereRaw adds a raw SQL condition with ? placeholders.
func (q *TableQuery) WhereRaw(sql string, args ...any) *TableQuery {
	q.conds = append(q.conds, condition{sql: "(" + sql + ")", args: args})
	return q
}

// OrderBy adds an ORDER BY column; desc selects descending order.
func (q *TableQuery) OrderBy(column string, desc bool) *TableQuery {
	id, err := QuoteIdentifier(column)
	if err != nil {
		return q.fail(err)
	}
	dir := "ASC"
	if desc {
		dir = "DESC"
	}
	q.order = append(q.order, id+" "+dir)
	return q
}

func (q *TableQuery) Limit(n int) *TableQuery {
	if n < 0 {
		return q.fail(bindErrorf("limit must be non-negative"))
	}
	q.limit = n
	return q
}

func (q *TableQuery) Offset(n int) *TableQuery {
	if n < 0 {
		return q.fail(bindErrorf("offset must be non-negative"))
	}
	q.offset = n
	return q
}

// ToSQL compiles the SELECT without running it.
func (q *TableQuery) ToSQL() (string, []any, error) {
	if q.err != nil {
		return "", nil, q.err
	}
	cols := "*"
	if len(q.columns) > 0 {
		quoted := make([]string, len(q.columns))
		for i, c := range q.columns {
			quoted[i], _ = QuoteIdentifier(c)
		}
		cols = strings.Join(quoted, ", ")
	}
	table, _ := QuoteIdentifier(q.table)
	where, args := whereClause(q.conds)
	sql := "SELECT " + cols + " FROM " + table + where
	if len(q.order) > 0 {
		sql += " ORDER BY " + strings.Join(q.order, ", ")
	}
	if q.limit >= 0 {
		sql += fmt.Sprintf(" LIMIT %d", q.limit)
	} else if q.offset >= 0 {
		sql += " LIMIT -1"
	}
	if q.offset >= 0 {
		sql += fmt.Sprintf(" OFFSET %d", q.offset)
	}
	return sql, args, nil
}

// Get runs the SELECT.
func (q *TableQuery) Get(ctx context.Context) ([]Row, error) {
	sql, args, err := q.ToSQL()
	if err != nil {
		return nil, err
	}
	res, err := q.db.Query(ctx, sql, args...)
	if err != nil {
		return nil, err
	}
	return res.Rows, nil
}

// First returns the first matching row or nil.
func (q *TableQuery) First(ctx context.Context) (Row, error) {
	saved := q.limit
	q.limit = 1
	defer func() { q.limit = saved }()
	rows, err := q.Get(ctx)
	if err != nil || len(rows) == 0 {
		return nil, err
	}
	return rows[0], nil
}

// Count returns the number of matching rows.
func (q *TableQuery) Count(ctx context.Context) (int64, error) {
	if q.err != nil {
		return 0, q.err
	}
	table, _ := QuoteIdentifier(q.table)
	where, args := whereClause(q.conds)
	row, err := q.db.QueryOne(ctx, "SELECT COUNT(*) AS n FROM "+table+where, args...)
	if err != nil || row == nil {
		return 0, err
	}
	n, _ := row["n"].(int64)
	return n, nil
}

// InsertSQL compiles an INSERT for rows (all rows must share the same columns).
func (q *TableQuery) InsertSQL(conflict string, rows ...Values) (string, []any, error) {
	if q.err != nil {
		return "", nil, q.err
	}
	if len(rows) == 0 || len(rows[0]) == 0 {
		return "", nil, bindErrorf("insert needs at least one row with at least one column")
	}
	cols := sortedKeys(rows[0])
	quoted := make([]string, len(cols))
	marks := make([]string, len(cols))
	for i, c := range cols {
		id, err := QuoteIdentifier(c)
		if err != nil {
			return "", nil, err
		}
		quoted[i], marks[i] = id, "?"
	}
	group := "(" + strings.Join(marks, ", ") + ")"
	groups := make([]string, len(rows))
	var args []any
	for i, row := range rows {
		if len(row) != len(cols) {
			return "", nil, bindErrorf("all inserted rows must have the same columns")
		}
		for _, c := range cols {
			v, ok := row[c]
			if !ok {
				return "", nil, bindErrorf("all inserted rows must have the same columns")
			}
			args = append(args, v)
		}
		groups[i] = group
	}
	table, _ := QuoteIdentifier(q.table)
	return "INSERT INTO " + table + " (" + strings.Join(quoted, ", ") + ") VALUES " + strings.Join(groups, ", ") + conflict, args, nil
}

// Insert inserts one or more rows atomically.
func (q *TableQuery) Insert(ctx context.Context, rows ...Values) (*ExecuteResult, error) {
	sql, args, err := q.InsertSQL("", rows...)
	if err != nil {
		return nil, err
	}
	return q.db.Execute(ctx, sql, args...)
}

// Upsert inserts rows or updates non-key columns when conflictColumns exist.
func (q *TableQuery) Upsert(ctx context.Context, conflictColumns []string, rows ...Values) (*ExecuteResult, error) {
	if len(conflictColumns) == 0 || len(rows) == 0 {
		return nil, bindErrorf("upsert needs conflict columns and at least one row")
	}
	target := make([]string, len(conflictColumns))
	isKey := map[string]bool{}
	for i, c := range conflictColumns {
		id, err := QuoteIdentifier(c)
		if err != nil {
			return nil, err
		}
		target[i] = id
		isKey[c] = true
	}
	var sets []string
	for _, c := range sortedKeys(rows[0]) {
		if !isKey[c] {
			id, err := QuoteIdentifier(c)
			if err != nil {
				return nil, err
			}
			sets = append(sets, id+" = excluded."+id)
		}
	}
	action := "DO NOTHING"
	if len(sets) > 0 {
		action = "DO UPDATE SET " + strings.Join(sets, ", ")
	}
	sql, args, err := q.InsertSQL(" ON CONFLICT ("+strings.Join(target, ", ")+") "+action, rows...)
	if err != nil {
		return nil, err
	}
	return q.db.Execute(ctx, sql, args...)
}

// Update starts an UPDATE ... SET; finish with Where(...).Run(ctx).
func (q *TableQuery) Update(values Values) *Mutation {
	m := &Mutation{db: q.db, conds: append([]condition(nil), q.conds...), err: q.err}
	if len(values) == 0 {
		m.err = bindErrorf("update needs at least one column")
		return m
	}
	table, _ := QuoteIdentifier(q.table)
	sets := make([]string, 0, len(values))
	for _, c := range sortedKeys(values) {
		id, err := QuoteIdentifier(c)
		if err != nil {
			m.err = err
			return m
		}
		sets = append(sets, id+" = ?")
		m.headArgs = append(m.headArgs, values[c])
	}
	m.head = "UPDATE " + table + " SET " + strings.Join(sets, ", ")
	return m
}

// Delete starts a DELETE; finish with Where(...).Run(ctx).
func (q *TableQuery) Delete() *Mutation {
	table, _ := QuoteIdentifier(q.table)
	return &Mutation{db: q.db, head: "DELETE FROM " + table, conds: append([]condition(nil), q.conds...), err: q.err}
}

// Mutation is an UPDATE or DELETE. It refuses to run without a WHERE clause
// unless All() was called.
type Mutation struct {
	db       *Client
	head     string
	headArgs []any
	conds    []condition
	all      bool
	err      error
}

func (m *Mutation) Where(where Values) *Mutation {
	conds, err := compileWhere(where)
	if err != nil && m.err == nil {
		m.err = err
	}
	m.conds = append(m.conds, conds...)
	return m
}

func (m *Mutation) WhereRaw(sql string, args ...any) *Mutation {
	m.conds = append(m.conds, condition{sql: "(" + sql + ")", args: args})
	return m
}

// All allows the statement to affect every row.
func (m *Mutation) All() *Mutation {
	m.all = true
	return m
}

// ToSQL compiles the statement.
func (m *Mutation) ToSQL() (string, []any, error) {
	if m.err != nil {
		return "", nil, m.err
	}
	if len(m.conds) == 0 && !m.all {
		return "", nil, ErrMissingWhere
	}
	where, args := whereClause(m.conds)
	return m.head + where, append(append([]any(nil), m.headArgs...), args...), nil
}

// Run executes the statement.
func (m *Mutation) Run(ctx context.Context) (*ExecuteResult, error) {
	sql, args, err := m.ToSQL()
	if err != nil {
		return nil, err
	}
	return m.db.Execute(ctx, sql, args...)
}
