//! Arrow `PyCapsule` ingestion for the counting hot path.
//!
//! Accepts any Python object exporting the Arrow C data interface with a
//! record-batch (struct) schema — `pyarrow.Table`, `RecordBatch`,
//! `RecordBatchReader` — and counts every string/binary value per row.
//! The only `unsafe` in the bindings lives here: taking ownership of the
//! C structs the capsules carry, exactly as the `PyCapsule` protocol specifies.

use arrow_array::ffi::{FFI_ArrowArray, from_ffi};
use arrow_array::ffi_stream::{ArrowArrayStreamReader, FFI_ArrowArrayStream};
use arrow_array::{Array, RecordBatch, StructArray};
use arrow_schema::ffi::FFI_ArrowSchema;
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyCapsule, PyTuple};

use sngram::learn::LocalTally;

/// Count all string/binary columns of `data` into `tally`, per row.
/// Returns the number of text bytes counted by this call.
pub fn count_arrow(
    py: Python<'_>,
    data: &Bound<'_, PyAny>,
    tally: &mut LocalTally,
) -> PyResult<u64> {
    let before = tally.bytes();
    if data.hasattr("__arrow_c_stream__")? {
        count_stream(py, data, tally)?;
    } else if data.hasattr("__arrow_c_array__")? {
        count_array(py, data, tally)?;
    } else {
        return Err(PyTypeError::new_err(
            "expected an Arrow object (pyarrow Table / RecordBatch / RecordBatchReader) \
             exporting __arrow_c_stream__ or __arrow_c_array__",
        ));
    }
    Ok(tally.bytes() - before)
}

/// Drain a `__arrow_c_stream__` of record batches.
fn count_stream(py: Python<'_>, data: &Bound<'_, PyAny>, tally: &mut LocalTally) -> PyResult<()> {
    let capsule: Bound<'_, PyCapsule> = data
        .call_method0("__arrow_c_stream__")?
        .cast_into()
        .map_err(|_| PyTypeError::new_err("__arrow_c_stream__ did not return a capsule"))?;

    // SAFETY: the capsule contract hands us an owned FFI_ArrowArrayStream;
    // from_raw moves it out and leaves a released struct for the capsule's
    // destructor, so it is consumed exactly once.
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

    // pull each batch with the GIL held (cheap slicing on the producer side),
    // count it with the GIL released (the heavy part)
    loop {
        let Some(batch) = reader.next().transpose().map_err(arrow_err)? else {
            return Ok(());
        };
        count_batch(py, &batch, tally);
    }
}

/// Consume a single `__arrow_c_array__` (a RecordBatch-shaped struct array).
fn count_array(py: Python<'_>, data: &Bound<'_, PyAny>, tally: &mut LocalTally) -> PyResult<()> {
    let pair: Bound<'_, PyTuple> = data
        .call_method0("__arrow_c_array__")?
        .cast_into()
        .map_err(|_| PyTypeError::new_err("__arrow_c_array__ did not return a 2-tuple"))?;
    let schema_capsule: Bound<'_, PyCapsule> = pair.get_item(0)?.cast_into()?;
    let array_capsule: Bound<'_, PyCapsule> = pair.get_item(1)?.cast_into()?;

    // SAFETY: same ownership contract as the stream path — each struct is
    // moved out of its capsule once, leaving an empty/released struct behind.
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

    let strukt = StructArray::from(array_data);
    let batch = RecordBatch::from(strukt);
    count_batch(py, &batch, tally);
    Ok(())
}

/// Count every string/binary column of one batch, GIL released.
fn count_batch(py: Python<'_>, batch: &RecordBatch, tally: &mut LocalTally) {
    py.detach(|| {
        for col in batch.columns() {
            count_column(col.as_ref(), tally);
        }
    });
}

#[allow(
    clippy::too_many_lines,
    reason = "one arm per supported arrow text type"
)]
fn count_column(col: &dyn Array, tally: &mut LocalTally) {
    use arrow_array::{
        BinaryArray, BinaryViewArray, LargeBinaryArray, LargeStringArray, StringArray,
        StringViewArray,
    };
    let any = col.as_any();
    if let Some(a) = any.downcast_ref::<StringArray>() {
        for v in a.iter().flatten() {
            tally.count_buffer(v.as_bytes());
        }
    } else if let Some(a) = any.downcast_ref::<LargeStringArray>() {
        for v in a.iter().flatten() {
            tally.count_buffer(v.as_bytes());
        }
    } else if let Some(a) = any.downcast_ref::<StringViewArray>() {
        for v in a.iter().flatten() {
            tally.count_buffer(v.as_bytes());
        }
    } else if let Some(a) = any.downcast_ref::<BinaryArray>() {
        for v in a.iter().flatten() {
            tally.count_buffer(v);
        }
    } else if let Some(a) = any.downcast_ref::<LargeBinaryArray>() {
        for v in a.iter().flatten() {
            tally.count_buffer(v);
        }
    } else if let Some(a) = any.downcast_ref::<BinaryViewArray>() {
        for v in a.iter().flatten() {
            tally.count_buffer(v);
        }
    } else if let Some(s) = any.downcast_ref::<StructArray>() {
        for child in s.columns() {
            count_column(child.as_ref(), tally);
        }
    }
    // non-text columns are ignored: the trainer projects to the text column,
    // and counting numbers would poison the table
}

#[allow(clippy::needless_pass_by_value, reason = "map_err adapter")]
fn arrow_err(e: arrow_schema::ArrowError) -> PyErr {
    PyValueError::new_err(format!("arrow error: {e}"))
}
