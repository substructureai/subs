//! Files a script reads. Its runner reads the bytes, and the script's own
//! process decodes them, so the decoding counts toward the script's limits.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read};
use std::rc::Rc;

use super::{Limit, Script, ScriptFile, Shown};

/// Bytes read from the runner at once.
const CHUNK: u64 = 256 << 10;
/// Records in one batch handed to the script.
const BATCH: usize = 512;
/// Bytes of one line or row.
const RECORD: usize = 4 << 20;
/// Files a script shows the model.
const SHOWN: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileError {
    /// The file is not there, or not this script's to read.
    Missing(String),
    /// The run read past its limit.
    Limit(String),
}

/// Reads bytes of a script's files. A runner process asks its runner; a test
/// serves them from memory.
pub trait Host {
    /// Bytes `offset..offset + len` of `file`, fewer at its end.
    fn read(&mut self, file: &str, offset: u64, len: u64) -> Result<Vec<u8>, FileError>;
}

/// A host with no files.
pub struct NoFiles;

impl Host for NoFiles {
    fn read(&mut self, file: &str, _: u64, _: u64) -> Result<Vec<u8>, FileError> {
        Err(FileError::Missing(format!("There is no file \"{file}\".")))
    }
}

/// Files held in memory, by id.
pub struct MemoryFiles(pub BTreeMap<String, Vec<u8>>);

impl Host for MemoryFiles {
    fn read(&mut self, file: &str, offset: u64, len: u64) -> Result<Vec<u8>, FileError> {
        let bytes = self
            .0
            .get(file)
            .ok_or_else(|| FileError::Missing(format!("There is no file \"{file}\".")))?;
        let start = (offset as usize).min(bytes.len());
        let end = (start + len as usize).min(bytes.len());
        Ok(bytes[start..end].to_vec())
    }
}

/// A run's reads, held to its limit.
struct Reader {
    host: Box<dyn Host>,
    left: u64,
    read: u64,
    limited: bool,
}

impl Reader {
    fn read(&mut self, file: &str, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        if len > self.left {
            self.limited = true;
            return Err(io::Error::other(
                "The script read its limit of file bytes for this run. Read less, or stop \
                 early with `limit`.",
            ));
        }
        let bytes = self.host.read(file, offset, len).map_err(|e| match e {
            FileError::Missing(m) => io::Error::new(io::ErrorKind::NotFound, m),
            FileError::Limit(m) => {
                self.limited = true;
                io::Error::other(m)
            }
        })?;
        self.left -= bytes.len() as u64;
        self.read += bytes.len() as u64;
        Ok(bytes)
    }
}

/// One file's bytes from `offset`, read a chunk at a time.
struct Source {
    reader: Rc<RefCell<Reader>>,
    file: String,
    offset: u64,
    size: u64,
    buf: Vec<u8>,
    at: usize,
}

