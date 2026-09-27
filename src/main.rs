use anyhow::Context;
use anyhow::Result;
use arboard::Clipboard;
use blake3::hash as blake3_hash;
use chrono::{Datelike, Local, TimeZone, Utc};
use clap::{Parser, Subcommand};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

// ──────────────────────────────────────────────────────────────────────
// CLI
// ──────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "clipd",
    version,
    about = "Clipboard daemon — lưu, tìm kiếm, quản lý clipboard"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,

    /// Thư mục dữ liệu (mặc định ~/.local/share/clipd).
    #[arg(long, value_name = "DIR")]
    data_dir: Option<PathBuf>,

    /// Log chi tiết ra stderr.
    #[arg(long)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Chạy daemon theo dõi clipboard.
    Run {
        /// Chạy foreground. Mặc định fork ra background.
        #[arg(long)]
        foreground: bool,
        /// Dừng sau N giây (0 = mãi).
        #[arg(long, default_value_t = 0)]
        duration: u64,
        /// Khoảng cách polling (ms).
        #[arg(long, default_value_t = 400)]
        interval: u64,
    },
    /// Tìm kiếm full-text trong kho clip.
    Search {
        /// Từ khoá (FTS5). Thấp hơn 3 kí tự dùng LIKE.
        query: String,
        /// Chỉ loại text|files.
        #[arg(long)]
        kind: Option<String>,
        /// Trong N ngày gần nhất.
        #[arg(long)]
        days: Option<u64>,
        /// Số kết quả tối đa.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Copy kết quả thứ N về clipboard (1-based).
        #[arg(long, value_name = "N")]
        copy: Option<usize>,
        /// In JSON.
        #[arg(long)]
        json: bool,
    },
    /// Liệt kê clip gần đây.
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Xem toàn văn một clip.
    Get { id: i64 },
    /// Copy nội dung clip về clipboard.
    Copy { id: i64 },
    /// Đặt / xem ghi chú.
    Note { id: i64, text: Option<String> },
    /// Xoá clip.
    Rm { id: i64, #[arg(long)] yes: bool },
    /// Thống kê kho.
    Stats,
    /// Dọn clip cũ.
    Prune {
        #[arg(long)]
        dry_run: bool,
        /// Giữ tối đa N ngày (0 = vô hạn).
        #[arg(long, default_value_t = 90)]
        max_age: u64,
    },
}

// ──────────────────────────────────────────────────────────────────────
// Store
// ──────────────────────────────────────────────────────────────────────

const SCHEMA: &str = include_str!("store/schema.sql");

struct Store {
    conn: Connection,
    dir: PathBuf,
}

impl Store {
    fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir.join("clips"))?;
        let db = dir.join("clipd.db");
        let conn = Connection::open(&db)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn,
            dir: dir.to_path_buf(),
        })
    }

    fn dir(&self) -> &Path {
        &self.dir
    }


}

#[derive(Debug)]
struct Clip {
    id: i64,
    kind: String,
    preview: String,
    captured_at: i64,
    note: String,
    pinned: bool,
    use_count: i64,
    text_path: Option<String>,
    rank: Option<f64>,
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn fmt_time(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "?".into())
}

// ──────────────────────────────────────────────────────────────────────
// Clip CRUD
// ──────────────────────────────────────────────────────────────────────

fn store_clip(store: &mut Store, kind: &str, text: &str, captured_at: i64) -> Result<(i64, bool)> {
    let hash = blake3_hash(text.as_bytes());
    let hash_bytes = hash.as_bytes().to_vec();

    // Dedup.
    if let Some(id) = store
        .conn
        .query_row(
            "SELECT id FROM clips WHERE hash = ?1 LIMIT 1",
            params![&hash_bytes],
            |r| r.get(0),
        )
        .ok()
        .flatten()
    {
        return Ok((id, false));
    }

    let preview = preview_text(text, 200);
    let keywords = keywords(text, 20);

    // Write blob.
    let rel = write_blob(store, &hash_bytes, text, captured_at)?;

    let tx = store.conn.transaction()?;
    tx.execute(
        "INSERT INTO clips (hash, kind, text_path, preview, keywords, byte_size, captured_at, note, pinned, use_count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, '', 0, 0)",
        params![&hash_bytes, kind, &rel, &preview, &keywords, text.len() as i64, captured_at],
    )?;
    let id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO clips_fts (rowid, preview, note, keywords, body) VALUES (?1, ?2, '', ?3, ?4)",
        params![id, &preview, &keywords, text],
    )?;
    tx.commit()?;
    Ok((id, true))
}

