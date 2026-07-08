//! Training counter bindings

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use sngram::learn::BigramCounter;

use crate::arrow_ffi;

/// Shared, lock-free byte-pair counter: the training accumulator
#[pyclass(frozen, name = "BigramCounter", module = "sngram")]
pub struct PyBigramCounter {
    inner: BigramCounter,
}

#[pymethods]
impl PyBigramCounter {
    #[new]
    fn new() -> Self {
        Self {
            inner: BigramCounter::new(),
        }
    }

    /// Fold a completed staging counter into this counter, thread-safe
    fn merge(&self, py: Python<'_>, other: &Self) {
        let inner = &self.inner;
        let staged = &other.inner;
        py.detach(|| inner.merge(staged));
    }

    /// Count one document directly
    fn process(&self, py: Python<'_>, data: &[u8]) {
        let inner = &self.inner;
        py.detach(|| inner.process(data));
    }

    /// Count every row of an Arrow object, per row, GIL-free
    ///
    /// Accepts anything exporting the Arrow `PyCapsule` interface with a
    /// struct/record-batch schema: a `pyarrow.Table`, `RecordBatch`, or
    /// `RecordBatchReader`. All string/binary columns are counted; nulls are
    /// skipped. Returns the number of text bytes counted.
    fn count_arrow(&self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<u64> {
        arrow_ffi::count_arrow(py, data, &self.inner)
    }

    /// Current count for one byte pair
    fn count(&self, c1: u8, c2: u8) -> u64 {
        self.inner.count(c1, c2)
    }

    /// Record `n` completed files or shards
    fn add_files(&self, n: u64) {
        self.inner.add_files(n);
    }

    /// Total byte pairs counted
    #[getter]
    fn pairs_processed(&self) -> u64 {
        self.inner.pairs_processed()
    }

    /// Total text bytes counted
    #[getter]
    fn bytes_processed(&self) -> u64 {
        self.inner.bytes_processed()
    }

    /// Total files recorded complete
    #[getter]
    fn files_processed(&self) -> u64 {
        self.inner.files_processed()
    }

    /// All 65,536 pair counts as little-endian u64 bytes, for checkpointing
    fn snapshot<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.inner.snapshot())
    }

    /// Restore a checkpoint: `snapshot()` bytes plus the saved totals
    fn restore(&self, counts: &[u8], pairs: u64, bytes: u64, files: u64) -> PyResult<()> {
        self.inner
            .restore(counts, pairs, bytes, files)
            .map_err(|e| PyValueError::new_err(e.to_string()))
    }

    /// Serialize the learned weight table as SPNG `.bin` bytes
    fn to_table_bytes<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        let inner = &self.inner;
        let bytes = py.detach(|| inner.to_table_bytes());
        PyBytes::new(py, &bytes)
    }

    fn __repr__(&self) -> String {
        format!(
            "BigramCounter(pairs={}, bytes={}, files={})",
            self.inner.pairs_processed(),
            self.inner.bytes_processed(),
            self.inner.files_processed()
        )
    }
}
