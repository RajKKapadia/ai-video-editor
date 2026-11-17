//! Structured source-coordinate transcript records. Timeline rows describe an
//! analysis snapshot; the current editor timeline remains the authority for edits.
use super::{Range, audio::Analysis};
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};

pub(super) struct Source<'a> {
    pub media_key: &'a str,
    pub clip_id: &'a str,
    pub model: &'a str,
    pub range: Range,
    pub timeline_key: &'a str,
    pub snapshot_id: &'a str,
    pub run: usize,
    pub timeline_start: f64,
}

fn id(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    format!("{:x}", hash.finalize())
}
fn time(value: f64) -> String {
    format!("{:.0}", value * 1_000_000.0)
}

fn database(path: &Path) -> Result<Connection, String> {
    #[cfg(unix)]
    {
        use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt};
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(error)?;
    }
    let db = Connection::open(path).map_err(error)?;
    db.busy_timeout(Duration::from_secs(5)).map_err(error)?;
    let version: i64 = db
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(error)?;
    if version > SCHEMA {
        return Err("The project transcript database uses a newer schema.".into());
    }
    db.execute_batch("PRAGMA foreign_keys=ON;
        CREATE TABLE IF NOT EXISTS media (id TEXT PRIMARY KEY);
        CREATE TABLE IF NOT EXISTS analyses (
            id TEXT PRIMARY KEY, media_id TEXT NOT NULL REFERENCES media(id),
            provider TEXT NOT NULL, model TEXT NOT NULL, alignment_version INTEGER NOT NULL,
            source_start REAL NOT NULL, source_end REAL NOT NULL,
            CHECK(source_end > source_start));
        CREATE TABLE IF NOT EXISTS utterances (
            id TEXT PRIMARY KEY, analysis_id TEXT NOT NULL REFERENCES analyses(id), ordinal INTEGER NOT NULL,
            source_start REAL NOT NULL, source_end REAL NOT NULL, text TEXT NOT NULL,
            UNIQUE(analysis_id, ordinal));
        CREATE TABLE IF NOT EXISTS words (
            id TEXT PRIMARY KEY, media_id TEXT NOT NULL REFERENCES media(id), model TEXT NOT NULL,
            source_start REAL NOT NULL, source_end REAL NOT NULL, text TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS utterance_words (
            utterance_id TEXT NOT NULL REFERENCES utterances(id), position INTEGER NOT NULL,
            word_id TEXT NOT NULL REFERENCES words(id), PRIMARY KEY(utterance_id, position));
        CREATE TABLE IF NOT EXISTS timeline_runs (
            snapshot_id TEXT NOT NULL, timeline_key TEXT NOT NULL, run INTEGER NOT NULL, clip_id TEXT NOT NULL,
            analysis_id TEXT NOT NULL REFERENCES analyses(id), timeline_start REAL NOT NULL,
            PRIMARY KEY(snapshot_id, run));
        CREATE INDEX IF NOT EXISTS words_by_source ON words(media_id, source_start, source_end);
        CREATE INDEX IF NOT EXISTS analyses_by_source ON analyses(media_id, model, source_start, source_end);").map_err(error)?;
    // Version 2 only adds tables, so a version 1 file upgrades in place.
    db.execute_batch("
        CREATE VIRTUAL TABLE IF NOT EXISTS utterances_fts USING fts5(
            text, content='utterances', content_rowid='rowid', tokenize='unicode61 remove_diacritics 2');
        CREATE TRIGGER IF NOT EXISTS utterances_fts_insert AFTER INSERT ON utterances BEGIN
            INSERT INTO utterances_fts(rowid, text) VALUES (new.rowid, new.text); END;
        CREATE TRIGGER IF NOT EXISTS utterances_fts_delete AFTER DELETE ON utterances BEGIN
            INSERT INTO utterances_fts(utterances_fts, rowid, text) VALUES ('delete', old.rowid, old.text); END;
        CREATE TABLE IF NOT EXISTS embeddings (
            utterance_id TEXT NOT NULL REFERENCES utterances(id), model TEXT NOT NULL,
            dim INTEGER NOT NULL CHECK(dim > 0), vector BLOB NOT NULL CHECK(length(vector) = dim * 4),
            PRIMARY KEY(utterance_id, model));
        CREATE TABLE IF NOT EXISTS take_groups (
            id TEXT PRIMARY KEY, timeline_key TEXT NOT NULL, created_at INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS take_members (
            group_id TEXT NOT NULL REFERENCES take_groups(id) ON DELETE CASCADE, member INTEGER NOT NULL,
            position INTEGER NOT NULL, utterance_id TEXT NOT NULL REFERENCES utterances(id),
            PRIMARY KEY(group_id, member, position));
        CREATE TABLE IF NOT EXISTS edit_ops (
            id INTEGER PRIMARY KEY AUTOINCREMENT, created_at INTEGER NOT NULL, instruction TEXT NOT NULL,
            kind TEXT NOT NULL, word_ids TEXT NOT NULL, source_ranges TEXT NOT NULL,
            state TEXT NOT NULL CHECK(state IN ('proposed','applied','reverted')));
        CREATE INDEX IF NOT EXISTS take_members_by_utterance ON take_members(utterance_id);").map_err(error)?;
    if version < 2 {
        // Index utterances saved before full-text search existed.
        db.execute_batch("INSERT INTO utterances_fts(utterances_fts) VALUES ('rebuild');").map_err(error)?;
    }
    db.pragma_update(None, "user_version", SCHEMA).map_err(error)?;
    Ok(db)
}

const SCHEMA: i64 = 2;

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.as_millis() as i64)
}

// Search, embedding and take-group helpers below are not wired to a caller
// yet; only tests use them until semantic search lands.

/// A stored word in source coordinates, ready for `projection::project`.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct StoredWord {
    pub id: String,
    pub media: String,
    pub source: Range,
    pub text: String,
}