impl Read for Source {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.at == self.buf.len() {
            if self.offset >= self.size {
                return Ok(0);
            }
            let len = CHUNK.min(self.size - self.offset);
            self.buf = self
                .reader
                .borrow_mut()
                .read(&self.file, self.offset, len)?;
            self.at = 0;
            self.offset += self.buf.len() as u64;
            if self.buf.is_empty() {
                self.size = self.offset;
                return Ok(0);
            }
        }
        let n = out.len().min(self.buf.len() - self.at);
        out[..n].copy_from_slice(&self.buf[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// How a stream reads its file.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(tag = "format", rename_all = "snake_case")]
pub enum Format {
    Lines {},
    Jsonl {},
    Csv {
        #[serde(default = "yes")]
        header: bool,
        #[serde(default = "comma")]
        delimiter: char,
    },
    Chunks {
        #[serde(default = "chunk")]
        size: usize,
        #[serde(default)]
        offset: u64,
    },
}

fn yes() -> bool {
    true
}

fn comma() -> char {
    ','
}

fn chunk() -> usize {
    1 << 20
}

enum Decoder {
    Lines {
        lines: BufReader<Source>,
        json: bool,
        number: u64,
    },
    Csv {
        rows: csv::Reader<Source>,
        header: Option<Vec<String>>,
    },
    Chunks {
        source: Source,
        size: usize,
        carry: Vec<u8>,
    },
}

struct Stream {
    decoder: Decoder,
    left: Option<u64>,
}

/// A script's files and the streams it has open.
pub struct Files {
    reader: Rc<RefCell<Reader>>,
    files: Vec<ScriptFile>,
    streams: BTreeMap<u32, Stream>,
    max_streams: usize,
    next: u32,
    text_cap: u64,
    shown: Vec<Shown>,
    shown_left: u64,
    media_left: u64,
}

impl Files {
    pub fn new(script: &Script, host: Box<dyn Host>) -> Self {
        Files {
            reader: Rc::new(RefCell::new(Reader {
                host,
                left: script.file_bytes(),
                read: 0,
                limited: false,
            })),
            files: script.files.clone(),
            streams: BTreeMap::new(),
            max_streams: script.limits.streams as usize,
            next: 1,
            text_cap: (script.limits.memory / 8) as u64,
            shown: Vec::new(),
            shown_left: script.limits.shown as u64,
            media_left: script.limits.shown_media,
        }
    }

    pub fn list(&self) -> &[ScriptFile] {
        &self.files
    }

    /// Bytes read so far, and whether a limit stopped a read.
    pub fn spent(&self) -> (u64, Option<Limit>) {
        let reader = self.reader.borrow();
        (reader.read, reader.limited.then_some(Limit::Files))
    }

    fn file(&self, id: &str) -> Result<&ScriptFile, String> {
        self.files.iter().find(|f| f.id == id).ok_or_else(|| {
            let ids: Vec<&str> = self.files.iter().map(|f| f.id.as_str()).collect();
            match ids.is_empty() {
                true => format!("There is no file \"{id}\". The conversation has no files."),
                false => format!("There is no file \"{id}\". Files: {}.", ids.join(", ")),
            }
        })
    }

    fn source(&self, file: &ScriptFile, offset: u64) -> Source {
        Source {
            reader: self.reader.clone(),
            file: file.id.clone(),
            offset,
            size: file.size,
            buf: Vec::new(),
            at: 0,
        }
    }

    /// Puts `id` in front of the model: a media file as itself, any other
    /// file as its text.
    pub fn show(&mut self, id: &str) -> Result<(), String> {
        let file = self.file(id)?.clone();
        if self.shown.len() >= SHOWN {
            return Err(format!("A script shows at most {SHOWN} files."));
        }
        let media = ["image/", "audio/", "video/"]
            .iter()
            .any(|kind| file.mime.starts_with(kind))
            || file.mime == "application/pdf";
        if media {
            if file.size > self.media_left {
                return Err(format!(
                    "\"{id}\" is {} KiB, and the script may show {} KiB more of media. Show \
                     fewer or smaller files.",
                    file.size >> 10,
                    self.media_left >> 10
                ));
            }
            self.media_left -= file.size;
            self.shown.push(Shown::File { file: file.id });
            return Ok(());
        }
        if file.size > self.shown_left {
            return Err(format!(
                "\"{id}\" is {} KiB, and the script may show {} KiB more of file text. Read it \
                 with `files.lines` or `files.csv`, and print what matters.",
                file.size >> 10,
                self.shown_left >> 10
            ));
        }
        let mut bytes = Vec::new();
        self.source(&file, 0)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        let text = String::from_utf8(bytes)
            .ok()
            .filter(|t| !t.contains('\0'))
            .ok_or_else(|| format!("\"{id}\" is {}, not text or media.", file.mime))?;
        self.shown_left -= text.len() as u64;
        self.shown.push(Shown::Text {
            file: file.id,
            text,
        });
        Ok(())
    }

    /// What the script showed, in order.
    pub fn take_shown(&mut self) -> Vec<Shown> {
        std::mem::take(&mut self.shown)
    }

    /// The whole file as text, when it is small enough to hold.
    pub fn text(&mut self, id: &str) -> Result<String, String> {
        let file = self.file(id)?.clone();
        if file.size > self.text_cap {
            return Err(format!(
                "\"{id}\" is {} KiB, over the {} KiB `files.text` takes. Read it with \
                 `files.lines`, `files.csv` or `files.chunks`.",
                file.size >> 10,
                self.text_cap >> 10
            ));
        }
        let mut bytes = Vec::new();
        self.source(&file, 0)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Opens a stream as `options` says: its `format`, the format's own
    /// options, and an optional `limit` on records.
    pub fn open_with(&mut self, id: &str, options: &str) -> Result<u32, String> {
        let mut options: serde_json::Value =
            serde_json::from_str(options).map_err(|e| e.to_string())?;
        let limit = options
            .as_object_mut()
            .and_then(|o| o.remove("limit"))
            .and_then(|l| l.as_u64());
        let format = serde_json::from_value(options)
            .map_err(|e| format!("The options for \"{id}\" are not right: {e}"))?;
        self.open(id, format, limit)
    }

    pub fn open(&mut self, id: &str, format: Format, limit: Option<u64>) -> Result<u32, String> {
        if self.streams.len() >= self.max_streams {
            return Err(format!(
                "A script can have {} files open at once. Finish or `break` out of one first.",
                self.max_streams
            ));
        }
        let file = self.file(id)?.clone();
        let decoder = match format {
            Format::Lines {} | Format::Jsonl {} => Decoder::Lines {
                lines: BufReader::new(self.source(&file, 0)),
                json: format == Format::Jsonl {},
                number: 0,
            },
            Format::Csv { header, delimiter } => Decoder::Csv {
                rows: csv::ReaderBuilder::new()
                    .has_headers(false)
                    .flexible(true)
                    .delimiter(u8::try_from(delimiter).unwrap_or(b','))
                    .from_reader(self.source(&file, 0)),
                header: header.then(Vec::new),
            },
            Format::Chunks { size, offset } => Decoder::Chunks {
                source: self.source(&file, offset),
                size: size.clamp(1, CHUNK as usize * 4),
                carry: Vec::new(),
            },
        };
        let stream = self.next;
        self.next += 1;
        self.streams.insert(
            stream,
            Stream {
                decoder,
                left: limit,
            },
        );
        Ok(stream)
    }

    /// The next records of `stream`, as a JSON array, or `None` at its end.
    pub fn next(&mut self, stream: u32) -> Result<Option<String>, String> {
        let Some(s) = self.streams.get_mut(&stream) else {
            return Ok(None);
        };
        let mut items = Vec::new();
        while items.len() < BATCH && s.left != Some(0) {
            let Some(item) = s.decoder.next().map_err(|e| e.to_string())? else {
                break;
            };
            items.push(item);
            s.left = s.left.map(|n| n - 1);
        }
        if items.is_empty() {
            self.streams.remove(&stream);
            return Ok(None);
        }
        Ok(Some(serde_json::Value::Array(items).to_string()))
    }

    pub fn close(&mut self, stream: u32) {
        self.streams.remove(&stream);
    }
}

impl Decoder {
    fn next(&mut self) -> io::Result<Option<serde_json::Value>> {
        use serde_json::Value;
        loop {
            match self {
                Decoder::Lines {
                    lines,
                    json,
                    number,
                } => {
                    let Some(line) = read_line(lines, *number + 1)? else {
                        return Ok(None);
                    };
                    *number += 1;
                    let text = String::from_utf8_lossy(&line);
                    return match json {
                        false => Ok(Some(Value::String(text.into_owned()))),
                        true if text.trim().is_empty() => continue,
                        true => serde_json::from_str(&text).map(Some).map_err(|e| {
                            io::Error::other(format!("Line {number} is not JSON: {e}"))
                        }),
                    };
                }
                Decoder::Csv { rows, header } => {
                    let mut record = csv::StringRecord::new();
                    if !rows.read_record(&mut record).map_err(csv_error)? {
                        return Ok(None);
                    }
                    if record.as_slice().len() > RECORD {
                        return Err(too_long("Row", record.position().map_or(0, |p| p.line())));
                    }
                    return Ok(Some(match header {
                        Some(names) if names.is_empty() => {
                            *names = record.iter().map(str::to_string).collect();
                            continue;
                        }
                        Some(names) => Value::Object(
                            names
                                .iter()
                                .zip(record.iter())
                                .map(|(k, v)| (k.clone(), Value::String(v.to_string())))
                                .collect(),
                        ),
                        None => Value::Array(
                            record
                                .iter()
                                .map(|v| Value::String(v.to_string()))
                                .collect(),
                        ),
                    }));
                }
                Decoder::Chunks {
                    source,
                    size,
                    carry,
                } => {
                    let mut bytes = std::mem::take(carry);
                    let mut part = vec![0; size.saturating_sub(bytes.len()).max(1)];
                    let n = read_full(source, &mut part)?;
                    bytes.extend_from_slice(&part[..n]);
                    if bytes.is_empty() {
                        return Ok(None);
                    }
                    let cut = match std::str::from_utf8(&bytes) {
                        Ok(_) => bytes.len(),
                        Err(e) if n > 0 && e.error_len().is_none() => e.valid_up_to(),
                        Err(_) => bytes.len(),
                    };
                    *carry = bytes.split_off(cut);
                    return Ok(Some(Value::String(
                        String::from_utf8_lossy(&bytes).into_owned(),
                    )));
                }
            }
        }
    }
}

/// The next line without its line end, or `None` at the end. A line over
/// `RECORD` bytes is an error, read no further than that.
fn read_line(lines: &mut BufReader<Source>, number: u64) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = lines.fill_buf()?;
        if available.is_empty() {
            break;
        }
        let (take, done) = match available.iter().position(|b| *b == b'\n') {
            Some(at) => (at + 1, true),
            None => (available.len(), false),
        };
        if line.len() + take > RECORD + 2 {
            return Err(too_long("Line", number));
        }
        line.extend_from_slice(&available[..take]);
        lines.consume(take);
        if done {
            break;
        }
    }
    if line.is_empty() {
        return Ok(None);
    }
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
    Ok(Some(line))
}

fn too_long(what: &str, number: u64) -> io::Error {
    io::Error::other(format!(
        "{what} {number} is over {} MiB. Read the file with `files.chunks`.",
        RECORD >> 20
    ))
}

fn read_full(source: &mut Source, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match source.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

fn csv_error(e: csv::Error) -> io::Error {
    match e.into_kind() {
        csv::ErrorKind::Io(e) => e,
        other => io::Error::other(format!("The CSV could not be read: {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codemode::{Language, Limits, VERSION};

    fn files(contents: &[(&str, &str)], limits: Limits) -> Files {
        let script = Script {
            version: VERSION,
            language: Language::Javascript,
            code: String::new(),
            calls: vec![],
            seed: 0,
            now_ms: 0,
            tools: vec![],
            spent: false,
            limits,
            cpu_used_ms: 0,
            files: contents
                .iter()
                .map(|(id, text)| ScriptFile {
                    id: id.to_string(),
                    mime: "text/plain".to_string(),
                    size: text.len() as u64,
                    skill: None,
                })
                .collect(),
            file_bytes_used: 0,
        };
        let host = MemoryFiles(
            contents
                .iter()
                .map(|(id, text)| (id.to_string(), text.as_bytes().to_vec()))
                .collect(),
        );
        Files::new(&script, Box::new(host))
    }

    fn all(files: &mut Files, stream: u32) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        while let Some(batch) = files.next(stream).unwrap() {
            out.extend(serde_json::from_str::<Vec<serde_json::Value>>(&batch).unwrap());
        }
        out
    }

    #[test]
    fn csv_rows_come_back_keyed_by_the_header_across_chunks() {
        let mut text = String::from("region,amount,note\n");
        for i in 0..20_000 {
            text.push_str(&format!("r{},{i},\"a, \"\"quoted\"\"\nnote\"\n", i % 3));
        }
        let mut f = files(&[("sales.csv", &text)], Limits::default());
        let stream = f
            .open(
                "sales.csv",
                Format::Csv {
                    header: true,
                    delimiter: ',',
                },
                None,
            )
            .unwrap();
        let rows = all(&mut f, stream);
        assert_eq!(rows.len(), 20_000);
        assert_eq!(rows[7]["amount"], "7");
        assert_eq!(rows[7]["note"], "a, \"quoted\"\nnote");
        assert_eq!(f.spent().0, text.len() as u64);
    }

    #[test]
    fn lines_and_json_lines_split_where_chunks_end() {
        let text: String = (0..50_000).map(|i| format!("{{\"n\":{i}}}\n")).collect();
        let mut f = files(&[("e.jsonl", &text)], Limits::default());
        let lines = f.open("e.jsonl", Format::Lines {}, Some(3)).unwrap();
        assert_eq!(all(&mut f, lines), ["{\"n\":0}", "{\"n\":1}", "{\"n\":2}"]);
        let events = f.open("e.jsonl", Format::Jsonl {}, None).unwrap();
        let events = all(&mut f, events);
        assert_eq!(events.len(), 50_000);
        assert_eq!(events[49_999]["n"], 49_999);
    }

    #[test]
    fn blank_lines_and_long_lines_are_handled_without_recursing() {
        let blanks = format!("{}{{\"n\":1}}\n", "\n".repeat(200_000));
        let mut f = files(&[("e.jsonl", &blanks)], Limits::default());
        let stream = f.open("e.jsonl", Format::Jsonl {}, None).unwrap();
        assert_eq!(all(&mut f, stream), [serde_json::json!({ "n": 1 })]);

        let long = "x".repeat(RECORD + 10);
        let mut f = files(&[("one.txt", &long)], Limits::default());
        let stream = f.open("one.txt", Format::Lines {}, None).unwrap();
        let error = f.next(stream).unwrap_err();
        assert!(error.contains("Line 1 is over 4 MiB"), "{error}");
    }

    #[test]
    fn chunks_never_split_a_character() {
        let text = "é".repeat(300_000);
        let mut f = files(&[("t.txt", &text)], Limits::default());
        let stream = f
            .open(
                "t.txt",
                Format::Chunks {
                    size: 1001,
                    offset: 0,
                },
                None,
            )
            .unwrap();
        let joined: String = all(&mut f, stream)
            .iter()
            .map(|c| c.as_str().unwrap().to_string())
            .collect();
        assert_eq!(joined, text);
    }

    #[test]
    fn a_run_stops_at_its_file_limit() {
        let text = "x\n".repeat(1 << 20);
        let mut f = files(
            &[("big.txt", &text)],
            Limits {
                file_bytes: 300 << 10,
                ..Limits::default()
            },
        );
        let stream = f.open("big.txt", Format::Lines {}, None).unwrap();
        let mut error = None;
        for _ in 0..10_000 {
            match f.next(stream) {
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(e) => {
                    error = Some(e);
                    break;
                }
            }
        }
        assert!(error.is_some_and(|e| e.contains("limit of file bytes")));
        assert_eq!(f.spent().1, Some(Limit::Files));
    }

    #[test]
    fn an_unknown_file_names_the_files_there_are() {
        let mut f = files(&[("a.csv", "x")], Limits::default());
        let error = f.open("b.csv", Format::Lines {}, None).unwrap_err();
        assert!(error.contains("Files: a.csv"), "{error}");
        assert!(f.text("a.csv").is_ok());
    }
}
