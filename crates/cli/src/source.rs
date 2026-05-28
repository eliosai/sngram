//! HF dataset definitions and one-file content streaming.

use std::time::Duration;

use anyhow::{Context, bail};
use arrow::array::{Array, BinaryArray, LargeBinaryArray, LargeStringArray, StringArray};
use opendal::Operator;
use opendal::services::Huggingface;
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::ParquetRecordBatchReader;
use parquet::arrow::async_reader::ParquetRecordBatchStreamBuilder;
use parquet_opendal::AsyncReader;

use crate::counter::{BigramCounter, LocalTally};

#[derive(Debug, Clone, Copy)]
pub struct Dataset {
    pub id: &'static str,
    pub repo: &'static str,
    pub field: &'static str,
    pub prefix: &'static str,
    pub langs: &'static [&'static str],
}

const WEB_LANGS: &[&str] = &[
    // top-tier global
    "eng_Latn", "cmn_Hani", "spa_Latn", "ara_Arab", "fra_Latn", "rus_Cyrl", "por_Latn",
    "deu_Latn", "jpn_Jpan", "ita_Latn", "kor_Hang", "tur_Latn", "vie_Latn", "pol_Latn",
    "nld_Latn", "ind_Latn", "fas_Arab", "ukr_Cyrl", "ces_Latn", "swe_Latn", "ron_Latn",
    "hun_Latn", "ell_Grek", "dan_Latn", "fin_Latn", "tha_Thai", "heb_Hebr", "nob_Latn",
    // South Asian
    "hin_Deva", "ben_Beng", "tam_Taml", "tel_Telu", "mar_Deva", "guj_Gujr", "kan_Knda",
    "mal_Mlym", "pan_Guru", "sin_Sinh", "urd_Arab", "npi_Deva", "asm_Beng", "ory_Orya",
    // SE Asian
    "msa_Latn", "jav_Latn", "sun_Latn", "tgl_Latn", "ceb_Latn", "khm_Khmr", "mya_Mymr",
    "lao_Laoo",
    // East Asian extras
    "yue_Hant",
    // European (more)
    "slk_Latn", "bul_Cyrl", "srp_Cyrl", "hrv_Latn", "bos_Latn", "slv_Latn", "lit_Latn",
    "lvs_Latn", "est_Latn", "isl_Latn", "cat_Latn", "glg_Latn", "eus_Latn", "gle_Latn",
    "cym_Latn", "mlt_Latn", "sqi_Latn", "mkd_Cyrl", "bel_Cyrl", "afr_Latn",
    // Caucasus / Central Asia / Middle East extras
    "kat_Geor", "hye_Armn", "aze_Latn", "kaz_Cyrl", "uzn_Latn", "kir_Cyrl", "tgk_Cyrl",
    "pus_Arab", "ckb_Arab",
    // African
    "swh_Latn", "hau_Latn", "yor_Latn", "ibo_Latn", "amh_Ethi", "zul_Latn", "xho_Latn",
    "som_Latn", "sna_Latn",
    // misc
    "lat_Latn", "epo_Latn",
];