/// Distinct words of one speech model that overlap `range` of `media`.
pub(super) fn words_in_source(
    path: &Path,
    media: &str,
    model: &str,
    range: Range,
) -> Result<Vec<StoredWord>, String> {
    let db = database(path)?;
    // Words from an older alignment revision would overlap the current ones.
    let mut query = db
        .prepare(
            "SELECT DISTINCT w.id, w.source_start, w.source_end, w.text FROM words w
             JOIN utterance_words uw ON uw.word_id = w.id
             JOIN utterances u ON u.id = uw.utterance_id
             JOIN analyses a ON a.id = u.analysis_id
             WHERE w.media_id=?1 AND w.model=?2 AND w.source_end > ?3 AND w.source_start < ?4
               AND a.alignment_version = ?5
             ORDER BY w.source_start, w.source_end",
        )
        .map_err(error)?;
    query
        .query_map(
            params![media, model, range.start, range.end, super::cache::ALIGNMENT_VERSION],
            |row| {
            Ok(StoredWord {
                id: row.get(0)?,
                media: media.to_owned(),
                source: Range::new(row.get(1)?, row.get(2)?),
                text: row.get(3)?,
            })
            },
        )
        .map_err(error)?
        .collect::<Result<_, _>>()
        .map_err(error)
}

fn optional<T>(result: rusqlite::Result<T>) -> Result<Option<T>, String> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(other) => Err(error(other)),
    }
}

/// The media revision and speech model most recently analysed for a timeline
/// clip. Imported clips never change under the same ID, so this identifies
/// their transcript without reading the media again.
pub(super) fn latest_clip_source(
    path: &Path,
    clip_id: &str,
) -> Result<Option<(String, String)>, String> {
    let db = database(path)?;
    optional(db.query_row(
        "SELECT a.media_id, a.model FROM timeline_runs r JOIN analyses a ON a.id = r.analysis_id
         WHERE r.clip_id=?1 AND a.alignment_version=?2 ORDER BY r.rowid DESC LIMIT 1",
        params![clip_id, super::cache::ALIGNMENT_VERSION],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ))
}

/// The speech model most recently used for a media revision, if any.
pub(super) fn latest_media_model(path: &Path, media: &str) -> Result<Option<String>, String> {
    let db = database(path)?;
    optional(db.query_row(
        "SELECT a.model FROM timeline_runs r JOIN analyses a ON a.id = r.analysis_id
         WHERE a.media_id=?1 AND a.alignment_version=?2 ORDER BY r.rowid DESC LIMIT 1",
        params![media, super::cache::ALIGNMENT_VERSION],
        |row| row.get(0),
    ))
}