fn write_blob(store: &Store, hash: &[u8], text: &str, at_ms: i64) -> Result<String> {
    let dt = Utc
        .timestamp_millis_opt(at_ms)
        .single()
        .unwrap_or_else(|| Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).unwrap());
    let rel = format!(
        "clips/{:04}/{:02}/{:02}/{}.txt",
        dt.year(),
        dt.month(),
        dt.day(),
        hex(hash)
    );
    let abs = store.dir.join(&rel);
    if let Some(p) = abs.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&abs, text)?;
    Ok(rel)
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn preview_text(text: &str, max_chars: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = flat.chars().collect();
    if chars.len() <= max_chars {
        flat
    } else {
        format!(
            "{}…",
            chars[..max_chars.saturating_sub(1)]
                .iter()
                .collect::<String>()
        )
    }
}

fn keywords(text: &str, limit: usize) -> String {
    let mut freq: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut display: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    for raw in text.split(|c: char| !c.is_alphanumeric()) {
        if raw.is_empty() {
            continue;
        }
        let tok = raw.to_string();
        // Skip hex digests.
        if tok.len() >= 16 && tok.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        // Skip high-entropy opaque blobs.
        if tok.len() >= 24 {
            let uniq: std::collections::HashSet<char> = tok.chars().collect();
            if uniq.len() as f64 / tok.len() as f64 > 0.85 {
                continue;
            }
        }
        let key = tok.to_lowercase();
        let e = freq.entry(key.clone()).or_insert(0);
        if *e == 0 {
            order.push(key.clone());
            display.insert(key.clone(), tok);
        }
        *e += 1;
    }

    let mut ranked: Vec<(usize, usize, String)> = order
        .iter()
        .enumerate()
        .map(|(i, k)| (*freq.get(k).unwrap_or(&1), i, k.clone()))
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));

    ranked
        .into_iter()
        .take(limit)
        .map(|(_, _, k)| display.get(&k).cloned().unwrap_or(k))
        .collect::<Vec<_>>()
        .join(" ")
}

fn clip_text(store: &Store, id: i64) -> Result<String> {
    let rel: String = store
        .conn
        .query_row(
            "SELECT text_path FROM clips WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .ok()
        .flatten()
        .context("clip không tồn tại")?;
    let abs = store.dir.join(&rel);
    std::fs::read_to_string(&abs).with_context(|| format!("reading {}", abs.display()))
}

// ──────────────────────────────────────────────────────────────────────
// Search / list
// ──────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Query {
    text: String,
    kind: Option<String>,
    since: Option<i64>,
    tag: Option<String>,
    has_note: Option<bool>,
    pinned_only: bool,
    limit: usize,
    offset: usize,
}

fn search(store: &Store, q: &Query) -> Result<Vec<Clip>> {
    let min_len = q.text.trim().chars().count();
    let use_fts = min_len >= 3;

    if use_fts {
        fts_search(store, q)
    } else if q.text.trim().is_empty() {
        list(store, q)
    } else {
        like_search(store, q)
    }
}

