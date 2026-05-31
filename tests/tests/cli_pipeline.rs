use std::path::Path;
use std::sync::Arc;

use arrow::array::{ArrayRef, Int64Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use opendal::Operator;
use opendal::services::Fs;
use parquet::arrow::ArrowWriter;

use sngram_cli::counter::BigramCounter;
use sngram_cli::mint::mint;
use sngram_cli::source::{count_file, list_files};
use sngram_types::WeightTable;

fn write_parquet(path: &Path, content: &[&str], junk: &[&str]) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("content", DataType::Utf8, false),
        Field::new("junk", DataType::Utf8, false),
    ]));
    let ids: Vec<i64> = (0..content.len() as i64).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(Int64Array::from(ids)) as ArrayRef,
            Arc::new(StringArray::from(content.to_vec())) as ArrayRef,
            Arc::new(StringArray::from(junk.to_vec())) as ArrayRef,
        ],
    )
    .unwrap();
    let f = std::fs::File::create(path).unwrap();
    let mut w = ArrowWriter::try_new(f, schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn fs_op(root: &Path) -> Operator {
    Operator::new(Fs::default().root(root.to_str().unwrap()))
        .unwrap()
        .finish()
}

async fn list_with_sizes(
    op: &Operator,
    dir: &Path,
    prefix: &str,
) -> Vec<sngram_cli::source::ParquetFile> {
    let mut files = list_files(op, prefix).await.unwrap();
    for f in &mut files {
        if f.size == 0 {
            f.size = std::fs::metadata(dir.join(&f.path)).unwrap().len();
        }
    }
    files
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn count_file_reads_only_content_column() {
    let dir = tempfile::tempdir().unwrap();
    write_parquet(&dir.path().join("f.parquet"), &["abcabc"], &["ZZZZZZ"]);
    let op = fs_op(dir.path());
    let files = list_with_sizes(&op, dir.path(), "").await;
    let counter = BigramCounter::new();
    let bytes = count_file(&op, &files[0], "content", &counter)
        .await
        .unwrap();
    assert!(bytes > 0, "should report counted bytes");
    assert!(counter.count(b'a', b'b') > 0, "content column counted");
    assert_eq!(counter.count(b'Z', b'Z'), 0, "junk column NOT counted");
    assert_eq!(counter.files_processed(), 1, "file counted once");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn count_file_does_not_straddle_row_boundary() {
    let dir = tempfile::tempdir().unwrap();
    write_parquet(
        &dir.path().join("f.parquet"),
        &["fn main() {}", "let x = 42;"],
        &["j", "j"],
    );
    let op = fs_op(dir.path());
    let files = list_with_sizes(&op, dir.path(), "").await;
    let counter = BigramCounter::new();
    count_file(&op, &files[0], "content", &counter)
        .await
        .unwrap();
    assert!(counter.count(b'f', b'n') > 0);
    assert!(counter.count(b'4', b'2') > 0);
    assert_eq!(
        counter.count(b';', b'l'),
        0,
        "no bigram across row boundary"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mint_writes_valid_weight_table() {
    let dir = tempfile::tempdir().unwrap();
    let counter = BigramCounter::new();
    for _ in 0..100 {
        counter.process(b"the quick brown fox");
    }
    counter.process(b"zqzqzq");
    let path = mint(&counter, dir.path(), "test").unwrap();
    assert!(path.exists());
    let bytes = std::fs::read(&path).unwrap();
    let table = WeightTable::from_bytes(&bytes).unwrap();
    let common = table.weight(b't', b'h');
    let rare = table.weight(b'z', b'q');
    assert!(rare > common, "rare={rare} should be > common={common}");
}