/// Source ranges already analysed with `model`, per media key.
pub(super) fn analysed_ranges(
    path: &Path,
    model: &str,
) -> Result<std::collections::HashMap<String, Vec<Range>>, String> {
    let db = database(path)?;
    let mut query = db
        .prepare("SELECT media_id, source_start, source_end FROM analyses WHERE model=?1")
        .map_err(error)?;
    let mut ranges = std::collections::HashMap::<String, Vec<Range>>::new();
    let rows = query
        .query_map([model], |row| {
            Ok((row.get::<_, String>(0)?, Range::new(row.get(1)?, row.get(2)?)))
        })
        .map_err(error)?;
    for row in rows {
        let (media, range) = row.map_err(error)?;
        ranges.entry(media).or_default().push(range);
    }
    Ok(ranges)
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Match {
    pub utterance_id: String,
    pub media: String,
    pub source: Range,
    pub text: String,
    pub score: f64,
}

/// Full-text search of one model's utterances. Words are matched in any order
/// unless the query is wrapped in double quotes, which matches the phrase.
/// Overlapping analyses of the same speech are reported once.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn search_keyword(
    path: &Path,
    model: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<Match>, String> {
    let trimmed = query.trim();
    let phrase = trimmed.len() > 1 && trimmed.starts_with('"') && trimmed.ends_with('"');
    let tokens: Vec<String> = trimmed
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|token| !token.is_empty())
        .map(|token| token.replace('"', ""))
        .collect();
    if tokens.is_empty() {
        return Ok(Vec::new());
    }
    // Quote every token so user text can never be read as FTS syntax.
    let expression = if phrase {
        format!("\"{}\"", tokens.join(" "))
    } else {
        tokens.iter().map(|token| format!("\"{token}\"")).collect::<Vec<_>>().join(" ")
    };
    let db = database(path)?;
    let mut statement = db
        .prepare(
            // FTS rank functions cannot run inside an aggregate: score in a
            // materialized step using the built-in bm25 `rank` column.
            "WITH hits AS MATERIALIZED (SELECT rowid, rank FROM utterances_fts
                           WHERE utterances_fts MATCH ?1)
             SELECT min(u.id), a.media_id, min(u.source_start), min(u.source_end), u.text, min(hits.rank)
             FROM hits JOIN utterances u ON u.rowid = hits.rowid
             JOIN analyses a ON a.id = u.analysis_id
             WHERE a.model = ?2
             GROUP BY a.media_id, round(u.source_start, 3), round(u.source_end, 3), u.text
             ORDER BY 6, 3 LIMIT ?3",
        )
        .map_err(error)?;
    statement
        .query_map(params![expression, model, limit.min(500) as i64], |row| {
            Ok(Match {
                utterance_id: row.get(0)?,
                media: row.get(1)?,
                source: Range::new(row.get(2)?, row.get(3)?),
                text: row.get(4)?,
                // bm25 is lower-is-better; report higher-is-better.
                score: -row.get::<_, f64>(5)?,
            })
        })
        .map_err(error)?
        .collect::<Result<_, _>>()
        .map_err(error)
}