fn fts_search(store: &Store, q: &Query) -> Result<Vec<Clip>> {
    let phrase = format!("\"{}\"", q.text.trim().replace('"', "\"\""));
    let mut clauses = Vec::new();
    let mut args: Vec<rusqlite::types::Value> =
        vec![rusqlite::types::Value::Text(phrase)];

    if let Some(k) = &q.kind {
        clauses.push(format!("c.kind = ?{}", args.len() + 1));
        args.push(rusqlite::types::Value::Text(k.clone()));
    }
    if let Some(ts) = q.since {
        clauses.push(format!("c.captured_at >= ?{}", args.len() + 1));
        args.push(rusqlite::types::Value::Integer(ts));
    }
    if let Some(t) = &q.tag {
        clauses.push(format!(
            "EXISTS (SELECT 1 FROM clip_tags ct JOIN tags tg ON tg.id=ct.tag_id \
             WHERE ct.clip_id=c.id AND tg.name=?{})",
            args.len() + 1
        ));
        args.push(rusqlite::types::Value::Text(t.clone()));
    }
    match q.has_note {
        Some(true) => clauses.push("c.note <> ''".into()),
        Some(false) => clauses.push("c.note = ''".into()),
        None => {}
    }
    if q.pinned_only {
        clauses.push("c.pinned = 1".into());
    }

    let tail = if clauses.is_empty() {
        String::new()
    } else {
        format!(" AND {}", clauses.join(" AND "))
    };
    let limit_idx = args.len() + 1;
    let offset_idx = limit_idx + 1;

    let sql = format!(
        "SELECT c.id, c.kind, c.preview, c.captured_at, c.note, c.pinned, c.use_count, \
         c.text_path, bm25(clips_fts) \
         FROM clips_fts JOIN clips c ON c.id = clips_fts.rowid \
         WHERE clips_fts MATCH ?1{tail} ORDER BY rank LIMIT ?{limit_idx} OFFSET ?{offset_idx}",
    );

    args.push(rusqlite::types::Value::Integer(q.limit as i64));
    args.push(rusqlite::types::Value::Integer(q.offset as i64));

    let mut stmt = store.conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(args))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let r = row;
        out.push(Clip {
            id: r.get(0)?,
            kind: r.get(1)?,
            preview: r.get(2)?,
            captured_at: r.get(3)?,
            note: r.get(4)?,
            pinned: r.get::<_, i64>(5)? != 0,
            use_count: r.get(6)?,
            text_path: r.get(7)?,
            rank: r.get(8).ok(),
        });
    }
    Ok(out)
}

fn like_search(store: &Store, q: &Query) -> Result<Vec<Clip>> {
    let like = format!(
        "%{}%",
        q.text
            .trim()
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    let mut clauses = Vec::new();
    let mut args: Vec<rusqlite::types::Value> =
        vec![rusqlite::types::Value::Text(like)];

    if let Some(k) = &q.kind {
        clauses.push(format!("c.kind = ?{}", args.len() + 1));
        args.push(rusqlite::types::Value::Text(k.clone()));
    }
    if let Some(ts) = q.since {
        clauses.push(format!("c.captured_at >= ?{}", args.len() + 1));
        args.push(rusqlite::types::Value::Integer(ts));
    }
    match q.has_note {
        Some(true) => clauses.push("c.note <> ''".into()),
        Some(false) => clauses.push("c.note = ''".into()),
        None => {}
    }
    if q.pinned_only {
        clauses.push("c.pinned = 1".into());
    }

    let tail = if clauses.is_empty() {
        String::new()
    } else {
        format!(" AND {}", clauses.join(" AND "))
    };
    let limit_idx = args.len() + 1;
    let offset_idx = limit_idx + 1;

    let sql = format!(
        "SELECT c.id, c.kind, c.preview, c.captured_at, c.note, c.pinned, c.use_count, \
         c.text_path \
         FROM clips c \
         WHERE (c.preview LIKE ?1 ESCAPE '\\' OR c.note LIKE ?1 ESCAPE '\\'){tail} \
         ORDER BY c.captured_at DESC LIMIT ?{limit_idx} OFFSET ?{offset_idx}",
    );
    args.push(rusqlite::types::Value::Integer(q.limit as i64));
    args.push(rusqlite::types::Value::Integer(q.offset as i64));

    let mut stmt = store.conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(args))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let r = row;
        out.push(Clip {
            id: r.get(0)?,
            kind: r.get(1)?,
            preview: r.get(2)?,
            captured_at: r.get(3)?,
            note: r.get(4)?,
            pinned: r.get::<_, i64>(5)? != 0,
            use_count: r.get(6)?,
            text_path: r.get(7)?,
            rank: None,
        });
    }
    Ok(out)
}

