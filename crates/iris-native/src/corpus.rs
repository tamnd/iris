//! The datasets a native kernel has to agree with the module about.

/// One dataset the two implementations are run against.
///
/// A case is the bytes of a data section, addressed from zero, which is the view both a guest and a
/// native implementation are given. It is not a container: nothing here parses a header or checks a
/// digest, because by the time a differential run happens the module bytes are already in hand and
/// the question is what the two implementations do with the same data.
///
/// The shape has to be stated because the harness derives the requests from it. A dataset of five
/// hundred rows and two columns is scanned whole, a row at a time from each end, past its own end,
/// and once for each column, and none of that can be worked out from a buffer of bytes whose layout
/// only the decoder understands.
#[derive(Clone, Debug)]
pub struct Case {
    name: String,
    data: Vec<u8>,
    rows: u64,
    columns: u32,
}

impl Case {
    /// A dataset, what it holds, and its shape.
    ///
    /// The name is what a mismatch is reported against, so it is worth making it say which dataset
    /// this is rather than numbering them.
    #[must_use]
    pub fn new(name: impl Into<String>, data: Vec<u8>, rows: u64, columns: u32) -> Self {
        Self {
            name: name.into(),
            data,
            rows,
            columns,
        }
    }

    /// What this case is called.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The bytes both implementations read.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// How many rows the dataset holds.
    #[must_use]
    pub const fn rows(&self) -> u64 {
        self.rows
    }

    /// How many columns the dataset holds.
    #[must_use]
    pub const fn columns(&self) -> u32 {
        self.columns
    }
}

/// The datasets a kernel is proved against.
///
/// # What a corpus is worth
///
/// Exactly as much as what is in it, and this is the honest limit of the whole arrangement. The
/// type system can force a differential run to happen and the run can force byte identity on every
/// case it was given, and neither of those can force the cases to be interesting. A corpus of one
/// dataset with one row proves that a kernel reads one row.
///
/// So the two checks here are the ones that can be made without knowing what the decoder reads. A
/// corpus with no cases in it is refused, and a case with no rows in it is refused, because both of
/// those are proofs of nothing dressed up as a passing run. Everything past that is the judgement of
/// whoever assembled the corpus, and the reason it is still worth having is the other half of the
/// design: a kernel that gets a row wrong in a way the corpus never asked about still hands its
/// batches to `iris-guard` and to Arrow on every scan, exactly as a guest does.
#[derive(Clone, Debug, Default)]
pub struct Corpus {
    cases: Vec<Case>,
}

impl Corpus {
    /// An empty corpus, which is not one anything can be proved against.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a dataset.
    #[must_use]
    pub fn with_case(mut self, case: Case) -> Self {
        self.cases.push(case);
        self
    }

    /// The datasets in it.
    #[must_use]
    pub fn cases(&self) -> &[Case] {
        &self.cases
    }

    /// How many datasets are in it.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cases.len()
    }

    /// Whether there is nothing in it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cases.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{Case, Corpus};

    #[test]
    fn a_corpus_holds_the_cases_it_was_given_in_the_order_they_arrived() {
        let corpus = Corpus::new()
            .with_case(Case::new("first", vec![1, 2, 3], 3, 1))
            .with_case(Case::new("second", vec![4], 1, 1));

        assert_eq!(corpus.len(), 2);
        assert_eq!(corpus.cases()[0].name(), "first");
        assert_eq!(corpus.cases()[1].name(), "second");
        assert_eq!(corpus.cases()[0].data(), &[1, 2, 3]);
        assert_eq!(corpus.cases()[0].rows(), 3);
        assert_eq!(corpus.cases()[0].columns(), 1);
    }

    #[test]
    fn a_new_corpus_is_empty() {
        assert!(Corpus::new().is_empty());
    }
}
