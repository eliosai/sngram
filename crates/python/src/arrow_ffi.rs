//! Arrow `PyCapsule` ingestion for the counting hot path

use arrow_array::ffi::{FFI_ArrowArray, from_ffi};
use arrow_array::ffi_stream::{ArrowArrayStreamReader, FFI_ArrowArrayStream};
use arrow_array::{
    Array, BinaryArray, BinaryViewArray, LargeBinaryArray, LargeStringArray, RecordBatch,
    StringArray, StringViewArray, StructArray,
};
use arrow_schema::ffi::FFI_ArrowSchema;
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyCapsule, PyTuple};

use sngram::learn::BigramCounter;

/// Count all string/binary columns of `data` into `counter`, returning bytes counted
pub fn count_arrow(
    py: Python<'_>,
    data: &Bound<'_, PyAny>,
    counter: &BigramCounter,
) -> PyResult<u64> {
    let before = counter.bytes_processed();
    if data.hasattr("__arrow_c_stream__")? {
        count_stream(py, data, counter)?;
    } else if data.hasattr("__arrow_c_array__")? {
        count_array(py, data, counter)?;
    } else {
        return Err(PyTypeError::new_err(
            "expected an Arrow object (pyarrow Table / RecordBatch / RecordBatchReader) \
             exporting __arrow_c_stream__ or __arrow_c_array__",
        ));
    }
    Ok(counter.bytes_processed() - before)
}

/// Drain a `__arrow_c_stream__` of record batches
fn count_stream(py: Python<'_>, data: &Bound<'_, PyAny>, counter: &BigramCounter) -> PyResult<()> {
    let capsule: Bound<'_, PyCapsule> = data
        .call_method0("__arrow_c_stream__")?
        .cast_into()
        .map_err(|_| PyTypeError::new_err("__arrow_c_stream__ did not return a capsule"))?;

    // SAFETY: from_raw moves the owned stream out of the capsule exactly once
    #[allow(
        unsafe_code,
        reason = "Arrow PyCapsule ownership transfer, per protocol"
    )]
    let mut reader = unsafe {
        ArrowArrayStreamReader::from_raw(
            capsule
                .pointer_checked(Some(c"arrow_array_stream"))?
                .cast::<FFI_ArrowArrayStream>()
                .as_ptr(),
        )
    }
    .map_err(arrow_err)?;

    // pull each batch with the GIL held, count it with the GIL released
    loop {
        let Some(batch) = reader.next().transpose().map_err(arrow_err)? else {
            return Ok(());
        };
        count_batch(py, &batch, counter);
    }
}

/// Consume a single `__arrow_c_array__` (a RecordBatch-shaped struct array)
fn count_array(py: Python<'_>, data: &Bound<'_, PyAny>, counter: &BigramCounter) -> PyResult<()> {
    let pair: Bound<'_, PyTuple> = data
        .call_method0("__arrow_c_array__")?
        .cast_into()
        .map_err(|_| PyTypeError::new_err("__arrow_c_array__ did not return a 2-tuple"))?;
    let schema_capsule: Bound<'_, PyCapsule> = pair.get_item(0)?.cast_into()?;
    let array_capsule: Bound<'_, PyCapsule> = pair.get_item(1)?.cast_into()?;
    let batch = import_batch(&schema_capsule, &array_capsule)?;
    count_batch(py, &batch, counter);
    Ok(())
}

/// Rebuild a record batch from schema and array capsules
fn import_batch(
    schema_capsule: &Bound<'_, PyCapsule>,
    array_capsule: &Bound<'_, PyCapsule>,
) -> PyResult<RecordBatch> {
    // SAFETY: each struct is moved out of its capsule once, leaving a released struct behind
    #[allow(
        unsafe_code,
        reason = "Arrow PyCapsule ownership transfer, per protocol"
    )]
    let array_data = {
        let schema_ptr = schema_capsule
            .pointer_checked(Some(c"arrow_schema"))?
            .cast::<FFI_ArrowSchema>()
            .as_ptr();
        let array_ptr = array_capsule
            .pointer_checked(Some(c"arrow_array"))?
            .cast::<FFI_ArrowArray>()
            .as_ptr();
        let array = unsafe { std::ptr::replace(array_ptr, FFI_ArrowArray::empty()) };
        let schema = unsafe { &*schema_ptr };
        unsafe { from_ffi(array, schema) }.map_err(arrow_err)?
    };
    Ok(RecordBatch::from(StructArray::from(array_data)))
}

/// Count every string/binary column of one batch, GIL released
fn count_batch(py: Python<'_>, batch: &RecordBatch, counter: &BigramCounter) {
    py.detach(|| {
        for col in batch.columns() {
            count_column(col.as_ref(), counter);
        }
    });
}

/// Count one column, recursing through struct children
fn count_column(col: &dyn Array, counter: &BigramCounter) {
    let any = col.as_any();
    if let Some(a) = any.downcast_ref::<StringArray>() {
        counter.process_batch(a.iter().flatten().map(str::as_bytes));
    } else if let Some(a) = any.downcast_ref::<LargeStringArray>() {
        counter.process_batch(a.iter().flatten().map(str::as_bytes));
    } else if let Some(a) = any.downcast_ref::<StringViewArray>() {
        counter.process_batch(a.iter().flatten().map(str::as_bytes));
    } else if let Some(a) = any.downcast_ref::<BinaryArray>() {
        counter.process_batch(a.iter().flatten());
    } else if let Some(a) = any.downcast_ref::<LargeBinaryArray>() {
        counter.process_batch(a.iter().flatten());
    } else if let Some(a) = any.downcast_ref::<BinaryViewArray>() {
        counter.process_batch(a.iter().flatten());
    } else if let Some(s) = any.downcast_ref::<StructArray>() {
        for child in s.columns() {
            count_column(child.as_ref(), counter);
        }
    }
    // non-text columns are ignored
}

fn arrow_err(e: arrow_schema::ArrowError) -> PyErr {
    PyValueError::new_err(format!("arrow error: {e}"))
}
