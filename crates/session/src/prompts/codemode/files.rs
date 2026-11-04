//! Errors a script's `files` calls throw.

/// A script named a file it may not read, or that is not there.
pub fn no_file(id: &str) -> String {
    format!("There is no file \"{id}\".")
}

/// A script named a file it may not read; `ids` are the files it may.
pub fn no_file_of(id: &str, ids: &[&str]) -> String {
    match ids.is_empty() {
        true => format!("{} The conversation has no files.", no_file(id)),
        false => format!("{} Files: {}.", no_file(id), ids.join(", ")),
    }
}

/// A file's blob is gone from the store.
pub fn no_longer_stored(id: &str) -> String {
    format!("\"{id}\" is no longer stored.")
}

/// A run read its file bytes.
pub const READ_LIMIT: &str =
    "The script read its limit of file bytes for this run. Read less, or stop early with `limit`.";

/// A script showed more files than it may, `max`.
pub fn shown_files(max: usize) -> String {
    format!("A script shows at most {max} files.")
}

/// A media file is bigger than what the script may still show, `left_kib`.
pub fn shown_media(id: &str, kib: u64, left_kib: u64) -> String {
    format!(
        "\"{id}\" is {kib} KiB, and the script may show {left_kib} KiB more of media. Show fewer \
         or smaller files."
    )
}

/// A text file is bigger than what the script may still show, `left_kib`.
pub fn shown_text(id: &str, kib: u64, left_kib: u64) -> String {
    format!(
        "\"{id}\" is {kib} KiB, and the script may show {left_kib} KiB more of file text. Read \
         it with `files.lines` or `files.csv`, and print what matters."
    )
}

/// A script showed a file that is neither text nor media.
pub fn not_shown(id: &str, mime: &str) -> String {
    format!("\"{id}\" is {mime}, not text or media.")
}

/// A file is bigger than `files.text` takes, `cap_kib`.
pub fn too_big_for_text(id: &str, kib: u64, cap_kib: u64) -> String {
    format!(
        "\"{id}\" is {kib} KiB, over the {cap_kib} KiB `files.text` takes. Read it with \
         `files.lines`, `files.csv` or `files.chunks`."
    )
}

/// A stream's options did not parse.
pub fn bad_options(id: &str, error: &str) -> String {
    format!("The options for \"{id}\" are not right: {error}")
}

/// A script opened more files at once than it may, `max`.
pub fn too_many_open(max: usize) -> String {
    format!("A script can have {max} files open at once. Finish or `break` out of one first.")
}

/// A `files.jsonl` line that is not JSON.
pub fn not_json(line: u64, error: &str) -> String {
    format!("Line {line} is not JSON: {error}")
}

/// A line or a row, `what`, longer than a record may be, `mib`.
pub fn too_long(what: &str, number: u64, mib: usize) -> String {
    format!("{what} {number} is over {mib} MiB. Read the file with `files.chunks`.")
}

/// A CSV that does not parse.
pub fn bad_csv(error: &str) -> String {
    format!("The CSV could not be read: {error}.")
}

/// A file that does not read as an xlsx workbook.
pub fn bad_xlsx(error: &str) -> String {
    format!("The workbook could not be read as xlsx: {error}.")
}

/// A workbook with no worksheets.
pub fn no_sheets(id: &str) -> String {
    format!("\"{id}\" has no worksheets.")
}

/// A script named a sheet the workbook does not have; `sheets` are the ones it has.
pub fn no_sheet(id: &str, sheet: &str, sheets: &[String]) -> String {
    format!(
        "\"{id}\" has no sheet \"{sheet}\". Sheets: {}.",
        sheets.join(", ")
    )
}
