-- Migration v1: Create the notes table
-- Applied by: novadb migrate <db> --dir examples/migrations/
CREATE TABLE notes (
    id         TEXT COLLATE BINARY PRIMARY KEY,
    title      TEXT NOT NULL,
    body       TEXT NOT NULL DEFAULT '',
    updated_at INTEGER NOT NULL
);

-- Migration v2: Create the tags table
CREATE TABLE tags (
    id   TEXT COLLATE BINARY PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);

-- Migration v3: Create a junction table for note-tags
-- Note: this table is NOT sync-enabled because it has a composite key.
-- Application code should manage it through the notes sync lifecycle.
CREATE TABLE note_tags (
    note_id TEXT NOT NULL,
    tag_id  TEXT NOT NULL,
    PRIMARY KEY (note_id, tag_id)
);

-- Migration v4: Add an index for faster tag lookups
CREATE INDEX idx_note_tags_tag ON note_tags(tag_id);