/// Utterances of `model` with no embedding from `embedder` yet, deduplicated
/// like search. An utterance ID changes with its content, so edited or
/// retranscribed speech always shows up here again.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn unembedded(
    path: &Path,
    model: &str,
    embedder: &str,
    limit: usize,
) -> Result<Vec<(String, String)>, String> {
    let db = database(path)?;
    let mut statement = db
        .prepare(
            "SELECT min(u.id), u.text FROM utterances u JOIN analyses a ON a.id = u.analysis_id
             WHERE a.model = ?1 AND NOT EXISTS (
                 SELECT 1 FROM embeddings e JOIN utterances same ON same.id = e.utterance_id
                 JOIN analyses sa ON sa.id = same.analysis_id
                 WHERE e.model = ?2 AND sa.media_id = a.media_id AND same.text = u.text
                   AND round(same.source_start, 3) = round(u.source_start, 3))
             GROUP BY a.media_id, round(u.source_start, 3), round(u.source_end, 3), u.text
             ORDER BY a.media_id, u.source_start LIMIT ?3",
        )
        .map_err(error)?;
    statement
        .query_map(params![model, embedder, limit as i64], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(error)?
        .collect::<Result<_, _>>()
        .map_err(error)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn save_embeddings(
    path: &Path,
    embedder: &str,
    vectors: &[(String, Vec<f32>)],
) -> Result<(), String> {
    let mut db = database(path)?;
    let transaction = db.transaction().map_err(error)?;
    for (utterance_id, vector) in vectors {
        if vector.is_empty() || vector.iter().any(|value| !value.is_finite()) {
            return Err("An embedding was empty or not finite.".into());
        }
        let bytes: Vec<u8> = vector.iter().flat_map(|value| value.to_le_bytes()).collect();
        transaction
            .execute(
                "INSERT OR REPLACE INTO embeddings VALUES (?1,?2,?3,?4)",
                params![utterance_id, embedder, vector.len() as i64, bytes],
            )
            .map_err(error)?;
    }
    transaction.commit().map_err(error)
}

/// Nearest utterances of `model` to `query` by cosine similarity, scanning
/// every stored vector. Thousands of utterances take milliseconds; switch to an
/// index only if projects grow far beyond that.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn search_semantic(
    path: &Path,
    model: &str,
    embedder: &str,
    query: &[f32],
    limit: usize,
) -> Result<Vec<Match>, String> {
    let norm = |vector: &[f32]| vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    let query_norm = norm(query);
    if query.is_empty() || query_norm == 0.0 || !query_norm.is_finite() {
        return Err("The search embedding was empty.".into());
    }
    let db = database(path)?;
    let mut statement = db
        .prepare(
            "SELECT u.id, a.media_id, u.source_start, u.source_end, u.text, e.vector
             FROM embeddings e JOIN utterances u ON u.id = e.utterance_id
             JOIN analyses a ON a.id = u.analysis_id
             WHERE e.model = ?1 AND a.model = ?2 AND e.dim = ?3",
        )
        .map_err(error)?;
    let mut matches: Vec<Match> = statement
        .query_map(params![embedder, model, query.len() as i64], |row| {
            let bytes: Vec<u8> = row.get(5)?;
            let vector: Vec<f32> = bytes
                .chunks_exact(4)
                .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
                .collect();
            let dot: f32 = vector.iter().zip(query).map(|(a, b)| a * b).sum();
            let length = norm(&vector);
            Ok(Match {
                utterance_id: row.get(0)?,
                media: row.get(1)?,
                source: Range::new(row.get(2)?, row.get(3)?),
                text: row.get(4)?,
                score: if length > 0.0 { f64::from(dot / (length * query_norm)) } else { 0.0 },
            })
        })
        .map_err(error)?
        .collect::<Result<_, _>>()
        .map_err(error)?;
    matches.sort_by(|a, b| b.score.total_cmp(&a.score));
    matches.truncate(limit);
    Ok(matches)
}

/// Replaces the take groups recorded for `timeline_key`. Each group lists its
/// members; each member is the utterance IDs of one attempt, in order.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn save_take_groups(
    path: &Path,
    timeline_key: &str,
    groups: &[Vec<Vec<String>>],
) -> Result<(), String> {
    let mut db = database(path)?;
    let transaction = db.transaction().map_err(error)?;
    transaction
        .execute("DELETE FROM take_groups WHERE timeline_key=?1", [timeline_key])
        .map_err(error)?;
    let created = now();
    for (index, members) in groups.iter().enumerate() {
        let group_id = id(&[timeline_key, &index.to_string()]);
        transaction
            .execute(
                "INSERT INTO take_groups VALUES (?1,?2,?3)",
                params![group_id, timeline_key, created],
            )
            .map_err(error)?;
        for (member, utterances) in members.iter().enumerate() {
            for (position, utterance_id) in utterances.iter().enumerate() {
                transaction
                    .execute(
                        "INSERT INTO take_members VALUES (?1,?2,?3,?4)",
                        params![group_id, member as i64, position as i64, utterance_id],
                    )
                    .map_err(error)?;
            }
        }
    }
    transaction.commit().map_err(error)
}

