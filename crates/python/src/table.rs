//! Weight table bindings

use std::cell::RefCell;
use std::path::PathBuf;

use pyo3::exceptions::{PyIOError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use sngram_types::WeightTable;

/// 256x256 byte-pair weight table
#[pyclass(frozen, name = "WeightTable", module = "sngram")]
pub struct PyWeightTable {
    inner: WeightTable,
}

impl PyWeightTable {
    /// Wrap a core table
    pub fn new(inner: WeightTable) -> Self {
        Self { inner }
    }

    /// The wrapped core table
    pub fn inner(&self) -> &WeightTable {
        &self.inner
    }
}

#[pymethods]
impl PyWeightTable {
    /// Load a table from its 262,160-byte SPNG binary
    #[staticmethod]
    fn from_bytes(data: &[u8]) -> PyResult<Self> {
        WeightTable::from_bytes(data)
            .map(Self::new)
            .map_err(|e| PyValueError::new_err(format!("invalid weight table: {e}")))
    }

    /// Load a table from a `.bin` file path
    #[staticmethod]
    fn from_path(path: PathBuf) -> PyResult<Self> {
        let data = std::fs::read(&path)
            .map_err(|e| PyIOError::new_err(format!("reading {}: {e}", path.display())))?;
        Self::from_bytes(&data)
    }

    /// Build a table by calling `weight(c1, c2)` for every byte pair
    #[staticmethod]
    fn from_weight_fn(weight: Bound<'_, PyAny>) -> PyResult<Self> {
        let failure = RefCell::new(None);
        let inner = WeightTable::from_weight_fn(|c1, c2| {
            weight
                .call1((c1, c2))
                .and_then(|value| value.extract())
                .unwrap_or_else(|e| {
                    failure.borrow_mut().get_or_insert(e);
                    0
                })
        });
        failure
            .into_inner()
            .map_or_else(|| Ok(Self::new(inner)), Err)
    }

    /// Serialize the table to its SPNG binary
    fn to_bytes<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.inner.to_bytes())
    }

    /// All 65,536 weights as little-endian u32 bytes, row-major by first byte
    fn matrix<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        let weights = self.inner.matrix();
        let mut out = Vec::with_capacity(weights.len() * 4);
        for weight in weights {
            out.extend_from_slice(&weight.to_le_bytes());
        }
        PyBytes::new(py, &out)
    }

    /// A copy of this table carrying the given provenance note
    fn with_provenance(&self, provenance: &str) -> PyResult<Self> {
        self.inner
            .clone()
            .with_provenance(provenance)
            .map(Self::new)
            .map_err(|e| PyValueError::new_err(format!("invalid provenance: {e}")))
    }

    /// Weight of the byte pair (c1, c2)
    fn weight(&self, c1: u8, c2: u8) -> u32 {
        self.inner.weight(c1, c2)
    }

    /// Table format version
    #[getter]
    fn version(&self) -> u32 {
        self.inner.version()
    }

    /// Stable table identity hash
    #[getter]
    fn fingerprint(&self) -> u64 {
        self.inner.fingerprint()
    }

    /// Provenance note embedded in the table, if any
    #[getter]
    fn provenance(&self) -> Option<String> {
        self.inner.provenance().map(str::to_owned)
    }

    fn __repr__(&self) -> String {
        format!("WeightTable(version={})", self.inner.version())
    }
}

/// The embedded weight table for the enabled tier
#[cfg(feature = "12tb")]
#[pyfunction]
pub fn weights() -> PyWeightTable {
    PyWeightTable::new(sngram::weights())
}
