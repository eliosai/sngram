//! Sparse n-gram extraction for the standard index format.

use std::io::BufRead;

pub mod cover;
mod engine;
mod facts;
pub mod settings;
mod space;
mod validate;

use sngram_types::{ScanError, ScanEvent, WeightTable};
#[cfg(feature = "stream")]
use tokio::io::AsyncBufRead;

/// Incremental scan for content classified by the caller as text
pub struct TextScanner<'t> {
    scanner: engine::DocumentScanner<'t>,
    started: bool,
}

impl<'t> TextScanner<'t> {
    /// Open an incremental text scan
    #[must_use]
    pub fn new(table: &'t WeightTable) -> Self {
        Self {
            scanner: engine::DocumentScanner::new(table),
            started: false,
        }
    }

    /// Scan one contiguous content chunk
    pub fn push(&mut self, chunk: &[u8], mut emit: impl for<'event> FnMut(ScanEvent<'event>)) {
        self.begin(&mut emit);
        self.scanner.push_content(chunk, &mut emit);
    }

    /// Finish the content stream and emit its final facts
    pub fn finish(mut self, mut emit: impl for<'event> FnMut(ScanEvent<'event>)) {
        self.begin(&mut emit);
        self.scanner.finish_document(&mut emit);
    }

    fn begin(&mut self, emit: &mut impl for<'event> FnMut(ScanEvent<'event>)) {
        if !self.started {
            self.scanner.begin_document(emit);
            self.started = true;
        }
    }
}

/// Extract sparse n-grams and scan metadata from one byte stream.
///
/// The scanner reads the input once, emits raw gram keys plus case-folded
/// supplement keys when needed, and brackets the document with virtual line
/// sentinels so anchored patterns can be planned against boundary grams.
///
/// # Errors
///
/// Returns [`ScanError::Io`] when reading from `input` fails, or
/// [`ScanError::Binary`] when the leading content sample is rejected as binary.
pub fn scan<R>(
    table: &WeightTable,
    input: R,
    mut emit: impl for<'event> FnMut(ScanEvent<'event>),
) -> Result<(), ScanError>
where
    R: BufRead,
{
    let validated = validate::read(input)?;
    let mut scanner = engine::DocumentScanner::new(table);
    scanner.begin_document(&mut emit);
    scanner.push_content(validated.prefix().bytes(), &mut emit);
    let mut input = validated.into_input();
    loop {
        let chunk = input.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        let len = chunk.len();
        scanner.push_content(chunk, &mut emit);
        input.consume(len);
    }
    scanner.finish_document(&mut emit);
    Ok(())
}

