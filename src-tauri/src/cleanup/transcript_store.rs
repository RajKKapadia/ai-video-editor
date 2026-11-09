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
    if version > 1 {
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
        CREATE INDEX IF NOT EXISTS analyses_by_source ON analyses(media_id, model, source_start, source_end);
        PRAGMA user_version=1;").map_err(error)?;
    Ok(db)
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
