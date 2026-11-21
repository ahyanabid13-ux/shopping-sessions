//! A small Parquet writer: uncompressed, PLAIN-encoded data pages (format
//! version 1), one page per column per row group, and Thrift compact
//! metadata. Enough for the two output tables and readable by pyarrow,
//! polars, duckdb and Spark.

use std::io::Write;
use std::path::Path;

/// A column's values, already PLAIN-encoded where that matters.
pub enum Column<'a> {
    /// Required UTF-8 strings: `values` holds each value's bytes, `ends` the
    /// exclusive end offset of each value.
    Str { name: &'a str, values: &'a [u8], ends: &'a [u32] },
    /// Optional UTF-8 strings; `present[i]` false is null (and has an empty
    /// value in `values`/`ends`).
    OptStr { name: &'a str, values: &'a [u8], ends: &'a [u32], present: &'a [bool] },
    /// Required INT32, annotated as DATE when `date`.
    I32 { name: &'a str, values: &'a [i32], date: bool },
    /// Required INT64.
    I64 { name: &'a str, values: &'a [i64] },
}

impl Column<'_> {
    fn name(&self) -> &str {
        match self {
            Column::Str { name, .. } | Column::OptStr { name, .. } | Column::I32 { name, .. } | Column::I64 { name, .. } => {
                name
            }
        }
    }

    fn len(&self) -> usize {
        match self {
            Column::Str { ends, .. } | Column::OptStr { ends, .. } => ends.len(),
            Column::I32 { values, .. } => values.len(),
            Column::I64 { values, .. } => values.len(),
        }
    }

    fn physical_type(&self) -> i32 {
        match self {
            Column::Str { .. } | Column::OptStr { .. } => 6, // BYTE_ARRAY
            Column::I32 { .. } => 1,                          // INT32
            Column::I64 { .. } => 2,                          // INT64
        }
    }
}

// Thrift compact protocol type ids.
const T_I32: u8 = 5;
const T_I64: u8 = 6;
const T_BINARY: u8 = 8;
const T_LIST: u8 = 9;
const T_STRUCT: u8 = 12;

struct Thrift {
    out: Vec<u8>,
    last_field: Vec<i16>,
}

impl Thrift {
    fn new() -> Self {
        Thrift { out: Vec::new(), last_field: vec![0] }
    }

    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.out.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.out.push(v as u8);
    }

    fn zigzag(v: i64) -> u64 {
        ((v << 1) ^ (v >> 63)) as u64
    }

    fn field(&mut self, id: i16, ty: u8) {
        let last = self.last_field.last_mut().unwrap();
        let delta = id - *last;
        if (1..=15).contains(&delta) {
            self.out.push(((delta as u8) << 4) | ty);
        } else {
            self.out.push(ty);
            let z = Self::zigzag(id as i64);
            self.varint(z);
        }
        *self.last_field.last_mut().unwrap() = id;
    }

    fn i32(&mut self, id: i16, v: i32) {
        self.field(id, T_I32);
        self.varint(Self::zigzag(v as i64));
    }

    fn i64(&mut self, id: i16, v: i64) {
        self.field(id, T_I64);
        self.varint(Self::zigzag(v));
    }

    fn binary(&mut self, id: i16, v: &[u8]) {
        self.field(id, T_BINARY);
        self.varint(v.len() as u64);
        self.out.extend_from_slice(v);
    }

    fn begin_struct(&mut self, id: i16) {
        self.field(id, T_STRUCT);
        self.last_field.push(0);
    }

    /// Starts a struct that is a list element (no field header).
    fn begin_element(&mut self) {
        self.last_field.push(0);
    }

    fn end_struct(&mut self) {
        self.out.push(0);
        self.last_field.pop();
    }

    fn list(&mut self, id: i16, element_type: u8, size: usize) {
        self.field(id, T_LIST);
        if size < 15 {
            self.out.push(((size as u8) << 4) | element_type);
        } else {
            self.out.push(0xF0 | element_type);
            self.varint(size as u64);
        }
    }

    fn list_i32(&mut self, v: i32) {
        self.varint(Self::zigzag(v as i64));
    }

    fn list_binary(&mut self, v: &[u8]) {
        self.varint(v.len() as u64);
        self.out.extend_from_slice(v);
    }
}

