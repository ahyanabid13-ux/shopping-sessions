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

const ROWS_PER_GROUP: usize = 1 << 20;

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

struct ChunkMeta {
    physical: i32,
    name: String,
    num_values: i64,
    size: i64,
    data_page_offset: i64,
}

/// Encodes rows `lo..hi` of one column as a page (header + body) appended to `file`.
fn write_page(file: &mut Vec<u8>, column: &Column, lo: usize, hi: usize) {
    let mut body: Vec<u8> = Vec::new();
    let count = hi - lo;
    match column {
        Column::Str { values, ends, .. } => {
            body.reserve(count * 14);
            let mut start = if lo == 0 { 0 } else { ends[lo - 1] as usize };
            for &end in &ends[lo..hi] {
                let end = end as usize;
                body.extend_from_slice(&((end - start) as u32).to_le_bytes());
                body.extend_from_slice(&values[start..end]);
                start = end;
            }
        }
        Column::OptStr { values, ends, present, .. } => {
            // Definition levels: one bit-packed run (bit width 1), with the
            // 4-byte length prefix of data page v1.
            let groups = count.div_ceil(8);
            let mut levels: Vec<u8> = Vec::with_capacity(groups + 10);
            let mut header = ((groups as u64) << 1) | 1;
            while header >= 0x80 {
                levels.push((header as u8) | 0x80);
                header >>= 7;
            }
            levels.push(header as u8);
            let base = levels.len();
            levels.resize(base + groups, 0);
            for (i, &p) in present[lo..hi].iter().enumerate() {
                if p {
                    levels[base + i / 8] |= 1 << (i % 8);
                }
            }
            body.extend_from_slice(&(levels.len() as u32).to_le_bytes());
            body.extend_from_slice(&levels);
            let mut start = if lo == 0 { 0 } else { ends[lo - 1] as usize };
            for (i, &end) in ends[lo..hi].iter().enumerate() {
                let end = end as usize;
                if present[lo + i] {
                    body.extend_from_slice(&((end - start) as u32).to_le_bytes());
                    body.extend_from_slice(&values[start..end]);
                }
                start = end;
            }
        }
        Column::I32 { values, .. } => {
            body.reserve(count * 4);
            for v in &values[lo..hi] {
                body.extend_from_slice(&v.to_le_bytes());
            }
        }
        Column::I64 { values, .. } => {
            body.reserve(count * 8);
            for v in &values[lo..hi] {
                body.extend_from_slice(&v.to_le_bytes());
            }
        }
    }
    let mut header = Thrift::new();
    header.i32(1, 0); // DATA_PAGE
    header.i32(2, body.len() as i32);
    header.i32(3, body.len() as i32);
    header.begin_struct(5); // DataPageHeader
    header.i32(1, count as i32);
    header.i32(2, 0); // PLAIN
    header.i32(3, 3); // RLE definition levels
    header.i32(4, 3); // RLE repetition levels
    header.end_struct();
    header.out.push(0); // end PageHeader
    file.extend_from_slice(&header.out);
    file.extend_from_slice(&body);
}

/// Writes a complete Parquet file with the given columns (all the same length).
pub fn write_file(path: &Path, columns: &[Column]) -> std::io::Result<()> {
    let rows = columns.first().map_or(0, |c| c.len());
    for c in columns {
        assert_eq!(c.len(), rows, "column {} has a different length", c.name());
    }
    let mut file: Vec<u8> = Vec::with_capacity(64 + rows * 48);
    file.extend_from_slice(b"PAR1");
    let mut groups: Vec<(Vec<ChunkMeta>, i64)> = Vec::new();
    let mut lo = 0;
    // An empty table has the schema and no row groups.
    while lo < rows {
        let hi = (lo + ROWS_PER_GROUP).min(rows);
        let mut chunks = Vec::new();
        for column in columns {
            let offset = file.len();
            write_page(&mut file, column, lo, hi);
            chunks.push(ChunkMeta {
                physical: column.physical_type(),
                name: column.name().to_string(),
                num_values: (hi - lo) as i64,
                size: (file.len() - offset) as i64,
                data_page_offset: offset as i64,
            });
        }
        groups.push((chunks, (hi - lo) as i64));
        lo = hi;
    }

    let mut meta = Thrift::new();
    meta.i32(1, 1); // version
    meta.list(2, T_STRUCT, columns.len() + 1);
    meta.begin_element();
    meta.binary(4, b"schema");
    meta.i32(5, columns.len() as i32);
    meta.end_struct();
    for column in columns {
        meta.begin_element();
        meta.i32(1, column.physical_type());
        let optional = matches!(column, Column::OptStr { .. });
        meta.i32(3, if optional { 1 } else { 0 }); // OPTIONAL / REQUIRED
        meta.binary(4, column.name().as_bytes());
        match column {
            Column::Str { .. } | Column::OptStr { .. } => {
                meta.i32(6, 0); // ConvertedType UTF8
                meta.begin_struct(10); // LogicalType
                meta.begin_struct(1); // STRING
                meta.end_struct();
                meta.end_struct();
            }
            Column::I32 { date: true, .. } => {
                meta.i32(6, 6); // ConvertedType DATE
                meta.begin_struct(10);
                meta.begin_struct(6); // DATE
                meta.end_struct();
                meta.end_struct();
            }
            _ => {}
        }
        meta.end_struct();
    }
    meta.i64(3, rows as i64);
    meta.list(4, T_STRUCT, groups.len());
    for (chunks, group_rows) in &groups {
        meta.begin_element();
        meta.list(1, T_STRUCT, chunks.len());
        let mut total = 0i64;
        for chunk in chunks {
            total += chunk.size;
            meta.begin_element();
            meta.i64(2, chunk.data_page_offset); // file_offset
            meta.begin_struct(3); // ColumnMetaData
            meta.i32(1, chunk.physical);
            meta.list(2, T_I32, 2);
            meta.list_i32(0); // PLAIN
            meta.list_i32(3); // RLE
            meta.list(3, T_BINARY, 1);
            meta.list_binary(chunk.name.as_bytes());
            meta.i32(4, 0); // UNCOMPRESSED
            meta.i64(5, chunk.num_values);
            meta.i64(6, chunk.size);
            meta.i64(7, chunk.size);
            meta.i64(9, chunk.data_page_offset);
            meta.end_struct();
            meta.end_struct();
        }
        meta.i64(2, total);
        meta.i64(3, *group_rows);
        meta.end_struct();
    }
    meta.binary(6, b"shopping-sessions");
    meta.out.push(0); // end FileMetaData

    file.extend_from_slice(&meta.out);
    file.extend_from_slice(&(meta.out.len() as u32).to_le_bytes());
    file.extend_from_slice(b"PAR1");
    let mut f = std::fs::File::create(path)?;
    f.write_all(&file)?;
    Ok(())
}
