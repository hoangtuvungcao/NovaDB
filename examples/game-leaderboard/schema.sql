-- Leaderboard schema shared by the Unity (C#) and JavaScript variants.
-- Both clients apply it as migration version 1 via db.migrate()/MigrateAsync().
CREATE TABLE IF NOT EXISTS players (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS scores (
    player_id   TEXT PRIMARY KEY REFERENCES players(id),
    best_score  INTEGER NOT NULL,
    level       INTEGER NOT NULL DEFAULT 1,
    updated_at  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS scores_best ON scores(best_score DESC);
