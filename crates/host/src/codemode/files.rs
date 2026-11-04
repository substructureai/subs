//! Files a script reads. Its runner reads the bytes, and the script's own
//! process decodes them, so the decoding counts toward the script's limits.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::rc::Rc;

use calamine::{DataRef, Reader as _, SheetType, Xlsx, XlsxCellReader, XlsxError};

use super::{Limit, Script, ScriptFile, Shown};
use substructure_session::prompts::codemode::files as prompts;

/// Bytes read from the runner at once.
const CHUNK: u64 = 256 << 10;
/// Records in one batch handed to the script.
const BATCH: usize = 512;
/// Bytes of one line or row.
const RECORD: usize = 4 << 20;
/// Files a script shows the model.

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
        Err(FileError::Missing(prompts::no_file(file)))
    }
}

/// Files held in memory, by id.
pub struct MemoryFiles(pub BTreeMap<String, Vec<u8>>);

impl Host for MemoryFiles {
    fn read(&mut self, file: &str, offset: u64, len: u64) -> Result<Vec<u8>, FileError> {
        let bytes = self
            .0
            .get(file)
            .ok_or_else(|| FileError::Missing(prompts::no_file(file)))?;
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
            return Err(io::Error::other(prompts::READ_LIMIT));
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

impl Seek for Source {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let start = self.offset - self.buf.len() as u64;
        let target = match to {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(d) => self.size.checked_add_signed(d),
            SeekFrom::Current(d) => (start + self.at as u64).checked_add_signed(d),
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before the start"))?;
        if (start..=self.offset).contains(&target) {
            self.at = (target - start) as usize;
        } else {
            self.buf.clear();
            self.at = 0;
            self.offset = target;
        }
        Ok(target)
    }
}

/// A worksheet's cells, read in order from the workbook that holds them.
#[ouroboros::self_referencing]
struct Sheet {
    book: Xlsx<Source>,
    #[borrows(mut book)]
    #[not_covariant]
    cells: XlsxCellReader<'this, Source>,
}

/// How a stream reads its file.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(tag = "format", rename_all = "snake_case", deny_unknown_fields)]
pub enum Format {
    Lines {
        #[serde(default)]
        offset: u64,
    },
    Jsonl {
        #[serde(default)]
        offset: u64,
    },
    Csv {
        #[serde(default = "yes")]
        header: bool,
        #[serde(default = "comma")]
        delimiter: char,
        #[serde(default)]
        offset: u64,
    },
    Chunks {
        #[serde(default = "chunk")]
        size: usize,
        #[serde(default)]
        offset: u64,
    },
    Xlsx {
        #[serde(default)]
        sheet: Option<String>,
        #[serde(default = "yes")]
        header: bool,
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
    Xlsx {
        rows: Rows,
        header: Option<Vec<String>>,
    },
}

/// A worksheet's rows that have a value.
struct Rows {
    sheet: Box<Sheet>,
    first: u32,
    pending: Option<((u32, u32), serde_json::Value)>,
    ended: bool,
}

struct Stream {
    decoder: Decoder,
    skip: u64,
    left: Option<u64>,
}

/// A script's files and the streams it has open.
pub struct Files {
    reader: Rc<RefCell<Reader>>,
    files: Vec<ScriptFile>,
    streams: BTreeMap<u32, Stream>,
    max_streams: usize,
    max_shown: usize,
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
            max_shown: script.limits.shown_files,
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
            prompts::no_file_of(id, &ids)
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
        if self.shown.len() >= self.max_shown {
            return Err(prompts::shown_files(self.max_shown));
        }
        let media = ["image/", "audio/", "video/"]
            .iter()
            .any(|kind| file.mime.starts_with(kind))
            || file.mime == "application/pdf";
        if media {
            if file.size > self.media_left {
                return Err(prompts::shown_media(
                    id,
                    file.size >> 10,
                    self.media_left >> 10,
                ));
            }
            self.media_left -= file.size;
            self.shown.push(Shown::File { file: file.id });
            return Ok(());
        }
        if file.size > self.shown_left {
            return Err(prompts::shown_text(
                id,
                file.size >> 10,
                self.shown_left >> 10,
            ));
        }
        let mut bytes = Vec::new();
        self.source(&file, 0)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        let text = String::from_utf8(bytes)
            .ok()
            .filter(|t| !t.contains('\0'))
            .ok_or_else(|| prompts::not_shown(id, &file.mime))?;
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
            return Err(prompts::too_big_for_text(
                id,
                file.size >> 10,
                self.text_cap >> 10,
            ));
        }
        let mut bytes = Vec::new();
        self.source(&file, 0)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn book(&self, file: &ScriptFile) -> Result<Xlsx<Source>, String> {
        Xlsx::new(self.source(file, 0)).map_err(|e| xlsx_error(e).to_string())
    }

    /// The names of a workbook's worksheets, in order.
    pub fn sheets(&mut self, id: &str) -> Result<Vec<String>, String> {
        let file = self.file(id)?.clone();
        Ok(worksheets(&self.book(&file)?))
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
            .map_err(|e| prompts::bad_options(id, &e.to_string()))?;
        self.open(id, format, limit)
    }

    pub fn open(&mut self, id: &str, format: Format, limit: Option<u64>) -> Result<u32, String> {
        if self.streams.len() >= self.max_streams {
            return Err(prompts::too_many_open(self.max_streams));
        }
        let file = self.file(id)?.clone();
        let (decoder, skip) = match format {
            Format::Lines { offset } | Format::Jsonl { offset } => (
                Decoder::Lines {
                    lines: BufReader::new(self.source(&file, 0)),
                    json: matches!(format, Format::Jsonl { .. }),
                    number: 0,
                },
                offset,
            ),
            Format::Csv {
                header,
                delimiter,
                offset,
            } => (
                Decoder::Csv {
                    rows: csv::ReaderBuilder::new()
                        .has_headers(false)
                        .flexible(true)
                        .delimiter(u8::try_from(delimiter).unwrap_or(b','))
                        .from_reader(self.source(&file, 0)),
                    header: header.then(Vec::new),
                },
                offset,
            ),
            Format::Chunks { size, offset } => (
                Decoder::Chunks {
                    source: self.source(&file, offset),
                    size: size.clamp(1, CHUNK as usize * 4),
                    carry: Vec::new(),
                },
                0,
            ),
            Format::Xlsx {
                sheet,
                header,
                offset,
            } => {
                let book = self.book(&file)?;
                let names = worksheets(&book);
                let name = match sheet {
                    Some(name) if names.contains(&name) => name,
                    Some(name) => return Err(prompts::no_sheet(id, &name, &names)),
                    None => names
                        .first()
                        .cloned()
                        .ok_or_else(|| prompts::no_sheets(id))?,
                };
                let sheet = Sheet::try_new(book, |book| book.worksheet_cells_reader(&name))
                    .map_err(|e| xlsx_error(e).to_string())?;
                let first = sheet.with_cells(|cells| cells.dimensions().start.1);
                (
                    Decoder::Xlsx {
                        rows: Rows {
                            sheet: Box::new(sheet),
                            first,
                            pending: None,
                            ended: false,
                        },
                        header: header.then(Vec::new),
                    },
                    offset,
                )
            }
        };
        let stream = self.next;
        self.next += 1;
        self.streams.insert(
            stream,
            Stream {
                decoder,
                skip,
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
        while s.skip > 0 && s.decoder.skip().map_err(|e| e.to_string())? {
            s.skip -= 1;
        }
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
    /// Passes over the next record, as `next` counts them, without building
    /// it. False at the end.
    fn skip(&mut self) -> io::Result<bool> {
        match self {
            Decoder::Lines {
                lines,
                json,
                number,
            } => loop {
                let Some(line) = read_line(lines, *number + 1)? else {
                    return Ok(false);
                };
                *number += 1;
                if !*json || !line.trim_ascii().is_empty() {
                    return Ok(true);
                }
            },
            Decoder::Csv { rows, header } => {
                if !read_header(rows, header)? {
                    return Ok(false);
                }
                rows.read_byte_record(&mut csv::ByteRecord::new())
                    .map_err(csv_error)
            }
            Decoder::Chunks { .. } | Decoder::Xlsx { .. } => Ok(self.next()?.is_some()),
        }
    }

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
                            io::Error::other(prompts::not_json(*number, &e.to_string()))
                        }),
                    };
                }
                Decoder::Csv { rows, header } => {
                    if !read_header(rows, header)? {
                        return Ok(None);
                    }
                    let Some(record) = read_row(rows)? else {
                        return Ok(None);
                    };
                    return Ok(Some(match header {
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
                Decoder::Xlsx { rows, header } => {
                    if header.as_ref().is_some_and(Vec::is_empty) {
                        let Some(row) = rows.next()? else {
                            return Ok(None);
                        };
                        *header = Some(
                            row.into_iter()
                                .enumerate()
                                .map(|(i, v)| match v {
                                    Value::Null => column(rows.first + i as u32),
                                    Value::String(s) => s,
                                    v => v.to_string(),
                                })
                                .collect(),
                        );
                    }
                    let Some(row) = rows.next()? else {
                        return Ok(None);
                    };
                    return Ok(Some(match header {
                        Some(names) => Value::Object(
                            names
                                .iter()
                                .cloned()
                                .zip(row.into_iter().chain(std::iter::repeat(Value::Null)))
                                .collect(),
                        ),
                        None => Value::Array(row),
                    }));
                }
            }
        }
    }
}

impl Rows {
    /// The next row, its cells from column `first` on and `null` where a
    /// cell is empty.
    fn next(&mut self) -> io::Result<Option<Vec<serde_json::Value>>> {
        let mut row = Vec::new();
        let mut at = None;
        loop {
            let cell = match self.pending.take() {
                Some(cell) => Some(cell),
                None if self.ended => None,
                None => self
                    .sheet
                    .with_cells_mut(|cells| {
                        cells.next_cell().map(|cell| {
                            cell.map(|cell| (cell.get_position(), cell_value(cell.get_value())))
                        })
                    })
                    .map_err(xlsx_error)?,
            };
            let Some(((r, c), value)) = cell else {
                self.ended = true;
                break;
            };
            if value.is_null() {
                continue;
            }
            match at {
                Some(a) if a != r => {
                    self.pending = Some(((r, c), value));
                    break;
                }
                _ => at = Some(r),
            }
            let i = c.saturating_sub(self.first) as usize;
            if row.len() <= i {
                row.resize(i + 1, serde_json::Value::Null);
            }
            row[i] = value;
        }
        Ok(at.map(|_| row))
    }
}

fn cell_value(data: &DataRef<'_>) -> serde_json::Value {
    use serde_json::Value;
    let number = |f: f64| serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number);
    match data {
        DataRef::Int(n) => Value::from(*n),
        DataRef::Float(f) => number(*f),
        DataRef::String(s) => Value::String(s.clone()),
        DataRef::SharedString(s) => Value::String(s.to_string()),
        DataRef::Bool(b) => Value::Bool(*b),
        DataRef::DateTime(d) => match d.as_datetime().filter(|_| d.is_datetime()) {
            Some(t) if t.time() == chrono::NaiveTime::MIN => {
                Value::String(t.format("%Y-%m-%d").to_string())
            }
            Some(t) => Value::String(t.format("%Y-%m-%dT%H:%M:%S%.f").to_string()),
            None => number(d.as_f64()),
        },
        DataRef::DateTimeIso(s) | DataRef::DurationIso(s) => Value::String(s.clone()),
        DataRef::Error(e) => Value::String(e.to_string()),
        DataRef::Empty => Value::Null,
    }
}

/// A column's letters: 0 is `A`, 26 is `AA`.
fn column(mut n: u32) -> String {
    let mut letters = Vec::new();
    loop {
        letters.push((b'A' + (n % 26) as u8) as char);
        if n < 26 {
            break;
        }
        n = n / 26 - 1;
    }
    letters.iter().rev().collect()
}

fn worksheets(book: &Xlsx<Source>) -> Vec<String> {
    book.sheets_metadata()
        .iter()
        .filter(|s| s.typ == SheetType::WorkSheet)
        .map(|s| s.name.clone())
        .collect()
}

/// A read's own error as it is; anything else as a workbook that does not read.
fn xlsx_error(e: XlsxError) -> io::Error {
    let mut source: Option<&dyn std::error::Error> = Some(&e);
    while let Some(s) = source {
        if let Some(io) = s
            .downcast_ref::<io::Error>()
            .filter(|io| matches!(io.kind(), io::ErrorKind::Other | io::ErrorKind::NotFound))
        {
            return io::Error::new(io.kind(), io.to_string());
        }
        source = s.source();
    }
    io::Error::other(prompts::bad_xlsx(&e.to_string()))
}

/// Reads the header into `header` the first time; false at the end.
fn read_header(
    rows: &mut csv::Reader<Source>,
    header: &mut Option<Vec<String>>,
) -> io::Result<bool> {
    match header {
        Some(names) if names.is_empty() => match read_row(rows)? {
            Some(record) => {
                *names = record.iter().map(str::to_string).collect();
                Ok(true)
            }
            None => Ok(false),
        },
        _ => Ok(true),
    }
}

fn read_row(rows: &mut csv::Reader<Source>) -> io::Result<Option<csv::StringRecord>> {
    let mut record = csv::StringRecord::new();
    if !rows.read_record(&mut record).map_err(csv_error)? {
        return Ok(None);
    }
    if record.as_slice().len() > RECORD {
        return Err(too_long("Row", record.position().map_or(0, |p| p.line())));
    }
    Ok(Some(record))
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
    io::Error::other(prompts::too_long(what, number, RECORD >> 20))
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
    let message = prompts::bad_csv(&e.to_string());
    match e.into_kind() {
        csv::ErrorKind::Io(e) => e,
        _ => io::Error::other(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codemode::{Language, Limits, VERSION};

    fn files<B: AsRef<[u8]>>(contents: &[(&str, B)], limits: Limits) -> Files {
        let script = Script {
            version: VERSION,
            language: Language::Javascript,
            code: String::new(),
            calls: vec![],
            seed: 0,
            now_ms: 0,
            spent: false,
            limits,
            output: crate::codemode::OUTPUT,
            cpu_used_ms: 0,
            files: contents
                .iter()
                .map(|(id, text)| ScriptFile {
                    id: id.to_string(),
                    mime: "text/plain".to_string(),
                    size: text.as_ref().len() as u64,
                    skill: None,
                })
                .collect(),
            file_bytes_used: 0,
        };
        let host = MemoryFiles(
            contents
                .iter()
                .map(|(id, text)| (id.to_string(), text.as_ref().to_vec()))
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
    fn a_stream_with_an_unknown_option_names_the_options() {
        let mut files = files(&[("a.csv", "x\n1\n")], Limits::default());
        let error = files
            .open_with("a.csv", r#"{"format":"csv","delimeter":";"}"#)
            .unwrap_err();
        assert!(
            error.contains("expected one of `header`, `delimiter`, `offset`"),
            "{error}"
        );
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
                    offset: 0,
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
    fn an_offset_skips_records_after_the_header() {
        let text: String = std::iter::once("n,sq\n".to_string())
            .chain((0..30_000).map(|i| format!("{i},{}\n", i * i)))
            .collect();
        let jsonl = format!("\n{{\"n\":0}}\n\n{{\"n\":1}}\n{{\"n\":2}}\n");
        let mut f = files(&[("sq.csv", &text), ("e.jsonl", &jsonl)], Limits::default());

        let rows = f
            .open_with("sq.csv", r#"{"format":"csv","offset":25000,"limit":2}"#)
            .unwrap();
        let rows = all(&mut f, rows);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["n"], "25000");
        assert_eq!(rows[1]["n"], "25001");

        let lines = f
            .open_with("sq.csv", r#"{"format":"lines","offset":1}"#)
            .unwrap();
        assert_eq!(
            f.next(lines).unwrap().unwrap().split('"').nth(1),
            Some("0,0")
        );

        let events = f
            .open_with("e.jsonl", r#"{"format":"jsonl","offset":1}"#)
            .unwrap();
        assert_eq!(
            all(&mut f, events),
            [serde_json::json!({"n":1}), serde_json::json!({"n":2})]
        );

        let past = f
            .open_with("sq.csv", r#"{"format":"csv","offset":40000}"#)
            .unwrap();
        assert!(all(&mut f, past).is_empty());
    }

    #[test]
    fn lines_and_json_lines_split_where_chunks_end() {
        let text: String = (0..50_000).map(|i| format!("{{\"n\":{i}}}\n")).collect();
        let mut f = files(&[("e.jsonl", &text)], Limits::default());
        let lines = f
            .open("e.jsonl", Format::Lines { offset: 0 }, Some(3))
            .unwrap();
        assert_eq!(all(&mut f, lines), ["{\"n\":0}", "{\"n\":1}", "{\"n\":2}"]);
        let events = f
            .open("e.jsonl", Format::Jsonl { offset: 0 }, None)
            .unwrap();
        let events = all(&mut f, events);
        assert_eq!(events.len(), 50_000);
        assert_eq!(events[49_999]["n"], 49_999);
    }

    #[test]
    fn blank_lines_and_long_lines_are_handled_without_recursing() {
        let blanks = format!("{}{{\"n\":1}}\n", "\n".repeat(200_000));
        let mut f = files(&[("e.jsonl", &blanks)], Limits::default());
        let stream = f
            .open("e.jsonl", Format::Jsonl { offset: 0 }, None)
            .unwrap();
        assert_eq!(all(&mut f, stream), [serde_json::json!({ "n": 1 })]);

        let long = "x".repeat(RECORD + 10);
        let mut f = files(&[("one.txt", &long)], Limits::default());
        let stream = f
            .open("one.txt", Format::Lines { offset: 0 }, None)
            .unwrap();
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
        let stream = f
            .open("big.txt", Format::Lines { offset: 0 }, None)
            .unwrap();
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
        let error = f
            .open("b.csv", Format::Lines { offset: 0 }, None)
            .unwrap_err();
        assert!(error.contains("Files: a.csv"), "{error}");
        assert!(f.text("a.csv").is_ok());
    }

    fn workbook() -> Vec<u8> {
        use rust_xlsxwriter::{ExcelDateTime, Format, Workbook};
        let mut book = Workbook::new();
        let summary = book.add_worksheet().set_name("Summary").unwrap();
        summary.write_string(1, 1, "total").unwrap();
        summary.write_number(1, 3, 42.5).unwrap();
        summary.write_string(3, 2, "after a blank row").unwrap();
        let day = Format::new().set_num_format("yyyy-mm-dd");
        let sales = book.add_worksheet().set_name("Sales").unwrap();
        for (c, name) in ["region", "amount", "", "day"].iter().enumerate() {
            sales.write_string(0, c as u16, *name).unwrap();
        }
        for i in 0..1_000u32 {
            let r = i + 1;
            sales
                .write_string(r, 0, ["EU", "US", "APAC"][i as usize % 3])
                .unwrap();
            if i != 5 {
                sales.write_number(r, 1, i as f64).unwrap();
            }
            sales.write_boolean(r, 2, i % 2 == 0).unwrap();
            let date = ExcelDateTime::from_ymd(2024, 3, 1).unwrap();
            sales.write_datetime_with_format(r, 3, &date, &day).unwrap();
        }
        book.save_to_buffer().unwrap()
    }

    #[test]
    fn xlsx_rows_come_back_typed_and_keyed_by_the_header() {
        let book = workbook();
        let mut f = files(&[("book.xlsx", book.as_slice())], Limits::default());
        assert_eq!(f.sheets("book.xlsx").unwrap(), ["Summary", "Sales"]);

        let stream = f
            .open_with("book.xlsx", r#"{"format":"xlsx","sheet":"Sales"}"#)
            .unwrap();
        let rows = all(&mut f, stream);
        assert_eq!(rows.len(), 1_000);
        assert_eq!(rows[0]["region"], "EU");
        assert_eq!(rows[0]["amount"].as_f64(), Some(0.0));
        assert_eq!(rows[0]["C"], true);
        assert_eq!(rows[0]["day"], "2024-03-01");
        assert_eq!(rows[5]["amount"], serde_json::Value::Null);
        assert_eq!(rows[999]["amount"].as_f64(), Some(999.0));

        let stream = f
            .open_with(
                "book.xlsx",
                r#"{"format":"xlsx","sheet":"Sales","offset":600,"limit":2}"#,
            )
            .unwrap();
        let rows = all(&mut f, stream);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["amount"].as_f64(), Some(600.0));
    }

    #[test]
    fn xlsx_rows_without_a_header_start_at_the_first_column_used() {
        let book = workbook();
        let mut f = files(&[("book.xlsx", book.as_slice())], Limits::default());
        let stream = f
            .open_with("book.xlsx", r#"{"format":"xlsx","header":false}"#)
            .unwrap();
        assert_eq!(
            serde_json::Value::Array(all(&mut f, stream)).to_string(),
            r#"[["total",null,42.5],[null,"after a blank row"]]"#
        );
    }

    #[test]
    fn a_bad_sheet_or_workbook_says_what_is_wrong() {
        let book = workbook();
        let mut f = files(
            &[("book.xlsx", book.as_slice()), ("a.csv", b"x\n1\n")],
            Limits::default(),
        );
        let error = f
            .open_with("book.xlsx", r#"{"format":"xlsx","sheet":"Costs"}"#)
            .unwrap_err();
        assert!(error.contains("Sheets: Summary, Sales."), "{error}");
        let error = f.open_with("a.csv", r#"{"format":"xlsx"}"#).unwrap_err();
        assert!(error.contains("could not be read as xlsx"), "{error}");
    }

    #[test]
    fn columns_are_named_by_their_letters() {
        assert_eq!(column(0), "A");
        assert_eq!(column(25), "Z");
        assert_eq!(column(26), "AA");
        assert_eq!(column(701), "ZZ");
        assert_eq!(column(702), "AAA");
    }
}
