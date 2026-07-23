//! Python bindings for sngram

#![allow(
    clippy::needless_pass_by_value,
    reason = "pyo3 extracts owned argument values"
)]
#![allow(clippy::missing_const_for_fn, reason = "pymethods cannot be const")]

mod arrow_ffi;
mod counter;
mod plan;
mod scanning;
mod table;

use pyo3::prelude::*;

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<table::PyWeightTable>()?;
    m.add_class::<scanning::PyScanResult>()?;
    m.add_class::<scanning::PyScanSummary>()?;
    m.add_class::<plan::PyQueryPlan>()?;
    m.add_class::<plan::PyScanNeed>()?;
    m.add_class::<counter::PyBigramCounter>()?;
    m.add_function(wrap_pyfunction!(scanning::scan, m)?)?;
    m.add_function(wrap_pyfunction!(plan::query, m)?)?;
    #[cfg(feature = "weights")]
    m.add_function(wrap_pyfunction!(table::weights, m)?)?;
    Ok(())
}
