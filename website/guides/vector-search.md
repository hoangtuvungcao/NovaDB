# Vector search

NovaDB adds SQL functions for comparing embedding vectors. Vectors are stored either as JSON arrays in `TEXT`
columns (`'[0.1, 0.2, 0.3]'`) or as compact float32 `BLOB`s created with `vector_to_blob()`.

::: tip Status
The `vector_*` functions work in the current build. A bug in an earlier 0.1.2 development build, where the T-SQL type
rewriter turned the `VECTOR` prefix into `TEXT` (`no such function: TEXT_dim`), is fixed. Every function below was
run against the current build, including the top-k example.
:::

## Functions

| Function | Returns | Description |
|---|---|---|
| `vector_cosine_similarity(a, b)` | `REAL` | Cosine similarity in `[-1, 1]` |
| `vector_cosine_distance(a, b)` | `REAL` | `1 - cosine_similarity`, in `[0, 2]` |
| `vector_l2_distance(a, b)` | `REAL` | Euclidean distance |
| `vector_dot_product(a, b)` | `REAL` | Inner product |
| `vector_norm(v)` | `REAL` | L2 norm |
| `vector_normalize(v)` | `TEXT` | Unit-length vector as a JSON array |
| `vector_dim(v)` | `INTEGER` | Number of elements |
| `vector_to_blob(json)` | `BLOB` | JSON array to packed little-endian float32 |
| `vector_from_blob(blob)` | `TEXT` | Packed float32 to JSON array |

Arguments may be JSON text or float32 BLOBs (mixing them works). Both arguments must have the same dimension: with
different dimensions the distance and similarity functions return `NULL` instead of raising an error. BLOBs are
float32, so `vector_from_blob()` shows float32 rounding (`0.12` comes back as `0.11999999731779099`).

## Example: top-k similarity

```sql
CREATE TABLE articles (
  id        INTEGER PRIMARY KEY,
  title     TEXT NOT NULL,
  embedding BLOB NOT NULL          -- vector_to_blob('[...]')
);

INSERT INTO articles(title, embedding)
VALUES ('Intro to SQLite', vector_to_blob('[0.12, -0.45, 0.88, 0.33]'));

SELECT id, title,
       vector_cosine_similarity(embedding, vector_to_blob('[0.10, -0.40, 0.90, 0.30]')) AS score
FROM articles
ORDER BY score DESC
LIMIT 5;
```

Embeddings are produced by your own model or API; NovaDB only stores and compares them.

## Performance

Search is an **exact, sequential scan**: every candidate row is scored. There is no approximate-nearest-neighbour
index (HNSW, IVF). This is fine for thousands to low hundreds of thousands of vectors; beyond that, pre-filter
with ordinary indexed columns (`WHERE category = ?`) or use a dedicated vector store.

The Studio's **Vector Lab** tab lets you try the functions interactively.