/// Extract sparse n-grams and scan metadata from one asynchronous byte stream.
///
/// # Errors
///
/// Returns [`ScanError::Io`] when reading from `input` fails, or
/// [`ScanError::Binary`] when the leading content sample is rejected as binary.
#[cfg(feature = "stream")]
pub async fn scan_async<R>(
    table: &WeightTable,
    input: R,
    mut emit: impl for<'event> FnMut(ScanEvent<'event>),
) -> Result<(), ScanError>
where
    R: AsyncBufRead + Unpin,
{
    let validated = validate::read_async(input).await?;
    let mut scanner = engine::DocumentScanner::new(table);
    scanner.begin_document(&mut emit);
    scanner.push_content(validated.prefix().bytes(), &mut emit);
    let mut input = validated.into_input();
    loop {
        let chunk = tokio::io::AsyncBufReadExt::fill_buf(&mut input).await?;
        if chunk.is_empty() {
            break;
        }
        let len = chunk.len();
        scanner.push_content(chunk, &mut emit);
        tokio::io::AsyncBufReadExt::consume(&mut input, len);
    }
    scanner.finish_document(&mut emit);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "stream")]
    use std::future::Future;
    use std::io::Cursor;
    #[cfg(feature = "stream")]
    use std::pin::Pin;
    #[cfg(feature = "stream")]
    use std::task::{Context, Poll};

    use sngram_types::{ScanError, ScanEvent, ScanSummary, WeightTable};

    use super::scan;

    fn table() -> WeightTable {
        WeightTable::from_weight_fn(|c1, c2| crc32fast::hash(&[c1, c2]))
    }

    #[test]
    fn binary_input_is_rejected_before_any_event() {
        let mut events = 0usize;
        let err = scan(&table(), Cursor::new(b"\x7fELF\x00\x00\x00rest"), |_| {
            events += 1;
        })
        .unwrap_err();

        assert!(matches!(err, ScanError::Binary));
        assert_eq!(events, 0);
    }

    #[cfg(feature = "stream")]
    #[test]
    fn async_scan_matches_sync_across_reader_chunks() {
        run(async {
            let input = b"fn Max_file_size() -> u64 { 0 }\n".repeat(300);
            let expected = collect_sync(&input);
            for capacity in [1, 2, 127, 8191, 8192, 8193] {
                let reader = tokio::io::BufReader::with_capacity(capacity, Cursor::new(&input));
                let actual = collect_async(reader).await;
                assert_eq!(actual, expected, "reader capacity {capacity}");
            }
        });
    }

    #[test]
    fn externally_classified_text_scans_incremental_chunks() {
        let table = table();
        let input = b"fn Max_file_size() -> u64 { 0 }\n";
        let expected = collect_sync(input);
        let mut grams = Vec::new();
        let mut summary = None;
        let mut scanner = super::TextScanner::new(&table);

        scanner.push(&input[..7], |event| {
            collect(event, &mut grams, &mut summary);
        });
        scanner.push(&input[7..], |event| {
            collect(event, &mut grams, &mut summary);
        });
        scanner.finish(|event| collect(event, &mut grams, &mut summary));
        grams.sort_unstable();

        assert_eq!((grams, summary), expected);
    }

    #[test]
    fn externally_classified_text_does_not_apply_binary_policy() {
        let table = table();
        let mut summary = None;
        let mut scanner = super::TextScanner::new(&table);

        scanner.push(b"text\0tail", |event| {
            if let ScanEvent::Finish(done) = event {
                summary = Some(*done);
            }
        });
        scanner.finish(|event| {
            if let ScanEvent::Finish(done) = event {
                summary = Some(*done);
            }
        });

        assert_eq!(summary.expect("scan summary").byte_len, 9);
    }

    #[cfg(feature = "stream")]
    #[test]
    fn async_binary_input_is_rejected_before_any_event() {
        run(async {
            let table = table();
            let input = tokio::io::BufReader::new(Cursor::new(b"\x7fELF\x00\x00\x00rest"));
            let mut events = 0usize;
            let error = super::scan_async(&table, input, |_| events += 1)
                .await
                .expect_err("binary input is rejected");
            assert!(matches!(error, ScanError::Binary));
            assert_eq!(events, 0);
        });
    }

    #[cfg(feature = "stream")]
    #[test]
    fn async_read_error_returns_without_a_summary() {
        run(async {
            let table = table();
            let mut summaries = 0usize;
            let error = super::scan_async(&table, FailingAsyncReader, |event| {
                summaries += usize::from(matches!(event, ScanEvent::Finish(_)));
            })
            .await
            .expect_err("read error surfaces");
            assert!(matches!(error, ScanError::Io(_)));
            assert_eq!(summaries, 0);
        });
    }

    #[cfg(feature = "stream")]
    fn run(future: impl Future<Output = ()>) {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime")
            .block_on(future);
    }

    #[cfg(feature = "stream")]
    async fn collect_async(
        input: impl tokio::io::AsyncBufRead + Unpin,
    ) -> (Vec<u64>, Option<ScanSummary>) {
        let table = table();
        let mut grams = Vec::new();
        let mut summary = None;
        super::scan_async(&table, input, |event| {
            collect(event, &mut grams, &mut summary);
        })
        .await
        .expect("async scan succeeds");
        grams.sort_unstable();
        (grams, summary)
    }

    fn collect_sync(input: &[u8]) -> (Vec<u64>, Option<ScanSummary>) {
        let table = table();
        let mut grams = Vec::new();
        let mut summary = None;
        scan(&table, Cursor::new(input), |event| {
            collect(event, &mut grams, &mut summary);
        })
        .expect("sync scan succeeds");
        grams.sort_unstable();
        (grams, summary)
    }

    fn collect(event: ScanEvent<'_>, grams: &mut Vec<u64>, summary: &mut Option<ScanSummary>) {
        match event {
            ScanEvent::Gram(gram) => grams.push(gram.key.value()),
            ScanEvent::Finish(done) => *summary = Some(*done),
        }
    }

    #[cfg(feature = "stream")]
    struct FailingAsyncReader;

    #[cfg(feature = "stream")]
    impl tokio::io::AsyncRead for FailingAsyncReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Err(std::io::Error::other("boom")))
        }
    }

    #[cfg(feature = "stream")]
    impl tokio::io::AsyncBufRead for FailingAsyncReader {
        fn poll_fill_buf(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<&[u8]>> {
            Poll::Ready(Err(std::io::Error::other("boom")))
        }

        fn consume(self: Pin<&mut Self>, _amount: usize) {}
    }
}
