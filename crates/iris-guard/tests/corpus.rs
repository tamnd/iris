//! Every case in the adversarial corpus, run against the guard.
//!
//! This is one test rather than one test per case, because the corpus is meant to be extended and a
//! contributor adding a case should not also have to remember to add a test. The failure message
//! names the case, so a failure here reads the same as a failure of a test that had its own name.

use std::collections::HashSet;

use iris_guard::corpus::{Case, cases};

#[test]
fn every_unsound_case_is_refused_under_the_rule_it_breaks() {
    let mut checked = 0;
    for Case {
        name,
        why,
        expected,
        subject,
    } in cases()
    {
        let Some(expected) = expected else { continue };
        checked += 1;

        let Err(err) = subject.run() else {
            panic!("the guard accepted \"{name}\", which it must not.\n{why}")
        };

        assert_eq!(
            err.invariant, expected,
            "\"{name}\" was refused under the {} rule and it breaks the {expected} rule.\n{why}\n\
             The message was: {err}",
            err.invariant
        );
    }

    assert!(checked >= 8, "the corpus has shrunk to {checked} cases");
}

#[test]
fn every_sound_case_is_accepted() {
    let mut checked = 0;
    for Case {
        name,
        why,
        expected,
        subject,
    } in cases()
    {
        if expected.is_some() {
            continue;
        }
        checked += 1;

        // A checker that refuses everything passes every adversarial corpus ever written, so this
        // is the half of the corpus that says the guard is worth having.
        if let Err(err) = subject.run() {
            panic!("the guard refused \"{name}\", which is sound.\n{why}\nIt said: {err}");
        }
    }

    assert!(checked >= 3, "the corpus has no sound cases left");
}

#[test]
fn the_cases_have_distinct_names() {
    let mut seen = HashSet::new();
    for case in cases() {
        assert!(
            seen.insert(case.name),
            "two cases are both called \"{}\", and a failure would name either",
            case.name
        );
    }
}
