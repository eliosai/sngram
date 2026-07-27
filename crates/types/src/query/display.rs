use core::fmt;

use super::{GramNeedle, PlanExpr, QueryPlan, ScanNeed};
use crate::SaturatingByteCounts256;

impl PlanExpr {
    fn write_all_of(
        f: &mut fmt::Formatter<'_>,
        grams: &[GramNeedle],
        needs: &[ScanNeed],
        children: &[Self],
    ) -> fmt::Result {
        let mut first = true;
        for gram in grams {
            Self::delimit(f, &mut first, " ")?;
            write!(f, "{gram:?}")?;
        }
        for need in needs {
            Self::delimit(f, &mut first, " ")?;
            write!(f, "{need}")?;
        }
        for child in children {
            Self::delimit(f, &mut first, " ")?;
            write!(f, "{child}")?;
        }
        Ok(())
    }

    fn write_any_of(
        f: &mut fmt::Formatter<'_>,
        grams: &[GramNeedle],
        needs: &[ScanNeed],
        children: &[Self],
    ) -> fmt::Result {
        let mut first = true;
        for gram in grams {
            Self::delimit(f, &mut first, "|")?;
            write!(f, "{gram:?}")?;
        }
        for need in needs {
            Self::delimit(f, &mut first, "|")?;
            write!(f, "{need}")?;
        }
        for child in children {
            Self::delimit(f, &mut first, "|")?;
            write!(f, "({child})")?;
        }
        Ok(())
    }

    fn delimit(f: &mut fmt::Formatter<'_>, first: &mut bool, sep: &str) -> fmt::Result {
        if *first {
            *first = false;
            Ok(())
        } else {
            f.write_str(sep)
        }
    }
}

impl fmt::Display for QueryPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.root.fmt(f)
    }
}

impl fmt::Display for ScanNeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MinByteLen(n) => write!(f, "MinByteLen({n})"),
            Self::MinLongestLineLen(n) => write!(f, "MinLongestLineLen({n})"),
            Self::ContainsAnyByte(bytes) => write!(f, "ContainsAnyByte({bytes:?})"),
            Self::MinByteCounts(counts) => write_byte_counts(f, counts),
            Self::LineStartsWithAnyByte(bytes) => write!(f, "LineStartsWithAnyByte({bytes:?})"),
            Self::LineEndsWithAnyByte(bytes) => write!(f, "LineEndsWithAnyByte({bytes:?})"),
            Self::StartsWith(edge) => write!(f, "StartsWith({edge:?})"),
            Self::EndsWith(edge) => write!(f, "EndsWith({edge:?})"),
        }
    }
}

fn write_byte_counts(f: &mut fmt::Formatter<'_>, counts: &SaturatingByteCounts256) -> fmt::Result {
    f.write_str("MinByteCounts(")?;
    let mut first = true;
    for (byte, count) in counts
        .counts
        .iter()
        .enumerate()
        .filter(|&(_, &count)| count > 0)
    {
        PlanExpr::delimit(f, &mut first, ",")?;
        write!(f, "{byte:#04x}:{count}")?;
    }
    f.write_str(")")
}

impl fmt::Display for PlanExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::All => f.write_str("+"),
            Self::None => f.write_str("-"),
            Self::AllOf {
                grams,
                needs,
                children,
            } => Self::write_all_of(f, grams, needs, children),
            Self::AnyOf {
                grams,
                needs,
                children,
            } => Self::write_any_of(f, grams, needs, children),
        }
    }
}