fn list(store: &Store, q: &Query) -> Result<Vec<Clip>> {
    let mut clauses = Vec::new();
    let mut args: Vec<rusqlite::types::Value> = Vec::new();

    if let Some(k) = &q.kind {
        clauses.push(format!("c.kind = ?{}", args.len() + 1));
        args.push(rusqlite::types::Value::Text(k.clone()));
    }
    if let Some(ts) = q.since {
        clauses.push(format!("c.captured_at >= ?{}", args.len() + 1));
        args.push(rusqlite::types::Value::Integer(ts));
    }
    match q.has_note {
        Some(true) => clauses.push("c.note <> ''".into()),
        Some(false) => clauses.push("c.note = ''".into()),
        None => {}
    }
    if q.pinned_only {
        clauses.push("c.pinned = 1".into());
    }

    let tail = if clauses.is_empty() {
        String::new()
    } else {
        format!(" AND {}", clauses.join(" AND "))
    };
    let limit_idx = args.len() + 1;
    let offset_idx = limit_idx + 1;

    let sql = format!(
        "SELECT c.id, c.kind, c.preview, c.captured_at, c.note, c.pinned, c.use_count, \
         c.text_path \
         FROM clips c WHERE 1=1{tail} \
         ORDER BY c.pinned DESC, c.captured_at DESC LIMIT ?{limit_idx} OFFSET ?{offset_idx}",
    );
    args.push(rusqlite::types::Value::Integer(q.limit as i64));
    args.push(rusqlite::types::Value::Integer(q.offset as i64));

    let mut stmt = store.conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(args))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let r = row;
        out.push(Clip {
            id: r.get(0)?,
            kind: r.get(1)?,
            preview: r.get(2)?,
            captured_at: r.get(3)?,
            note: r.get(4)?,
            pinned: r.get::<_, i64>(5)? != 0,
            use_count: r.get(6)?,
            text_path: r.get(7)?,
            rank: None,
        });
    }
    Ok(out)
}

fn get_clip(store: &Store, id: i64) -> Result<Clip> {
    let mut stmt = store.conn.prepare(
        "SELECT c.id, c.kind, c.preview, c.captured_at, c.note, c.pinned, c.use_count, \
         c.text_path FROM clips c WHERE c.id = ?1",
    )?;
    let mut rows = stmt.query(params![id])?;
    let r = rows.next()?.context("clip không tồn tại")?;
    Ok(Clip {
        id: r.get(0)?,
        kind: r.get(1)?,
        preview: r.get(2)?,
        captured_at: r.get(3)?,
        note: r.get(4)?,
        pinned: r.get::<_, i64>(5)? != 0,
        use_count: r.get(6)?,
        text_path: r.get(7)?,
        rank: None,
    })
}

fn stats_simple(store: &Store) -> Result<(i64, i64, i64)> {
    let mut stmt = store.conn.prepare(
        "SELECT COUNT(*), COALESCE(SUM(pinned),0), \
         COALESCE(SUM(CASE WHEN note<>'' THEN 1 ELSE 0 END),0) FROM clips",
    )?;
    let r = stmt.query_row([], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
    })?;
    Ok(r)
}

