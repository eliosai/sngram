//! Tantivy mmap-backed index backend.

// Planned path:
// - Rayon reads files and extracts sparse n-grams in parallel.
// - A single Tantivy IndexWriter owns segment creation and commits.
// - Search opens Tantivy's mmap-backed directory and collects doc ords.
