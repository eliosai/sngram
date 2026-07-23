"""End-to-end soundness: a pure-python inverted index never misses a match."""

import re

import pytest

import sngram


DOCS = [
    b"fn main() { println!(\"hello world\"); }",
    b"pub async fn read_content(hash: Hash) -> Result<Bytes, Error> {\n    todo!()\n}",
    b"#include <stdio.h>\nint main(void) { return 0; }",
    b"def max_file_size(limit):\n    return min(limit, MAX_FILE_SIZE)\n",
    b"MAX_FILE_SIZE = 1 << 20\nMIN_FILE_SIZE = 512\n",
    b"SELECT grams FROM content_ngrams WHERE grams @> ARRAY[1,2,3];",
    b"the quick brown fox jumps over the lazy dog",
    b"The Quick Brown Fox Jumps Over The Lazy Dog",
    b"error: could not compile `sngram-python` (lib) due to 2 previous errors",
    b"for (int i = 0; i < n; i++) { sum += values[i]; }",
    b"let tuned = plan.tune(index.df, total_entries, stop_df);",
    b"x",
    b"",
    b"foo_bar baz_qux\nfoo_baz bar_qux\n",
    b"match event {\n    ScanEvent::Gram(gram) => grams.push(gram),\n    ScanEvent::Finish(s) => summary = Some(*s),\n}",
]

PATTERNS = [
    r"hello world",
    r"MAX_FILE_SIZE",
    r"max_\w+_size",
    r"(?i)quick brown fox",
    r"read_content|write_content",
    r"ScanEvent::(Gram|Finish)",
    r"#include <\w+\.h>",
    r"errors?$",
    r"^SELECT ",
    r"foo_(bar|baz) ",
    r"\bfox\b",
    r"sum \+= values\[i\]",
    r"[^\s\S]",
    r".*",
]


class ToyIndex:
    """Inverted index over gram keys plus per-document scan summaries."""

    def __init__(self, table, docs):
        self.table = table
        self.docs = docs
        self.postings = {}
        self.summaries = []
        for ord_, doc in enumerate(docs):
            result = sngram.scan(table, doc)
            self.summaries.append(result.summary)
            for _, _, key in result.grams:
                self.postings.setdefault(key, set()).add(ord_)

    def df(self, key):
        return len(self.postings.get(key, ()))

    def candidates(self, plan):
        return {ord_ for ord_ in range(len(self.docs)) if self._admits(plan, ord_)}

    def _admits(self, plan, ord_):
        if plan.op == "all":
            return True
        if plan.op == "none":
            return False
        checks = (
            [any(ord_ in self.postings.get(k, ()) for k in alts) for alts in plan.grams]
            + [need.satisfied_by(self.summaries[ord_]) for need in plan.needs]
            + [self._admits(child, ord_) for child in plan.children]
        )
        return all(checks) if plan.op == "and" else any(checks)


@pytest.fixture(scope="module")
def table():
    return sngram.weights()


@pytest.fixture(scope="module")
def index(table):
    return ToyIndex(table, DOCS)


def regex_matches(pattern):
    compiled = re.compile(pattern.encode())
    return {ord_ for ord_, doc in enumerate(DOCS) if compiled.search(doc)}


@pytest.mark.parametrize("pattern", PATTERNS)
def test_plan_candidates_cover_regex_matches(index, table, pattern):
    plan = sngram.query(table, pattern)
    candidates = index.candidates(plan)
    assert regex_matches(pattern) <= candidates


@pytest.mark.parametrize("pattern", PATTERNS)
def test_tuned_plan_stays_sound(index, table, pattern):
    plan = sngram.query(table, pattern)
    tuned = plan.tune(index.df, total_entries=len(DOCS), stop_df=len(DOCS) // 2)
    assert regex_matches(pattern) <= index.candidates(tuned)


def test_plans_actually_narrow(index, table):
    plan = sngram.query(table, "MAX_FILE_SIZE")
    assert index.candidates(plan) < set(range(len(DOCS)))


def test_fresh_table_indexes_end_to_end():
    counter = sngram.BigramCounter()
    for doc in DOCS * 3:
        counter.process(doc)
    fresh = sngram.WeightTable.from_bytes(counter.to_table_bytes())
    index = ToyIndex(fresh, DOCS)
    for pattern in (r"MAX_FILE_SIZE", r"(?i)quick brown fox", r"foo_(bar|baz) "):
        plan = sngram.query(fresh, pattern)
        assert regex_matches(pattern) <= index.candidates(plan)