/// Physical and logical type of a column.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Required UTF-8 string.
    Str,
    /// Optional UTF-8 string (the body starts with its definition levels).
    OptStr,
    /// Required INT32 annotated as DATE.
    Date,
    /// Required INT32.
    I32,
    /// Required INT64.
    I64,
}

impl Kind {
    fn physical(self) -> i32 {
        match self {
            Kind::Str | Kind::OptStr => 6, // BYTE_ARRAY
            Kind::Date | Kind::I32 => 1,   // INT32
            Kind::I64 => 2,                // INT64
        }
    }
}

/// One column chunk of the file's single row group: one PLAIN data page whose
/// body is the concatenation of `parts`, already encoded.
pub struct Chunk<'a> {
    pub name: &'a str,
    pub kind: Kind,
    pub parts: Vec<&'a [u8]>,
}

/// Appends one PLAIN BYTE_ARRAY value (4-byte length, then the bytes).
#[inline(always)]
pub fn put_str(body: &mut Vec<u8>, value: &[u8]) {
    body.extend_from_slice(&(value.len() as u32).to_le_bytes());
    body.extend_from_slice(value);
}

/// Definition levels of an optional column of `count` values, as data page
/// v1 stores them: a 4-byte length, then one bit-packed run of bit width 1.
pub struct Levels {
    bytes: Vec<u8>,
    base: usize,
}

impl Levels {
    pub fn new(count: usize) -> Levels {
        let groups = count.div_ceil(8);
        let mut bytes: Vec<u8> = Vec::with_capacity(groups + 16);
        bytes.extend_from_slice(&[0; 4]);
        let mut header = ((groups as u64) << 1) | 1;
        while header >= 0x80 {
            bytes.push((header as u8) | 0x80);
            header >>= 7;
        }
        bytes.push(header as u8);
        let base = bytes.len();
        bytes.resize(base + groups, 0);
        let len = (bytes.len() - 4) as u32;
        bytes[..4].copy_from_slice(&len.to_le_bytes());
        Levels { bytes, base }
    }

