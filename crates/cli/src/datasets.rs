//! Canonical HF dataset streaming via opendal + parquet.

use anyhow::{Context, bail};
use arrow::array::{Array, BinaryArray, LargeBinaryArray, LargeStringArray, StringArray};
use futures::TryStreamExt;
use opendal::Operator;
use opendal::services::Huggingface;
use parquet::arrow::ParquetRecordBatchStreamBuilder;
use parquet_opendal::AsyncReader;

use crate::counter::BigramCounter;

pub const DATASETS: &[Dataset] = &[
    Dataset { name: "fineweb", repo: "HuggingFaceFW/fineweb",
              field: "text", prefix: "data/CC-MAIN-2024-10/", weight: 50 },
    Dataset { name: "fineweb-older", repo: "HuggingFaceFW/fineweb",
              field: "text", prefix: "data/CC-MAIN-2023-50/", weight: 30 },
    Dataset { name: "redpajama", repo: "togethercomputer/RedPajama-Data-V2",
              field: "raw_content", prefix: "data/", weight: 20 },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dataset {
    pub name: &'static str,
    pub repo: &'static str,
    pub field: &'static str,
    pub prefix: &'static str,
    pub weight: u8,
}

/// Build a reusable operator. Create once per dataset.
///
/// # Errors
///
/// Returns error if HF backend cannot be initialized.
pub fn operator(ds: &Dataset, token: Option<&str>) -> anyhow::Result<Operator> {
    let mut builder = Huggingface::default();
    builder = builder.repo_type("dataset").repo_id(ds.repo);
    if let Some(t) = token {
        builder = builder.token(t);
    }
    let op = Operator::new(builder).context("building operator")?.finish();
    Ok(op.layer(opendal::layers::RetryLayer::new().with_max_times(3)))
}

/// List parquet files via HF tree API (single HTTP call, sub-second).
///
/// # Errors
///
/// Returns error if HF repo is inaccessible.
pub async fn list_files(ds: &Dataset, token: &str) -> anyhow::Result<Vec<String>> {
    let url = format!(
        "https://huggingface.co/api/datasets/{}/tree/main/{}?recursive=true",
        ds.repo, ds.prefix,
    );
    let client = reqwest::Client::new();
    let resp = client.get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .context("listing files")?;

    let body: serde_json::Value = resp.json().await.context("parsing file list")?;
    let mut files = Vec::new();
    if let Some(arr) = body.as_array() {
        for entry in arr {
            if let Some(path) = entry.get("path").and_then(|p| p.as_str()) {
                if path.ends_with(".parquet") {
                    files.push(path.to_owned());
                }
            }
        }
    }
    files.sort();
    Ok(files)
}

/// # Errors
///
/// Returns error on network, parsing, or unsupported column type.
pub async fn stream_file(
    op: &Operator,
    path: &str,
    field: &str,
    counter: &BigramCounter,
) -> anyhow::Result<u64> {
    let meta = op.stat(path).await.context("stat file")?;
    let reader = op
        .reader_with(path)
        .gap(512 * 1024)
        .chunk(16 * 1024 * 1024)
        .concurrent(8)
        .await
        .context("opening reader")?;
    let async_reader = AsyncReader::new(reader, meta.content_length());

    let builder = ParquetRecordBatchStreamBuilder::new(async_reader)
        .await
        .context("reading parquet metadata")?;

    let field_idx = find_field(builder.schema(), field)?;
    let mut stream = builder.with_batch_size(4096).build()
        .context("building record stream")?;

    let mut bytes = 0u64;
    while let Some(batch) = stream.try_next().await.context("reading batch")? {
        bytes += process_column(batch.column(field_idx), counter)?;
    }
    Ok(bytes)
}

fn process_column(col: &dyn Array, counter: &BigramCounter) -> anyhow::Result<u64> {
    let mut bytes = 0u64;

    if let Some(arr) = col.as_any().downcast_ref::<StringArray>() {
        for val in arr.iter().flatten() { count_str(&mut bytes, counter, val); }
    } else if let Some(arr) = col.as_any().downcast_ref::<LargeStringArray>() {
        for val in arr.iter().flatten() { count_str(&mut bytes, counter, val); }
    } else if let Some(arr) = col.as_any().downcast_ref::<BinaryArray>() {
        for val in arr.iter().flatten() { count_bin(&mut bytes, counter, val); }
    } else if let Some(arr) = col.as_any().downcast_ref::<LargeBinaryArray>() {
        for val in arr.iter().flatten() { count_bin(&mut bytes, counter, val); }
    } else {
        bail!("unsupported column type: {:?}", col.data_type());
    }

    Ok(bytes)
}

#[inline]
fn count_str(bytes: &mut u64, counter: &BigramCounter, val: &str) {
    counter.process(val.as_bytes());
    *bytes += val.len() as u64;
}

#[inline]
fn count_bin(bytes: &mut u64, counter: &BigramCounter, val: &[u8]) {
    counter.process(val);
    *bytes += val.len() as u64;
}

const CONTENT_FIELDS: &[&str] = &["content", "text", "raw_content", "body"];

fn find_field(
    schema: &arrow::datatypes::SchemaRef,
    preferred: &str,
) -> anyhow::Result<usize> {
    let names = std::iter::once(preferred).chain(
        CONTENT_FIELDS.iter().copied().filter(|&n| n != preferred),
    );
    for name in names {
        if let Some(idx) = schema.fields().iter().position(|f| f.name() == name) {
            return Ok(idx);
        }
    }
    let available: Vec<_> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    anyhow::bail!("no content field found. Available: {available:?}")
}
