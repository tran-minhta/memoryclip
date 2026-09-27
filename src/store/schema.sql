-- memoryclip schema. Applied on every open; every statement is idempotent.
--
-- `clips` holds the metadata index. The clip text itself lives on disk under
-- `clips/` (plain) or `vault/` (encrypted), addressed by `hash`.

PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS clips (
    id           INTEGER PRIMARY KEY,
    hash         BLOB    NOT NULL UNIQUE,
    kind         TEXT    NOT NULL,
    -- Path relative to the data directory. NULL when a clip has no blob yet.
    text_path    TEXT,
    preview      TEXT    NOT NULL,
    keywords     TEXT    NOT NULL DEFAULT '',
    source_app   TEXT,
    byte_size    INTEGER NOT NULL,
    -- Unix milliseconds.
    captured_at  INTEGER NOT NULL,
    note         TEXT    NOT NULL DEFAULT '',
    pinned       INTEGER NOT NULL DEFAULT 0,
    is_sensitive INTEGER NOT NULL DEFAULT 0,
    use_count    INTEGER NOT NULL DEFAULT 0,
    last_used_at INTEGER
);

CREATE INDEX IF NOT EXISTS idx_clips_captured ON clips (captured_at DESC);
CREATE INDEX IF NOT EXISTS idx_clips_pinned   ON clips (pinned DESC, captured_at DESC);
CREATE INDEX IF NOT EXISTS idx_clips_prune    ON clips (pinned, captured_at);

CREATE TABLE IF NOT EXISTS tags (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS clip_tags (
    clip_id INTEGER NOT NULL REFERENCES clips (id) ON DELETE CASCADE,
    tag_id  INTEGER NOT NULL REFERENCES tags (id)  ON DELETE CASCADE,
    PRIMARY KEY (clip_id, tag_id)
);

CREATE INDEX IF NOT EXISTS idx_clip_tags_tag ON clip_tags (tag_id);

-- Standalone FTS table, maintained explicitly by Store so that encrypted clips
-- can be indexed without their plaintext ever entering the index.
--
-- `trigram` is used instead of `unicode61` because Vietnamese is not
-- whitespace-delimited: "đăng nhập" is a single word, so accent-stripped
-- prefix matching silently misses. Trigram matching finds both whole words and
-- substrings inside them. It needs queries of at least 3 characters, below
-- which Store falls back to LIKE.
CREATE VIRTUAL TABLE IF NOT EXISTS clips_fts USING fts5 (
    preview,
    note,
    keywords,
    body,
    tokenize = 'trigram'
);