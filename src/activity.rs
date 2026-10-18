//! Today's token totals from Claude Code's local transcripts
//! (`~/.claude/projects/**/*.jsonl`, including subagent transcripts).
//!
//! Each API response can span several JSONL lines (one per content block) that
//! repeat the same usage, so usage is counted once per message id. Transcripts
//! are append-only: each file is read incrementally from where we left off,
//! and files not modified today are skipped.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, NaiveDate, TimeZone};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_create: u64,
    pub messages: u64,
}

impl Totals {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_create
    }

    fn add(&mut self, other: &Totals) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_create += other.cache_create;
        self.messages += other.messages;
    }
}

#[derive(Default)]
struct FileState {
    size: u64,
    offset: u64,
    seen: HashSet<String>,
    days: HashMap<NaiveDate, Totals>,
}

impl FileState {
    /// Counts one transcript line if it is an assistant message with usage.
    fn add_line(&mut self, line: &str) {
        if !line.contains("\"usage\"") {
            return;
        }
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            return;
        };
        if record.get("type").and_then(Value::as_str) != Some("assistant") {
            return;
        }
        let (Some(usage), Some(timestamp)) = (
            record.pointer("/message/usage"),
            record.get("timestamp").and_then(Value::as_str),
        ) else {
            return;
        };
        let Ok(time) = DateTime::parse_from_rfc3339(timestamp) else {
            return;
        };
        let id = record
            .pointer("/message/id")
            .or_else(|| record.get("requestId"))
            .and_then(Value::as_str);
        if let Some(id) = id
            && !self.seen.insert(id.to_owned())
        {
            return;
        }
        let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
        let day = time.with_timezone(&Local).date_naive();
        let totals = self.days.entry(day).or_default();
        totals.input += count("input_tokens");
        totals.output += count("output_tokens");
        totals.cache_read += count("cache_read_input_tokens");
        totals.cache_create += count("cache_creation_input_tokens");
        totals.messages += 1;
    }

    /// Reads new complete lines appended since last time.
    fn update(&mut self, file: &Path, size: u64) -> std::io::Result<()> {
        // Shrunk or replaced: start over.
        if size < self.offset {
            *self = FileState::default();
        }
        if size == self.size {
            return Ok(());
        }
        let mut handle = std::fs::File::open(file)?;
        handle.seek(SeekFrom::Start(self.offset))?;
        let mut buffer = Vec::with_capacity((size - self.offset) as usize);
        handle.take(size - self.offset).read_to_end(&mut buffer)?;
        // Only consume complete lines; a partly written last line waits for next time.
        let Some(last_newline) = buffer.iter().rposition(|b| *b == b'\n') else {
            return Ok(());
        };
        for line in String::from_utf8_lossy(&buffer[..last_newline]).split('\n') {
            self.add_line(line);
        }
        self.offset += last_newline as u64 + 1;
        self.size = size;
        Ok(())
    }
}

/// Keeps per-file progress between refreshes.
#[derive(Default)]
pub struct Activity {
    files: HashMap<PathBuf, FileState>,
}

fn list_transcripts(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => list_transcripts(&path, out),
            Ok(_) if path.extension().is_some_and(|ext| ext == "jsonl") => out.push(path),
            _ => {}
        }
    }
}

impl Activity {
    /// Totals for the local calendar day `today` across transcripts in `projects`.
    pub fn today(&mut self, projects: &Path, today: NaiveDate) -> Totals {
        let start_of_day = Local
            .from_local_datetime(&today.and_hms_opt(0, 0, 0).expect("midnight"))
            .earliest()
            .map(std::time::SystemTime::from)
            .unwrap_or(std::time::UNIX_EPOCH);
        let mut files = Vec::new();
        list_transcripts(projects, &mut files);
        let mut totals = Totals::default();
        for file in files {
            let Ok(meta) = std::fs::metadata(&file) else {
                continue;
            };
            if meta.modified().is_ok_and(|m| m < start_of_day) {
                continue; // nothing written today
            }
            let state = self.files.entry(file.clone()).or_default();
            if state.update(&file, meta.len()).is_err() {
                continue;
            }
            if let Some(day) = state.days.get(&today) {
                totals.add(day);
            }
        }
        totals
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn line(id: &str, out: u64, timestamp: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{timestamp}","message":{{"id":"{id}","usage":{{"input_tokens":1,"output_tokens":{out},"cache_read_input_tokens":100,"cache_creation_input_tokens":10}}}}}}"#
        )
    }

    #[test]
    fn dedupes_by_message_id_and_reads_incrementally() {
        let dir = std::env::temp_dir().join(format!("claude-opendeck-activity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("proj/sub")).unwrap();
        let now = Local::now();
        let stamp = now.to_rfc3339();
        let today = now.date_naive();
        let file = dir.join("proj/sub/t.jsonl");
        {
            let mut f = std::fs::File::create(&file).unwrap();
            // Same message split over two content-block lines, a user line, and a partial line.
            writeln!(f, "{}", line("m1", 5, &stamp)).unwrap();
            writeln!(f, "{}", line("m1", 5, &stamp)).unwrap();
            writeln!(f, r#"{{"type":"user","message":{{"content":"hi"}}}}"#).unwrap();
            write!(f, "{}", &line("m2", 7, &stamp)[..20]).unwrap();
        }
        let mut activity = Activity::default();
        let first = activity.today(&dir, today);
        assert_eq!(
            first,
            Totals {
                input: 1,
                output: 5,
                cache_read: 100,
                cache_create: 10,
                messages: 1
            }
        );
        assert_eq!(first.total(), 116);

        // Finish the partial line by rewriting the file with the full second message.
        let full = format!(
            "{}\n{}\n{}\n{}\n",
            line("m1", 5, &stamp),
            line("m1", 5, &stamp),
            r#"{"type":"user","message":{"content":"hi"}}"#,
            line("m2", 7, &stamp)
        );
        std::fs::write(&file, full).unwrap();
        let second = activity.today(&dir, today);
        assert_eq!(second.output, 12);
        assert_eq!(second.messages, 2);

        // Other days don't count.
        let yesterday = today.pred_opt().unwrap();
        assert_eq!(activity.today(&dir, yesterday), Totals::default());
        let _ = std::fs::remove_dir_all(dir);
    }
}
