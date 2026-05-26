//! Canonical HF dataset streaming via opendal + parquet.

use anyhow::Context;
use arrow::array::{Array, StringArray, BinaryArray};
use futures::TryStreamExt;
use opendal::Operator;
use opendal::services::Huggingface;
use parquet::arrow::ParquetRecordBatchStreamBuilder;
use parquet_opendal::AsyncReader;

use crate::counter::BigramCounter;

pub const DATASETS: &[Dataset] = &[
    Dataset {
        name: "the-stack-v2",
        repo: "bigcode/the-stack-v2-dedup",
        field: "content",
        prefix: "data/",
        weight: 50,
    },
    Dataset {
        name: "fineweb-2",
        repo: "HuggingFaceFW/fineweb-2",
        field: "text",
        prefix: "data/",
        weight: 30,
    },
    Dataset {
        name: "redpajama",
        repo: "togethercomputer/RedPajama-Data-V2",
        field: "raw_content",
        prefix: "data/",
        weight: 20,
    },
];

pub struct Dataset {
    pub name: &'static str,
    pub repo: &'static str,
    pub field: &'static str,
    pub prefix: &'static str,
    pub weight: u8,
}

/// # Errors
///
/// Returns error if HF repo is inaccessible.
pub async fn list_files(ds: &Dataset, token: Option<&str>) -> anyhow::Result<Vec<String>> {
    let op = build_operator(ds.repo, token)?;
    let entries = op
        .list(ds.prefix)
        .await
        .context("listing dataset files")?;

    let mut files: Vec<String> = entries
        .into_iter()
        .filter(|e| e.path().ends_with(".parquet"))
        .map(|e| e.path().to_owned())
        .collect();

    files.sort();
    Ok(files)
}

/// # Errors
///
/// Returns error on network or parsing failure.
pub async fn stream_file(
    ds: &Dataset,
    path: &str,
    token: Option<&str>,
    counter: &BigramCounter,
) -> anyhow::Result<u64> {
    let op = build_operator(ds.repo, token)?;
    let meta = op.stat(path).await.context("stat parquet file")?;
    let or = op.reader(path).await.context("opening reader")?;
    let reader = AsyncReader::new(or, meta.content_length());

    let builder = ParquetRecordBatchStreamBuilder::new(reader)
        .await
        .context("reading parquet metadata")?;

    let field_idx = find_field(builder.schema(), ds.field)?;
    let mut stream = builder.with_batch_size(4096).build()
        .context("building record stream")?;

    let mut bytes_streamed = 0u64;
    while let Some(batch) = stream.try_next().await.context("reading batch")? {
        bytes_streamed += process_column(batch.column(field_idx), counter);
    }

    Ok(bytes_streamed)
}

fn process_column(col: &dyn Array, counter: &BigramCounter) -> u64 {
    let mut bytes = 0u64;
    if let Some(arr) = col.as_any().downcast_ref::<StringArray>() {
        for val in arr.iter().flatten() {
            counter.process(val.as_bytes());
            bytes += val.len() as u64;
        }
    } else if let Some(arr) = col.as_any().downcast_ref::<BinaryArray>() {
        for val in arr.iter().flatten() {
            counter.process(val);
            bytes += val.len() as u64;
        }
    }
    bytes
}

fn build_operator(repo: &str, token: Option<&str>) -> anyhow::Result<Operator> {
    let mut builder = Huggingface::default();
    builder = builder.repo_type("dataset").repo_id(repo);
    if let Some(t) = token {
        builder = builder.token(t);
    }
    let op = Operator::new(builder)
        .context("building HF operator")?
        .finish();
    Ok(op.layer(opendal::layers::RetryLayer::new().with_max_times(3)))
}

fn find_field(schema: &arrow::datatypes::SchemaRef, name: &str) -> anyhow::Result<usize> {
    schema.fields()
        .iter()
        .position(|f| f.name() == name)
        .with_context(|| format!("field '{name}' not found in schema"))
}
