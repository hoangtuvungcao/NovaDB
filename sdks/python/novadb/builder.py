"""A tiny query builder: ``db.table("users").where(id=1).limit(10).get()``."""

from __future__ import annotations

from typing import TYPE_CHECKING, Any, Dict, Iterable, List, Mapping, Optional, Sequence, Tuple, Union

from .errors import BindError, NovaDBError
from .sql import quote_identifier

if TYPE_CHECKING:  # pragma: no cover
    from .client import NovaDB

Condition = Tuple[str, List[Any]]
Row = Dict[str, Any]


def _compile_where(
    where: Union[Mapping[str, Any], str, None], params: Sequence[Any], kwargs: Mapping[str, Any]
) -> List[Condition]:
    conditions: List[Condition] = []
    if isinstance(where, str):
        conditions.append((f"({where})", list(params)))
    elif where is not None:
        if not isinstance(where, Mapping):
            raise BindError("where() expects a mapping, keyword arguments or a SQL string")
        kwargs = {**where, **kwargs}
    for column, value in kwargs.items():
        ident = quote_identifier(column)
        if value is None:
            conditions.append((f"{ident} IS NULL", []))
        elif isinstance(value, (list, tuple, set, frozenset)):
            values = list(value)
            if not values:
                conditions.append(("1 = 0", []))
            else:
                conditions.append((f"{ident} IN ({', '.join('?' for _ in values)})", values))
        else:
            conditions.append((f"{ident} = ?", [value]))
    return conditions


def _where_clause(conditions: List[Condition]) -> Tuple[str, List[Any]]:
    if not conditions:
        return "", []
    return (
        " WHERE " + " AND ".join(sql for sql, _ in conditions),
        [p for _, params in conditions for p in params],
    )


def _columns_of(rows: Sequence[Mapping[str, Any]]) -> List[str]:
    if not rows:
        raise BindError("insert() needs at least one row")
    columns = list(rows[0].keys())
    if not columns:
        raise BindError("insert() rows need at least one column")
    for row in rows:
        if set(row.keys()) != set(columns):
            raise BindError("all rows passed to insert() must have the same columns")
    return columns