pub const DATASETS: &[Dataset] = &[
    Dataset { id: "the-stack",     repo: "bigcode/the-stack",      field: "content", prefix: "data/", langs: &[] },
    Dataset { id: "finepdfs",      repo: "HuggingFaceFW/finepdfs", field: "text",    prefix: "data/", langs: WEB_LANGS },
    Dataset { id: "fineweb-2",     repo: "HuggingFaceFW/fineweb-2",field: "text",    prefix: "data/", langs: WEB_LANGS },
    Dataset { id: "starcoderdata", repo: "bigcode/starcoderdata",  field: "content", prefix: "",      langs: &[] },
    Dataset { id: "github-code",   repo: "codeparrot/github-code", field: "code",    prefix: "data/", langs: &[] },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParquetFile {
    pub path: String,
    pub size: u64,
}

pub fn hf_operator(repo: &str, token: Option<&str>) -> anyhow::Result<Operator> {
    let mut builder = Huggingface::default().repo_type("dataset").repo_id(repo);
    if let Some(t) = token {
        builder = builder.token(t);
    }
    let op = Operator::new(builder).context("building operator")?.finish();
    let retry = opendal::layers::RetryLayer::new()
        .with_max_times(5)
        .with_factor(2.0)
        .with_max_delay(Duration::from_secs(30))
        .with_jitter();
    Ok(op.layer(retry))
}

pub async fn list_files(op: &Operator, prefix: &str) -> anyhow::Result<Vec<ParquetFile>> {
    let entries = op.list_with(prefix).recursive(true).await.context("listing")?;
    let mut files: Vec<ParquetFile> = entries
        .into_iter()
        .filter(|e| e.path().ends_with(".parquet"))
        .map(|e| ParquetFile { path: e.path().to_owned(), size: e.metadata().content_length() })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

#[must_use]
pub fn is_transient(e: &anyhow::Error) -> bool {
    let s = format!("{e:#}");
    s.contains("429")
        || s.contains("RateLimited")
        || s.contains("temporar")
        || s.contains("Too Many")
        || s.contains("timeout")
        || s.contains("timed out")
        || s.contains("connection reset")
        || s.contains("broken pipe")
}

#[must_use]
pub fn is_not_found(e: &anyhow::Error) -> bool {
    let s = format!("{e:#}");
    s.contains("NotFound") || s.contains("404") || s.contains("not found")
}

pub async fn count_file(
    op: &Operator,
    file: &ParquetFile,
    field: &str,
    counter: &BigramCounter,
) -> anyhow::Result<u64> {
    let mut stream = open_content_stream(op, file, field).await?;
    let mut tally = LocalTally::new();
    while let Some(rg) = stream.next_row_group().await.context("fetching row group")? {
        decode_row_group(rg, &mut tally)?;
    }
    let bytes = tally.bytes();
    counter.merge(&tally);
    counter.inc_files(1);
    counter.add_downloaded(file.size);
    Ok(bytes)
}

type FileStream = parquet::arrow::async_reader::ParquetRecordBatchStream<AsyncReader>;

async fn open_content_stream(
    op: &Operator,
    file: &ParquetFile,
    field: &str,
) -> anyhow::Result<FileStream> {
    let reader = op
        .reader_with(&file.path)
        .gap(4 * 1024 * 1024)
        .chunk(16 * 1024 * 1024)
        .concurrent(4)
        .await
        .context("opening reader")?;
    let builder = ParquetRecordBatchStreamBuilder::new(AsyncReader::new(reader, file.size))
        .await
        .context("reading parquet metadata")?;
    let field_idx = find_field(builder.schema(), field)?;
    let mask = ProjectionMask::roots(
        builder.metadata().file_metadata().schema_descr(),
        [field_idx],
    );
    builder.with_projection(mask).with_batch_size(65_536).build()
        .context("building record stream")
}

fn decode_row_group(
    mut reader: ParquetRecordBatchReader,
    tally: &mut LocalTally,
) -> anyhow::Result<()> {
    reader.try_for_each(|batch| {
        let batch = batch.context("decoding batch")?;
        count_column(batch.column(0), tally)
    })
}

fn count_column(col: &dyn Array, tally: &mut LocalTally) -> anyhow::Result<()> {
    if let Some(arr) = col.as_any().downcast_ref::<StringArray>() {
        for v in arr.iter().flatten() { tally.count_buffer(v.as_bytes()); }
    } else if let Some(arr) = col.as_any().downcast_ref::<LargeStringArray>() {
        for v in arr.iter().flatten() { tally.count_buffer(v.as_bytes()); }
    } else if let Some(arr) = col.as_any().downcast_ref::<BinaryArray>() {
        for v in arr.iter().flatten() { tally.count_buffer(v); }
    } else if let Some(arr) = col.as_any().downcast_ref::<LargeBinaryArray>() {
        for v in arr.iter().flatten() { tally.count_buffer(v); }
    } else {
        bail!("unsupported column type: {:?}", col.data_type());
    }
    Ok(())
}

const CONTENT_FIELDS: &[&str] = &["content", "text", "code", "raw_content", "body"];

fn find_field(schema: &arrow::datatypes::SchemaRef, preferred: &str) -> anyhow::Result<usize> {
    let names = std::iter::once(preferred)
        .chain(CONTENT_FIELDS.iter().copied().filter(|&n| n != preferred));
    for name in names {
        if let Some(idx) = schema.fields().iter().position(|f| f.name() == name) {
            return Ok(idx);
        }
    }
    let available: Vec<_> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    bail!("no content field found. Available: {available:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_ids_are_unique() {
        let mut ids: Vec<_> = DATASETS.iter().map(|d| d.id).collect();
        ids.sort_unstable();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n, "dataset ids must be unique");
    }

    #[test]
    fn datasets_in_expected_order() {
        let order: Vec<_> = DATASETS.iter().map(|d| d.id).collect();
        assert_eq!(order, vec!["the-stack", "finepdfs", "fineweb-2", "starcoderdata", "github-code"]);
    }

    #[test]
    fn is_transient_recognizes_rate_limits() {
        assert!(is_transient(&anyhow::anyhow!("HTTP 429 Too Many Requests")));
        assert!(is_transient(&anyhow::anyhow!("connection reset by peer")));
        assert!(!is_transient(&anyhow::anyhow!("403 Forbidden")));
    }
}