/// Every attempt in the most recent group containing `utterance_id`, as lists
/// of utterance IDs. Empty when the line was never grouped with a retake.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn takes_for(path: &Path, utterance_id: &str) -> Result<Vec<Vec<String>>, String> {
    let db = database(path)?;
    let group: Option<String> = db
        .query_row(
            "SELECT g.id FROM take_members m JOIN take_groups g ON g.id = m.group_id
             WHERE m.utterance_id=?1 ORDER BY g.created_at DESC LIMIT 1",
            [utterance_id],
            |row| row.get(0),
        )
        .map(Some)
        .or_else(|err| match err {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
        .map_err(error)?;
    let Some(group) = group else { return Ok(Vec::new()) };
    let mut statement = db
        .prepare("SELECT member, utterance_id FROM take_members WHERE group_id=?1 ORDER BY member, position")
        .map_err(error)?;
    let mut members: Vec<Vec<String>> = Vec::new();
    let rows = statement
        .query_map([group], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))
        .map_err(error)?;
    for row in rows {
        let (member, utterance) = row.map_err(error)?;
        let member = member as usize;
        if members.len() <= member {
            members.resize_with(member + 1, Vec::new);
        }
        members[member].push(utterance);
    }
    members.retain(|member| !member.is_empty());
    Ok(members)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum EditState {
    Proposed,
    Applied,
    Reverted,
}

impl EditState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Applied => "applied",
            Self::Reverted => "reverted",
        }
    }
    fn parse(value: &str) -> Option<Self> {
        match value {
            "proposed" => Some(Self::Proposed),
            "applied" => Some(Self::Applied),
            "reverted" => Some(Self::Reverted),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct EditOp {
    pub id: i64,
    pub created_at: i64,
    pub instruction: String,
    pub kind: String,
    pub word_ids: Vec<String>,
    /// `(media, source range)` pairs the edit removes.
    pub source_ranges: Vec<(String, Range)>,
    pub state: EditState,
}

/// Appends an edit to the log and returns its ID. The log is never rewritten:
/// only an edit's state moves on, so the agent can refer back to it later.
pub(super) fn record_edit(
    path: &Path,
    instruction: &str,
    kind: &str,
    word_ids: &[String],
    source_ranges: &[(String, Range)],
) -> Result<i64, String> {
    let db = database(path)?;
    db.execute(
        "INSERT INTO edit_ops (created_at, instruction, kind, word_ids, source_ranges, state)
         VALUES (?1,?2,?3,?4,?5,'proposed')",
        params![
            now(),
            instruction,
            kind,
            serde_json::to_string(word_ids).map_err(error)?,
            serde_json::to_string(source_ranges).map_err(error)?,
        ],
    )
    .map_err(error)?;
    Ok(db.last_insert_rowid())
}

/// Moves an edit to `state`. Proposed edits may be applied; applied edits may
/// be reverted; reverted edits may be applied again (redo).
pub(super) fn set_edit_state(path: &Path, edit: i64, state: EditState) -> Result<(), String> {
    let db = database(path)?;
    let from: &[&str] = match state {
        EditState::Applied => &["proposed", "reverted"],
        EditState::Reverted => &["applied"],
        EditState::Proposed => &[],
    };
    let changed = db
        .execute(
            "UPDATE edit_ops SET state=?1 WHERE id=?2 AND state IN (SELECT value FROM json_each(?3))",
            params![state.as_str(), edit, serde_json::to_string(from).map_err(error)?],
        )
        .map_err(error)?;
    if changed == 0 {
        return Err("That edit is not in a state that allows this change.".into());
    }
    Ok(())
}

/// The most recent edits, newest first.
pub(super) fn recent_edits(path: &Path, limit: usize) -> Result<Vec<EditOp>, String> {
    let db = database(path)?;
    let mut statement = db
        .prepare(
            "SELECT id, created_at, instruction, kind, word_ids, source_ranges, state
             FROM edit_ops ORDER BY id DESC LIMIT ?1",
        )
        .map_err(error)?;
    let rows = statement
        .query_map([limit as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })
        .map_err(error)?;
    let mut edits = Vec::new();
    for row in rows {
        let (id, created_at, instruction, kind, words, ranges, state) = row.map_err(error)?;
        edits.push(EditOp {
            id,
            created_at,
            instruction,
            kind,
            word_ids: serde_json::from_str(&words).map_err(error)?,
            source_ranges: serde_json::from_str(&ranges).map_err(error)?,
            state: EditState::parse(&state).ok_or_else(|| error("unknown edit state"))?,
        });
    }
    Ok(edits)
}
fn error(_: impl std::fmt::Display) -> String {
    "Could not save structured transcripts in the project database.".into()
}

pub(super) fn save(path: &Path, source: &Source<'_>, analysis: &Analysis) -> Result<(), String> {
    let mut db = database(path)?;
    let transaction = db.transaction().map_err(error)?;
    let version = super::cache::ALIGNMENT_VERSION.to_string();
    // Include transcript content: retranscription never changes the meaning of an
    // existing ID, and future embeddings can be invalidated by identity alone.
    let content = serde_json::to_string(&analysis.utterances).map_err(error)?;
    let analysis_id = id(&[
        source.media_key,
        source.model,
        &version,
        &time(source.range.start),
        &time(source.range.end),
        &content,
    ]);
    transaction
        .execute(
            "INSERT OR IGNORE INTO media VALUES (?1)",
            [source.media_key],
        )
        .map_err(error)?;
    transaction
        .execute(
            "INSERT OR IGNORE INTO analyses VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                analysis_id,
                source.media_key,
                if source.model.starts_with("openai:") {
                    "openai"
                } else {
                    "whisper.cpp"
                },
                source.model,
                super::cache::ALIGNMENT_VERSION,
                source.range.start,
                source.range.end
            ],
        )
        .map_err(error)?;
    for line in &analysis.utterances {
        let utterance_id = id(&[&analysis_id, &line.ordinal.to_string()]);
        let speech = line.speech.offset(source.range.start);
        transaction
            .execute(
                "INSERT OR IGNORE INTO utterances VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    utterance_id,
                    analysis_id,
                    line.ordinal as i64,
                    speech.start,
                    speech.end,
                    line.text
                ],
            )
            .map_err(error)?;
        for (position, word) in line.words.iter().enumerate() {
            let speech = word.speech.offset(source.range.start);
            let word_id = id(&[
                source.media_key,
                source.model,
                &version,
                &time(speech.start),
                &time(speech.end),
                &word.text,
            ]);
            transaction
                .execute(
                    "INSERT OR IGNORE INTO words VALUES (?1,?2,?3,?4,?5,?6)",
                    params![
                        word_id,
                        source.media_key,
                        source.model,
                        speech.start,
                        speech.end,
                        word.text
                    ],
                )
                .map_err(error)?;
            transaction
                .execute(
                    "INSERT OR IGNORE INTO utterance_words VALUES (?1,?2,?3)",
                    params![utterance_id, position as i64, word_id],
                )
                .map_err(error)?;
        }
    }
    transaction.execute("INSERT INTO timeline_runs VALUES (?1,?2,?3,?4,?5,?6)
        ON CONFLICT(snapshot_id,run) DO UPDATE SET clip_id=excluded.clip_id, analysis_id=excluded.analysis_id,
            timeline_start=excluded.timeline_start",
        params![source.snapshot_id, source.timeline_key, source.run as i64, source.clip_id, analysis_id, source.timeline_start]).map_err(error)?;
    transaction.commit().map_err(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cleanup::audio::{Utterance, Word};
    fn fixture() -> Analysis {
        Analysis {
            pauses: vec![],
            utterances: vec![Utterance {
                speech: Range::new(1.0, 2.0),
                start: Some(0.85),
                end: Some(2.15),
                ordinal: 0,
                last: true,
                text: "Hello".into(),
                words: vec![Word {
                    speech: Range::new(1.0, 2.0),
                    text: "Hello".into(),
                    anchor: None,
                }],
            }],
            motion: Vec::new(),
        }
    }
    #[test]
    fn persists_source_words_and_reuses_ids_across_timeline_moves() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcripts.sqlite3");
        let mut source = Source {
            media_key: "revision-a",
            clip_id: "clip",
            model: "openai:whisper-1:words-v1",
            range: Range::new(10.0, 20.0),
            timeline_key: "timeline-1",
            snapshot_id: "run-1",
            run: 0,
            timeline_start: 30.0,
        };
        save(&path, &source, &fixture()).unwrap();
        source.timeline_key = "timeline-2";
        source.snapshot_id = "run-2";
        source.timeline_start = 5.0;
        save(&path, &source, &fixture()).unwrap();
        let db = database(&path).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM words", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            db.query_row("SELECT source_start FROM words", [], |row| row
                .get::<_, f64>(0))
                .unwrap(),
            11.0
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM timeline_runs", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        source.media_key = "revision-b";
        save(&path, &source, &fixture()).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM words", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            2
        );
        source.model = "different-model";
        save(&path, &source, &fixture()).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM words", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            3
        );
        assert_eq!(
            db.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
    }
    fn spoken(lines: &[(&str, f64)]) -> Analysis {
        Analysis {
            pauses: vec![],
            utterances: lines
                .iter()
                .enumerate()
                .map(|(ordinal, (text, start))| {
                    let words: Vec<Word> = text
                        .split(' ')
                        .enumerate()
                        .map(|(i, word)| Word {
                            speech: Range::new(start + i as f64 * 0.4, start + i as f64 * 0.4 + 0.3),
                            text: word.into(),
                            anchor: None,
                        })
                        .collect();
                    Utterance {
                        speech: Range::new(*start, words.last().unwrap().speech.end),
                        start: None,
                        end: None,
                        ordinal,
                        last: ordinal + 1 == lines.len(),
                        text: (*text).into(),
                        words,
                    }
                })
                .collect(),
            motion: Vec::new(),
        }
    }
    fn source<'a>(media: &'a str, range: Range, snapshot: &'a str) -> Source<'a> {
        Source {
            media_key: media,
            clip_id: "clip",
            model: "model",
            range,
            timeline_key: "timeline",
            snapshot_id: snapshot,
            run: 0,
            timeline_start: 0.0,
        }
    }
    fn lines() -> Analysis {
        spoken(&[
            ("so today we explain caching", 0.0),
            ("caching keeps answers close", 5.0),
            ("thanks for watching", 10.0),
        ])
    }

    #[test]
    fn version_one_files_upgrade_and_index_existing_utterances() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sqlite3");
        save(&path, &source("m", Range::new(0.0, 20.0), "r1"), &lines()).unwrap();
        // Turn the file back into a version 1 database, as older builds wrote it.
        {
            let db = Connection::open(&path).unwrap();
            db.execute_batch(
                "DROP TRIGGER utterances_fts_insert; DROP TRIGGER utterances_fts_delete;
                 DROP TABLE utterances_fts; DROP TABLE embeddings; DROP TABLE take_members;
                 DROP TABLE take_groups; DROP TABLE edit_ops; PRAGMA user_version=1;",
            )
            .unwrap();
        }
        let found = search_keyword(&path, "model", "caching", 10).unwrap();
        assert_eq!(found.len(), 2);
        let db = database(&path).unwrap();
        assert_eq!(
            db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0)).unwrap(),
            SCHEMA
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM utterances", [], |r| r.get::<_, i64>(0)).unwrap(),
            3
        );
        db.pragma_update(None, "user_version", SCHEMA + 1).unwrap();
        drop(db);
        assert!(database(&path).is_err());
    }

    #[test]
    fn keyword_search_matches_phrases_ignores_syntax_and_reports_duplicates_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sqlite3");
        save(&path, &source("m", Range::new(0.0, 20.0), "r1"), &lines()).unwrap();
        // A trimmed re-analysis stores the same speech again under a new analysis.
        let mut trimmed = lines();
        for line in &mut trimmed.utterances {
            line.speech = line.speech.offset(-0.5);
            for word in &mut line.words {
                word.speech = word.speech.offset(-0.5);
            }
        }
        save(&path, &source("m", Range::new(0.5, 20.0), "r2"), &trimmed).unwrap();

        let found = search_keyword(&path, "model", "Caching", 10).unwrap();
        assert_eq!(found.len(), 2, "duplicates from overlapping analyses collapse");
        assert_eq!(found[0].media, "m");
        assert_eq!(
            search_keyword(&path, "model", "\"explain caching\"", 10).unwrap().len(),
            1
        );
        assert!(search_keyword(&path, "model", "\"caching explain\"", 10).unwrap().is_empty());
        assert_eq!(search_keyword(&path, "model", "keeps caching", 10).unwrap().len(), 1);
        // FTS operators in user text are treated as plain words.
        assert!(search_keyword(&path, "model", "caching OR NEAR( * \"", 10).is_ok());
        assert!(search_keyword(&path, "model", "  ", 10).unwrap().is_empty());
        assert!(search_keyword(&path, "other-model", "caching", 10).unwrap().is_empty());
    }

    #[test]
    fn words_and_analysed_ranges_feed_the_projection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sqlite3");
        save(&path, &source("m", Range::new(100.0, 120.0), "r1"), &lines()).unwrap();
        let words = words_in_source(&path, "m", "model", Range::new(105.0, 106.0)).unwrap();
        let text: Vec<&str> = words.iter().map(|word| word.text.as_str()).collect();
        assert_eq!(text, ["caching", "keeps", "answers"]);
        assert_eq!(words[0].source, Range::new(105.0, 105.3));
        let ranges = analysed_ranges(&path, "model").unwrap();
        assert_eq!(ranges["m"], [Range::new(100.0, 120.0)]);
        assert!(analysed_ranges(&path, "other").unwrap().is_empty());
    }

    #[test]
    fn embeddings_rank_by_similarity_and_changed_speech_needs_a_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sqlite3");
        save(&path, &source("m", Range::new(0.0, 20.0), "r1"), &lines()).unwrap();
        let pending = unembedded(&path, "model", "emb", 10).unwrap();
        assert_eq!(pending.len(), 3);
        let vectors: Vec<(String, Vec<f32>)> = pending
            .iter()
            .map(|(id, text)| {
                let vector = if text.contains("caching") { vec![1.0, 0.1] } else { vec![0.0, 1.0] };
                (id.clone(), vector)
            })
            .collect();
        save_embeddings(&path, "emb", &vectors).unwrap();
        assert!(unembedded(&path, "model", "emb", 10).unwrap().is_empty());
        assert_eq!(unembedded(&path, "model", "other-embedder", 10).unwrap().len(), 3);

        let found = search_semantic(&path, "model", "emb", &[1.0, 0.0], 2).unwrap();
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|m| m.text.contains("caching")));
        assert!(found[0].score > 0.99);
        assert!(search_semantic(&path, "model", "emb", &[1.0, 0.0, 0.0], 5).unwrap().is_empty());
        assert!(search_semantic(&path, "model", "emb", &[0.0, 0.0], 5).is_err());
        assert!(save_embeddings(&path, "emb", &[(pending[0].0.clone(), vec![f32::NAN])]).is_err());

        // Retranscription that changes the words produces new utterance IDs.
        let changed = spoken(&[("so today we explain cashing", 0.0)]);
        save(&path, &source("m", Range::new(0.0, 4.0), "r2"), &changed).unwrap();
        let pending = unembedded(&path, "model", "emb", 10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1, "so today we explain cashing");
    }

    #[test]
    fn take_groups_are_replaced_per_timeline_and_looked_up_by_any_member() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sqlite3");
        save(&path, &source("m", Range::new(0.0, 20.0), "r1"), &lines()).unwrap();
        let ids: Vec<String> = unembedded(&path, "model", "x", 10)
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        save_take_groups(
            &path,
            "timeline",
            &[vec![vec![ids[0].clone()], vec![ids[1].clone(), ids[2].clone()]]],
        )
        .unwrap();
        let takes = takes_for(&path, &ids[2]).unwrap();
        assert_eq!(takes, [vec![ids[0].clone()], vec![ids[1].clone(), ids[2].clone()]]);
        save_take_groups(&path, "timeline", &[]).unwrap();
        assert!(takes_for(&path, &ids[0]).unwrap().is_empty());
        let db = database(&path).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM take_members", [], |r| r.get::<_, i64>(0)).unwrap(),
            0
        );
    }

    #[test]
    fn edit_log_appends_and_only_allows_valid_state_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sqlite3");
        let ranges = vec![("m".to_string(), Range::new(1.0, 2.5))];
        let first = record_edit(&path, "cut the intro", "cut", &["w1".into(), "w2".into()], &ranges).unwrap();
        let second = record_edit(&path, "remove ums", "cut", &["w9".into()], &[]).unwrap();
        assert!(second > first);
        assert!(set_edit_state(&path, first, EditState::Reverted).is_err());
        set_edit_state(&path, first, EditState::Applied).unwrap();
        set_edit_state(&path, first, EditState::Reverted).unwrap();
        set_edit_state(&path, first, EditState::Applied).unwrap();
        assert!(set_edit_state(&path, first, EditState::Proposed).is_err());
        assert!(set_edit_state(&path, 999, EditState::Applied).is_err());
        let edits = recent_edits(&path, 10).unwrap();
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].id, second);
        assert_eq!(edits[1].instruction, "cut the intro");
        assert_eq!(edits[1].word_ids, ["w1", "w2"]);
        assert_eq!(edits[1].source_ranges, ranges);
        assert_eq!(edits[1].state, EditState::Applied);
        assert_eq!(edits[0].state, EditState::Proposed);
    }

    #[test]
    fn source_word_ids_survive_trimmed_cache_reuse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("project.sqlite3");
        let mut source = Source {
            media_key: "media",
            clip_id: "clip",
            model: "model",
            range: Range::new(10.0, 20.0),
            timeline_key: "before",
            snapshot_id: "run-1",
            run: 0,
            timeline_start: 0.0,
        };
        save(&path, &source, &fixture()).unwrap();
        source.range = Range::new(10.5, 20.0);
        source.timeline_key = "after";
        source.snapshot_id = "run-2";
        let mut trimmed = fixture();
        trimmed.utterances[0].speech = Range::new(0.5, 1.5);
        trimmed.utterances[0].words[0].speech = Range::new(0.5, 1.5);
        save(&path, &source, &trimmed).unwrap();
        let db = database(&path).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM words", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM analyses", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
    }
}