class TableQuery:
    """SELECT / INSERT / UPDATE / DELETE helper bound to one table."""

    def __init__(self, db: "NovaDB", table: str):
        quote_identifier(table)
        self._db = db
        self.table = table
        self._columns: List[str] = []
        self._conditions: List[Condition] = []
        self._order: List[str] = []
        self._limit: Optional[int] = None
        self._offset: Optional[int] = None

    def select(self, *columns: str) -> "TableQuery":
        for c in columns:
            quote_identifier(c)
        self._columns.extend(columns)
        return self

    def where(self, where: Union[Mapping[str, Any], str, None] = None, params: Sequence[Any] = (), **kwargs: Any) -> "TableQuery":
        """``where(id=1)``, ``where({"id": [1, 2]})`` or ``where("score > ?", [10])``."""
        self._conditions.extend(_compile_where(where, params, kwargs))
        return self

    def order_by(self, column: str, direction: str = "asc") -> "TableQuery":
        direction = direction.lower()
        if direction not in ("asc", "desc"):
            raise BindError("direction must be 'asc' or 'desc'")
        self._order.append(f"{quote_identifier(column)} {direction.upper()}")
        return self

    def limit(self, count: int) -> "TableQuery":
        if not isinstance(count, int) or isinstance(count, bool) or count < 0:
            raise BindError("limit must be a non-negative integer")
        self._limit = count
        return self

    def offset(self, count: int) -> "TableQuery":
        if not isinstance(count, int) or isinstance(count, bool) or count < 0:
            raise BindError("offset must be a non-negative integer")
        self._offset = count
        return self

    def to_sql(self) -> Tuple[str, List[Any]]:
        cols = ", ".join(quote_identifier(c) for c in self._columns) if self._columns else "*"
        where_sql, params = _where_clause(self._conditions)
        sql = f"SELECT {cols} FROM {quote_identifier(self.table)}{where_sql}"
        if self._order:
            sql += " ORDER BY " + ", ".join(self._order)
        if self._limit is not None:
            sql += f" LIMIT {self._limit}"
        elif self._offset is not None:
            sql += " LIMIT -1"
        if self._offset is not None:
            sql += f" OFFSET {self._offset}"
        return sql, params

    def get(self) -> List[Row]:
        sql, params = self.to_sql()
        return self._db.query(sql, params).rows

    def first(self) -> Optional[Row]:
        saved = self._limit
        self._limit = 1
        try:
            rows = self.get()
        finally:
            self._limit = saved
        return rows[0] if rows else None

    def count(self) -> int:
        where_sql, params = _where_clause(self._conditions)
        result = self._db.query(f"SELECT COUNT(*) AS n FROM {quote_identifier(self.table)}{where_sql}", params)
        return int(result.rows[0]["n"]) if result.rows else 0

    def insert_sql(self, rows: Union[Mapping[str, Any], Iterable[Mapping[str, Any]]], conflict: str = "") -> Tuple[str, List[Any]]:
        row_list = [rows] if isinstance(rows, Mapping) else list(rows)
        columns = _columns_of(row_list)
        placeholders = "(" + ", ".join("?" for _ in columns) + ")"
        sql = (
            f"INSERT INTO {quote_identifier(self.table)} "
            f"({', '.join(quote_identifier(c) for c in columns)}) VALUES "
            + ", ".join(placeholders for _ in row_list)
            + conflict
        )
        return sql, [row[c] for row in row_list for c in columns]

    def insert(self, rows: Union[Mapping[str, Any], Iterable[Mapping[str, Any]]]) -> Dict[str, Any]:
        """Inserts one mapping or an iterable of mappings (atomically)."""
        sql, params = self.insert_sql(rows)
        return self._db.execute(sql, params)

    def upsert(self, rows: Union[Mapping[str, Any], Iterable[Mapping[str, Any]]], conflict_columns: Sequence[str]) -> Dict[str, Any]:
        """Inserts rows, updating non-key columns when ``conflict_columns`` already exist."""
        if not conflict_columns:
            raise BindError("upsert() needs at least one conflict column")
        row_list = [rows] if isinstance(rows, Mapping) else list(rows)
        columns = _columns_of(row_list)
        updates = [c for c in columns if c not in conflict_columns]
        target = ", ".join(quote_identifier(c) for c in conflict_columns)
        action = (
            "DO UPDATE SET " + ", ".join(f"{quote_identifier(c)} = excluded.{quote_identifier(c)}" for c in updates)
            if updates
            else "DO NOTHING"
        )
        sql, params = self.insert_sql(row_list, f" ON CONFLICT ({target}) {action}")
        return self._db.execute(sql, params)

    def update(self, values: Optional[Mapping[str, Any]] = None, **kwargs: Any) -> "MutationQuery":
        """Starts an UPDATE. Finish with ``.where(...).run()``."""
        values = {**(values or {}), **kwargs}
        if not values:
            raise BindError("update() needs at least one column")
        set_sql = ", ".join(f"{quote_identifier(c)} = ?" for c in values)
        return MutationQuery(self._db, f"UPDATE {quote_identifier(self.table)} SET {set_sql}", list(values.values()), list(self._conditions))

    def delete(self) -> "MutationQuery":
        """Starts a DELETE. Finish with ``.where(...).run()``."""
        return MutationQuery(self._db, f"DELETE FROM {quote_identifier(self.table)}", [], list(self._conditions))


class MutationQuery:
    """UPDATE/DELETE. Refuses to run without a WHERE clause unless ``.all()`` was called."""

    def __init__(self, db: "NovaDB", head: str, head_params: List[Any], conditions: List[Condition]):
        self._db = db
        self._head = head
        self._head_params = head_params
        self._conditions = conditions
        self._all = False

    def where(self, where: Union[Mapping[str, Any], str, None] = None, params: Sequence[Any] = (), **kwargs: Any) -> "MutationQuery":
        self._conditions.extend(_compile_where(where, params, kwargs))
        return self

    def all(self) -> "MutationQuery":
        """Explicitly allow the statement to affect every row."""
        self._all = True
        return self

    def to_sql(self) -> Tuple[str, List[Any]]:
        if not self._conditions and not self._all:
            raise NovaDBError(
                "refusing to UPDATE/DELETE without where(); call .all() to affect every row",
                "missing_where",
            )
        where_sql, params = _where_clause(self._conditions)
        return self._head + where_sql, self._head_params + params

    def run(self) -> Dict[str, Any]:
        sql, params = self.to_sql()
        return self._db.execute(sql, params)