    /// Marks value `i` as present (not null).
    #[inline(always)]
    pub fn set(&mut self, i: usize) {
        self.bytes[self.base + i / 8] |= 1 << (i % 8);
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Encodes rows of a generic column as one page body.
fn encode(column: &Column) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    match column {
        Column::Str { values, ends, .. } => {
            let mut start = 0usize;
            for &end in ends.iter() {
                put_str(&mut body, &values[start..end as usize]);
                start = end as usize;
            }
        }
        Column::OptStr { values, ends, present, .. } => {
            let mut levels = Levels::new(ends.len());
            let mut data = Vec::new();
            let mut start = 0usize;
            for (i, &end) in ends.iter().enumerate() {
                if present[i] {
                    levels.set(i);
                    put_str(&mut data, &values[start..end as usize]);
                }
                start = end as usize;
            }
            body.extend_from_slice(levels.bytes());
            body.extend_from_slice(&data);
        }
        Column::I32 { values, .. } => {
            for v in values.iter() {
                body.extend_from_slice(&v.to_le_bytes());
            }
        }
        Column::I64 { values, .. } => {
            for v in values.iter() {
                body.extend_from_slice(&v.to_le_bytes());
            }
        }
    }
    body
}

/// Writes a complete Parquet file with the given columns (all the same length).
pub fn write_file(path: &Path, columns: &[Column]) -> std::io::Result<()> {
    let rows = columns.first().map_or(0, |c| c.len());
    for c in columns {
        assert_eq!(c.len(), rows, "column {} has a different length", c.name());
    }
    let bodies: Vec<Vec<u8>> = columns.iter().map(encode).collect();
    let chunks: Vec<Chunk> = columns
        .iter()
        .zip(&bodies)
        .map(|(c, body)| Chunk {
            name: c.name(),
            kind: match c {
                Column::Str { .. } => Kind::Str,
                Column::OptStr { .. } => Kind::OptStr,
                Column::I32 { date: true, .. } => Kind::Date,
                Column::I32 { date: false, .. } => Kind::I32,
                Column::I64 { .. } => Kind::I64,
            },
            parts: vec![body.as_slice()],
        })
        .collect();
    write_chunks(path, rows, &chunks)
}

/// Writes a Parquet file of `rows` rows as one row group (none when empty)
/// with one uncompressed PLAIN data page (format v1) per column.
pub fn write_chunks(path: &Path, rows: usize, chunks: &[Chunk]) -> std::io::Result<()> {
    let mut f = std::io::BufWriter::with_capacity(1 << 16, std::fs::File::create(path)?);
    f.write_all(b"PAR1")?;
    let mut offset = 4i64;
    // (data page offset, chunk size) per column.
    let mut placed: Vec<(i64, i64)> = Vec::with_capacity(chunks.len());
    if rows > 0 {
        for chunk in chunks {
            let body_len: usize = chunk.parts.iter().map(|p| p.len()).sum();
            assert!(body_len < i32::MAX as usize, "page of column {} too large", chunk.name);
            let mut header = Thrift::new();
            header.i32(1, 0); // DATA_PAGE
            header.i32(2, body_len as i32);
            header.i32(3, body_len as i32);
            header.begin_struct(5); // DataPageHeader
            header.i32(1, rows as i32);
            header.i32(2, 0); // PLAIN
            header.i32(3, 3); // RLE definition levels
            header.i32(4, 3); // RLE repetition levels
            header.end_struct();
            header.out.push(0); // end PageHeader
            f.write_all(&header.out)?;
            for part in &chunk.parts {
                f.write_all(part)?;
            }
            let size = (header.out.len() + body_len) as i64;
            placed.push((offset, size));
            offset += size;
        }
    }

    let mut meta = Thrift::new();
    meta.i32(1, 1); // version
    meta.list(2, T_STRUCT, chunks.len() + 1);
    meta.begin_element();
    meta.binary(4, b"schema");
    meta.i32(5, chunks.len() as i32);
    meta.end_struct();
    for chunk in chunks {
        meta.begin_element();
        meta.i32(1, chunk.kind.physical());
        meta.i32(3, if chunk.kind == Kind::OptStr { 1 } else { 0 }); // OPTIONAL / REQUIRED
        meta.binary(4, chunk.name.as_bytes());
        match chunk.kind {
            Kind::Str | Kind::OptStr => {
                meta.i32(6, 0); // ConvertedType UTF8
                meta.begin_struct(10); // LogicalType
                meta.begin_struct(1); // STRING
                meta.end_struct();
                meta.end_struct();
            }
            Kind::Date => {
                meta.i32(6, 6); // ConvertedType DATE
                meta.begin_struct(10);
                meta.begin_struct(6); // DATE
                meta.end_struct();
                meta.end_struct();
            }
            Kind::I32 | Kind::I64 => {}
        }
        meta.end_struct();
    }
    meta.i64(3, rows as i64);
    meta.list(4, T_STRUCT, if rows > 0 { 1 } else { 0 });
    if rows > 0 {
        meta.begin_element();
        meta.list(1, T_STRUCT, chunks.len());
        let mut total = 0i64;
        for (chunk, &(data_page_offset, size)) in chunks.iter().zip(&placed) {
            total += size;
            meta.begin_element();
            meta.i64(2, data_page_offset); // file_offset
            meta.begin_struct(3); // ColumnMetaData
            meta.i32(1, chunk.kind.physical());
            meta.list(2, T_I32, 2);
            meta.list_i32(0); // PLAIN
            meta.list_i32(3); // RLE
            meta.list(3, T_BINARY, 1);
            meta.list_binary(chunk.name.as_bytes());
            meta.i32(4, 0); // UNCOMPRESSED
            meta.i64(5, rows as i64);
            meta.i64(6, size);
            meta.i64(7, size);
            meta.i64(9, data_page_offset);
            meta.end_struct();
            meta.end_struct();
        }
        meta.i64(2, total);
        meta.i64(3, rows as i64);
        meta.end_struct();
    }
    meta.binary(6, b"shopping-sessions");
    meta.out.push(0); // end FileMetaData

    f.write_all(&meta.out)?;
    f.write_all(&(meta.out.len() as u32).to_le_bytes())?;
    f.write_all(b"PAR1")?;
    f.flush()?;
    Ok(())
}