fn record_use(store: &Store, id: i64) -> Result<()> {
    store.conn.execute(
        "UPDATE clips SET use_count = use_count + 1 WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

fn set_note(store: &Store, id: i64, text: &str) -> Result<()> {
    store.conn.execute(
        "UPDATE clips SET note = ?2 WHERE id = ?1",
        params![id, text],
    )?;
    store.conn.execute(
        "UPDATE clips_fts SET note = ?2 WHERE rowid = ?1",
        params![id, text],
    )?;
    Ok(())
}

fn remove_clip(store: &Store, id: i64) -> Result<()> {
    let rel: Option<String> = store
        .conn
        .query_row(
            "SELECT text_path FROM clips WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .ok()
        .flatten();

    store.conn.execute("DELETE FROM clips_fts WHERE rowid = ?1", params![id])?;
    store.conn.execute("DELETE FROM clips WHERE id = ?1", params![id])?;

    if let Some(r) = rel {
        let _ = std::fs::remove_file(store.dir.join(r));
    }
    Ok(())
}

fn prune(store: &Store, max_age_days: u64) -> Result<(usize, u64)> {
    let mut removed = 0;
    let mut freed = 0u64;

    if max_age_days > 0 {
        let cutoff = unix_ms() - (max_age_days as i64) * 86_400_000;
        let victims: Vec<(i64, String)> = {
            let mut stmt = store
                .conn
                .prepare("SELECT id, text_path FROM clips WHERE pinned = 0 AND captured_at < ?1")?;
            stmt.query_map(params![cutoff], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (id, rel) in victims {
            let abs = store.dir.join(&rel);
            if let Ok(md) = std::fs::metadata(&abs) {
                freed += md.len();
            }
            store.conn.execute("DELETE FROM clips_fts WHERE rowid = ?1", params![id])?;
            store.conn.execute("DELETE FROM clips WHERE id = ?1", params![id])?;
            let _ = std::fs::remove_file(abs);
            removed += 1;
        }
    }
    Ok((removed, freed))
}

// ──────────────────────────────────────────────────────────────────────
// Daemon
// ──────────────────────────────────────────────────────────────────────

fn run_daemon(store: &mut Store, interval_ms: u64, duration_secs: u64) -> Result<()> {
    let deadline = if duration_secs > 0 {
        Some(std::time::Instant::now() + std::time::Duration::from_secs(duration_secs))
    } else {
        None
    };

    let mut cb = Clipboard::new().context("mở clipboard thất bại")?;
    let mut seen: std::collections::HashSet<[u8; 32]> = std::collections::HashSet::new();
    let mut stored: u64 = 0;

    eprintln!("clipd đang chạy · pid {}", std::process::id());
    eprintln!("thư mục: {}", store.dir().display());
    eprintln!("polling: {} ms", interval_ms);

    loop {
        if let Some(d) = deadline {
            if std::time::Instant::now() >= d {
                break;
            }
        }

        match cb.get_text() {
            Ok(text) => {
                if text.trim().is_empty() {
                    std::thread::sleep(std::time::Duration::from_millis(interval_ms));
                    continue;
                }
                let hash = blake3_hash(text.as_bytes());
                let hash_arr: [u8; 32] = *hash.as_bytes();
                if seen.contains(&hash_arr) {
                    std::thread::sleep(std::time::Duration::from_millis(interval_ms));
                    continue;
                }
                seen.insert(hash_arr);
                let kind = if text.lines().all(|l| l.starts_with("file://")) {
                    "files"
                } else {
                    "text"
                };
                match store_clip(store, kind, &text, unix_ms()) {
                    Ok((id, true)) => {
                        stored += 1;
                        eprintln!("[{}] lưu clip #{} ({})", fmt_time(unix_ms()), id, kind);
                    }
                    Ok((_, false)) => {}
                    Err(e) => eprintln!("lỗi lưu: {e}"),
                }
            }
            Err(_) => {
                // Clipboard có thể đang bị ứng dụng khác giữ.
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(interval_ms));
    }

    eprintln!("dừng. {} clip mới.", stored);
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────
// Output helpers
// ──────────────────────────────────────────────────────────────────────

fn print_clips(hits: &[Clip], query: &str, json: bool) {
    if json {
        let items: Vec<_> = hits
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "kind": c.kind,
                    "preview": c.preview,
                    "captured_at": c.captured_at,
                    "captured_at_iso": fmt_time(c.captured_at),
                    "note": c.note,
                    "pinned": c.pinned,
                    "rank": c.rank,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items).unwrap_or_default());
        return;
    }

    if hits.is_empty() {
        if query.is_empty() {
            println!("(chưa có clip nào)");
        } else {
            println!("(không tìm thấy {})", query);
        }
        return;
    }
    if !query.is_empty() {
        println!("{} kết quả cho {query:?}\n", hits.len());
    }
    for c in hits {
        let pin = if c.pinned { "📌 " } else { "" };
        println!(
            "{pin}#{}  {}  [{}]  {}",
            c.id,
            fmt_time(c.captured_at),
            c.kind,
            c.preview
        );
        if !c.note.is_empty() {
            println!("   📝 {}", c.note);
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Main
// ──────────────────────────────────────────────────────────────────────

fn data_dir(cli: &Cli) -> Result<PathBuf> {
    match &cli.data_dir {
        Some(p) => Ok(p.clone()),
        None => {
            let d = dirs::data_dir()
                .context("không tìm được data dir")?
                .join("clipd");
            Ok(d)
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let dir = data_dir(&cli)?;
    let mut store = Store::open(&dir)?;

    match cli.cmd {
        Cmd::Run {
            foreground: _,
            duration,
            interval,
        } => {
            run_daemon(&mut store, interval, duration)?;
        }
        Cmd::Search {
            query,
            kind,
            days,
            limit,
            copy,
            json,
        } => {
            let mut q = Query {
                text: query,
                limit,
                ..Default::default()
            };
            if let Some(k) = kind {
                q.kind = Some(k);
            }
            if let Some(d) = days {
                q.since = Some(unix_ms() - (d as i64) * 86_400_000);
            }
            let hits = search(&store, &q)?;
            print_clips(&hits, &q.text, json);
            if let Some(n) = copy {
                if let Some(c) = hits.get(n.saturating_sub(1)) {
                    let text = clip_text(&store, c.id)?;
                    Clipboard::new()?.set_text(&text)?;
                    record_use(&store, c.id)?;
                    println!("→ copy clip #{} về clipboard", c.id);
                }
            }
        }
        Cmd::List { limit, json } => {
            let q = Query {
                limit,
                ..Default::default()
            };
            let hits = list(&store, &q)?;
            print_clips(&hits, "", json);
        }
        Cmd::Get { id } => {
            let c = get_clip(&store, id)?;
            println!("#{} · {} · {}", c.id, fmt_time(c.captured_at), c.kind);
            if !c.note.is_empty() {
                println!("Ghi chú: {}", c.note);
            }
            println!("---\n{}", clip_text(&store, id)?);
        }
        Cmd::Copy { id } => {
            let text = clip_text(&store, id)?;
            Clipboard::new()?.set_text(&text)?;
            record_use(&store, id)?;
            println!("→ copy clip #{} về clipboard", id);
        }
        Cmd::Note { id, text } => match text {
            Some(t) => {
                set_note(&store, id, &t)?;
                println!("→ ghi chú clip #{}", id);
            }
            None => {
                let c = get_clip(&store, id)?;
                if c.note.is_empty() {
                    println!("(chưa có ghi chú)");
                } else {
                    println!("{}", c.note);
                }
            }
        },
        Cmd::Rm { id, yes } => {
            if !yes {
                let c = get_clip(&store, id)?;
                print!("Xoá clip #{} ({}): ", id, c.preview);
                use std::io::Write;
                std::io::stdout().flush()?;
                let mut s = String::new();
                std::io::stdin().read_line(&mut s)?;
                if !matches!(s.trim().to_lowercase().as_str(), "y" | "yes") {
                    println!("Huỷ.");
                    return Ok(());
                }
            }
            remove_clip(&store, id)?;
            println!("→ xoá clip #{}", id);
        }
        Cmd::Stats => {
            let (n, pinned, notes) = stats_simple(&store)?;
            println!("Tổng clip : {n}");
            println!("Đã ghim   : {pinned}");
            println!("Có ghi chú: {notes}");
            println!("Thư mục   : {}", store.dir().display());
        }
        Cmd::Prune { dry_run, max_age } => {
            if dry_run {
                let (n, _, _) = stats_simple(&store)?;
                println!("Hiện tại: {} clip", n);
                println!("Chính sách: giữ tối đa {max_age} ngày");
                println!("Chạy `clipd prune` để dọn.");
                return Ok(());
            }
            let (removed, freed) = prune(&store, max_age)?;
            println!("→ xoá {} clip, giải phóng {} KB", removed, freed / 1024);
        }
    }
    Ok(())
}