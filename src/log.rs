use crate::event::{normalize_prefix, Event};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub struct Log {
    dir: PathBuf,
    home: PathBuf,
}

impl Log {
    pub fn open(home: impl Into<PathBuf>) -> Result<Self> {
        let home = home.into();
        let dir = home.join("log");
        if !dir.is_dir() {
            bail!("log directory {} does not exist", dir.display());
        }
        Ok(Self { dir, home })
    }

    pub fn append(&self, text: String) -> Result<Event> {
        let text = text.trim_end().to_owned();
        if text.trim().is_empty() {
            bail!("event text must not be empty");
        }
        let now: DateTime<Local> = Local::now();
        let event = Event {
            id: ulid::Ulid::new().to_string(),
            ts: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            text: Some(text),
            redacted: None,
        };
        let month = now.format("%Y-%m").to_string();
        let record = serde_json::to_string(&event)? + "\n";

        let lock_path = self.home.join(".lock");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(lock_path)?;
        lock.lock_exclusive()
            .context("could not acquire log lock")?;
        let result: Result<()> = (|| {
            let path = self.dir.join(format!("{month}.jsonl"));
            let mut file = OpenOptions::new().create(true).append(true).open(path)?;
            file.write_all(record.as_bytes())?;
            file.sync_all()?;
            Ok(())
        })();
        fs2::FileExt::unlock(&lock)?;
        result?;
        Ok(event)
    }

    pub fn events(&self) -> Result<Vec<Event>> {
        let mut events = Vec::new();
        for path in self.month_files()? {
            events.extend(read_file(&path)?);
        }
        events.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(events)
    }

    pub fn resolve_prefix(&self, input: &str) -> Result<Event> {
        let prefix = normalize_prefix(input)?;
        let matches: Vec<_> = self
            .events()?
            .into_iter()
            .filter(|event| event.id.starts_with(&prefix))
            .collect();
        match matches.len() {
            0 => bail!("no event matches `{prefix}`"),
            1 => Ok(matches.into_iter().next().unwrap()),
            _ => bail!(
                "ambiguous id prefix `{prefix}`; matches:\n{}",
                matches
                    .iter()
                    .map(|e| e.id.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        }
    }

    pub fn since(&self, id: &str) -> Result<Vec<Event>> {
        Ok(self
            .events()?
            .into_iter()
            .filter(|e| e.id.as_str() > id)
            .collect())
    }
    pub fn upto(&self, id: &str) -> Result<Vec<Event>> {
        Ok(self
            .events()?
            .into_iter()
            .filter(|e| e.id.as_str() <= id)
            .collect())
    }

    pub fn redact(&self, id_prefix: &str, reason: &str) -> Result<Event> {
        if reason.trim().is_empty() {
            bail!("redaction reason must not be empty");
        }
        let event = self.resolve_prefix(id_prefix)?;
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(self.home.join(".lock"))?;
        lock.lock_exclusive()?;
        let result: Result<()> = (|| {
            for path in self.month_files()? {
                let raw = fs::read_to_string(&path)?;
                let mut found = false;
                let mut lines = Vec::new();
                for line in raw.lines() {
                    let mut record: Event = serde_json::from_str(line)?;
                    if record.id == event.id {
                        record.text = None;
                        record.redacted = Some(reason.to_string());
                        found = true;
                    }
                    lines.push(serde_json::to_string(&record)?);
                }
                if found {
                    let temp = path.with_extension("jsonl.tmp");
                    let mut output = File::create(&temp)?;
                    output.write_all(lines.join("\n").as_bytes())?;
                    output.write_all(b"\n")?;
                    output.sync_all()?;
                    fs::rename(temp, path)?;
                    break;
                }
            }
            Ok(())
        })();
        fs2::FileExt::unlock(&lock)?;
        result?;
        Ok(event)
    }

    pub fn month_files(&self) -> Result<Vec<PathBuf>> {
        let mut paths = fs::read_dir(&self.dir)?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.ends_with(".jsonl"))
                    .unwrap_or(false)
            })
            .collect::<Vec<_>>();
        paths.sort();
        Ok(paths)
    }

    pub fn check(&self, repair: bool) -> Result<Vec<String>> {
        let mut problems = Vec::new();
        for path in self.month_files()? {
            let mut raw = String::new();
            File::open(&path)?.read_to_string(&mut raw)?;
            if raw.is_empty() {
                continue;
            }
            for (index, piece) in raw.split('\n').enumerate() {
                if piece.is_empty() && index + 1 == raw.split('\n').count() {
                    continue;
                }
                match serde_json::from_str::<Event>(piece).and_then(|event| {
                    event.validate().map_err(|error| {
                        <serde_json::Error as serde::de::Error>::custom(error.to_string())
                    })?;
                    Ok(event)
                }) {
                    Ok(_) => {}
                    Err(_) if index + 1 == raw.split('\n').count() && !raw.ends_with('\n') => {
                        let message = format!("{} has a torn final line", path.display());
                        if repair {
                            let quarantine = self.home.join("state/quarantine");
                            fs::create_dir_all(&quarantine)?;
                            let torn =
                                format!("{}.torn", path.file_name().unwrap().to_string_lossy());
                            fs::write(quarantine.join(torn), piece.as_bytes())?;
                            let valid = &raw
                                [..raw.rfind('\n').unwrap_or(0) + usize::from(raw.contains('\n'))];
                            fs::write(&path, valid)?;
                            problems.push(format!("repaired {message}"));
                        } else {
                            problems.push(message);
                        }
                    }
                    Err(error) => {
                        bail!("{} line {} is invalid: {error}", path.display(), index + 1)
                    }
                }
            }
        }
        Ok(problems)
    }
}

fn read_file(path: &Path) -> Result<Vec<Event>> {
    let raw = fs::read_to_string(path)?;
    let mut events = Vec::new();
    for (number, line) in raw.lines().enumerate() {
        let event: Event = serde_json::from_str(line)
            .with_context(|| format!("{} line {} is invalid", path.display(), number + 1))?;
        event
            .validate()
            .with_context(|| format!("{} line {} is invalid", path.display(), number + 1))?;
        events.push(event);
    }
    if !raw.is_empty() && !raw.ends_with('\n') {
        bail!(
            "{} has a torn final line; run `lw doctor --repair`",
            path.display()
        );
    }
    Ok(events)
}
