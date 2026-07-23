//! Scan bindings: sparse grams and per-entry summaries

use std::io::Cursor;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use sngram_types::{ScanEvent, ScanSummary};

use crate::table::PyWeightTable;

type GramTriple = (usize, usize, u64);

/// Final scan-derived metadata for one indexed text entry
#[pyclass(frozen, name = "ScanSummary", module = "sngram")]
pub struct PyScanSummary {
    inner: ScanSummary,
}

impl PyScanSummary {
    /// The wrapped core summary
    pub fn inner(&self) -> &ScanSummary {
        &self.inner
    }
}

#[pymethods]
impl PyScanSummary {
    /// Original content length in bytes
    #[getter]
    fn byte_len(&self) -> u64 {
        self.inner.byte_len
    }

    /// Number of text lines observed
    #[getter]
    fn line_count(&self) -> u32 {
        self.inner.line_count
    }

    /// Number of empty lines observed
    #[getter]
    fn empty_line_count(&self) -> u32 {
        self.inner.empty_line_count
    }

    /// Longest line length in bytes, excluding the newline
    #[getter]
    fn longest_line_len(&self) -> u32 {
        self.inner.longest_line_len
    }

    /// Number of gram keys emitted
    #[getter]
    fn gram_count(&self) -> u32 {
        self.inner.gram_count
    }

    /// First bytes of the content
    #[getter]
    fn prefix<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, self.inner.prefix.as_slice())
    }

    /// Last bytes of the content
    #[getter]
    fn suffix<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, self.inner.suffix.as_slice())
    }

    fn __repr__(&self) -> String {
        format!(
            "ScanSummary(byte_len={}, line_count={}, gram_count={})",
            self.inner.byte_len, self.inner.line_count, self.inner.gram_count
        )
    }
}

/// One scanned entry: sparse grams plus the final summary
#[pyclass(frozen, name = "ScanResult", module = "sngram")]
pub struct PyScanResult {
    #[pyo3(get)]
    grams: Vec<GramTriple>,
    summary: Py<PyScanSummary>,
}

#[pymethods]
impl PyScanResult {
    /// Scan-derived metadata for the whole entry
    #[getter]
    fn summary(&self, py: Python<'_>) -> Py<PyScanSummary> {
        self.summary.clone_ref(py)
    }

    /// Gram index keys as little-endian u64 bytes, one per gram
    ///
    /// Zero-copy view from Python: `np.frombuffer(buf, dtype=np.uint64)`.
    fn key_bytes<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        let mut out = Vec::with_capacity(self.grams.len() * 8);
        for &(_, _, key) in &self.grams {
            out.extend_from_slice(&key.to_le_bytes());
        }
        PyBytes::new(py, &out)
    }

    fn __repr__(&self) -> String {
        format!("ScanResult(grams={})", self.grams.len())
    }
}

/// Sparse grams of `data` as `(content_start, content_end, key)` plus a summary
#[pyfunction]
pub fn scan(py: Python<'_>, table: &PyWeightTable, data: &[u8]) -> PyResult<PyScanResult> {
    let (grams, summary) = collect_scan(py, table, data)?;
    Ok(PyScanResult {
        grams,
        summary: Py::new(py, PyScanSummary { inner: summary })?,
    })
}

fn collect_scan(
    py: Python<'_>,
    table: &PyWeightTable,
    data: &[u8],
) -> PyResult<(Vec<GramTriple>, ScanSummary)> {
    let table = table.inner();
    py.detach(|| {
        let mut grams = Vec::new();
        let mut summary = None;
        sngram::scan(table, Cursor::new(data), |event| match event {
            ScanEvent::Gram(gram) => grams.push((gram.span.start, gram.span.end, gram.key.value())),
            ScanEvent::Finish(finish) => summary = Some(*finish),
        })
        .map_err(|e| PyRuntimeError::new_err(format!("scan failed: {e}")))?;
        summary
            .map(|summary| (grams, summary))
            .ok_or_else(|| PyRuntimeError::new_err("scan emitted no summary"))
    })
}
