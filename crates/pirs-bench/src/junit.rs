//! JUnit-XML result parsing — the single interchange format the harness reads.
//!
//! Standardizing on JUnit (pytest `--junitxml`, `gotestsum`, `cargo nextest
//! --junit`, jest, …) means one robust parser instead of a fragile per-runner
//! text scraper. The matcher maps the runner's reported cases back onto the
//! *requested* test ids and — critically — reports any requested id the run
//! never produced as [`TestOutcome::NotCollected`], never as a pass.

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::types::{Snapshot, TestId, TestOutcome};

/// One `<testcase>` as reported by the runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JunitCase {
    pub classname: String,
    pub name: String,
    pub outcome: TestOutcome,
}

impl JunitCase {
    /// The leaf test name — the last `::`/`.`/`/`-separated segment of a node id
    /// is what JUnit records as `name`, so this is the primary match anchor.
    fn leaf(&self) -> &str {
        &self.name
    }
}

/// Parse JUnit XML into per-case outcomes. A `<testcase>` with a `<failure>`
/// child is `Fail`, `<error>` is `Errored`, `<skipped>` is `NotCollected` (a
/// skip did not exercise the test), and a childless case is `Pass`.
pub fn parse(xml: &str) -> anyhow::Result<Vec<JunitCase>> {
    use quick_xml::events::BytesStart;

    fn read_case(e: &BytesStart) -> JunitCase {
        let (mut classname, mut name) = (String::new(), String::new());
        for attr in e.attributes().flatten() {
            match attr.key.as_ref() {
                b"classname" => classname = attr.unescape_value().unwrap_or_default().into_owned(),
                b"name" => name = attr.unescape_value().unwrap_or_default().into_owned(),
                _ => {}
            }
        }
        JunitCase {
            classname,
            name,
            outcome: TestOutcome::Pass,
        }
    }

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut cases = Vec::new();
    let mut cur: Option<JunitCase> = None;

    loop {
        match reader.read_event()? {
            Event::Eof => break,
            // Self-closing <testcase/> is a completed pass with no End event.
            Event::Empty(e) if e.name().as_ref() == b"testcase" => cases.push(read_case(&e)),
            Event::Start(e) if e.name().as_ref() == b"testcase" => cur = Some(read_case(&e)),
            Event::End(e) if e.name().as_ref() == b"testcase" => {
                if let Some(c) = cur.take() {
                    cases.push(c);
                }
            }
            // Outcome markers inside an open <testcase>…</testcase>.
            Event::Start(e) | Event::Empty(e) => match e.name().as_ref() {
                b"failure" => set_outcome(&mut cur, TestOutcome::Fail),
                b"error" => set_outcome(&mut cur, TestOutcome::Errored),
                b"skipped" => set_outcome(&mut cur, TestOutcome::NotCollected),
                _ => {}
            },
            _ => {}
        }
    }
    Ok(cases)
}

fn set_outcome(cur: &mut Option<JunitCase>, outcome: TestOutcome) {
    if let Some(c) = cur.as_mut() {
        c.outcome = outcome;
    }
}

/// Map reported cases onto the requested test ids. Every requested id that the
/// run did not report becomes `NotCollected` — the anti-false-green default.
///
/// Matching anchors on the leaf test name (JUnit `name` == the id's last
/// `::` segment) and disambiguates collisions by requiring the case's classname
/// tokens to appear in the requested id.
pub fn to_snapshot(requested: &[TestId], cases: &[JunitCase], build_ok: bool) -> Snapshot {
    let mut states = std::collections::HashMap::new();
    for id in requested {
        let leaf = id.rsplit([':', '/']).next().unwrap_or(id);
        let hit = cases
            .iter()
            .filter(|c| c.leaf() == leaf)
            .find(|c| classname_consistent(&c.classname, id))
            .or_else(|| cases.iter().find(|c| c.leaf() == leaf));
        let outcome = hit.map(|c| c.outcome).unwrap_or(TestOutcome::NotCollected);
        states.insert(id.clone(), outcome);
    }
    Snapshot {
        states,
        build_ok,
        runs: 1,
    }
}

/// Whether a JUnit `classname` plausibly belongs to the requested node id: each
/// dotted classname token must appear somewhere in the id. Cheap, and enough to
/// separate same-named tests in different modules.
fn classname_consistent(classname: &str, id: &str) -> bool {
    classname
        .split(['.', ':', '/'])
        .filter(|t| !t.is_empty())
        .all(|tok| id.contains(tok))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TestOutcome::*;

    const SAMPLE: &str = r#"
      <testsuites>
        <testsuite name="pytest" tests="4">
          <testcase classname="tests.test_math" name="test_add" time="0.01"/>
          <testcase classname="tests.test_math" name="test_sub" time="0.02">
            <failure message="assert 1 == 2">detail</failure>
          </testcase>
          <testcase classname="tests.test_io" name="test_read" time="0.03">
            <error message="ImportError">trace</error>
          </testcase>
          <testcase classname="tests.test_io" name="test_skip" time="0.0">
            <skipped message="no network"/>
          </testcase>
        </testsuite>
      </testsuites>"#;

    #[test]
    fn parses_all_outcome_kinds() {
        let cases = parse(SAMPLE).unwrap();
        assert_eq!(cases.len(), 4);
        assert_eq!(cases[0].outcome, Pass);
        assert_eq!(cases[1].outcome, Fail);
        assert_eq!(cases[2].outcome, Errored);
        assert_eq!(cases[3].outcome, NotCollected); // skipped
    }

    #[test]
    fn matches_pytest_node_ids() {
        let cases = parse(SAMPLE).unwrap();
        let requested = vec![
            "tests/test_math.py::test_add".to_string(),
            "tests/test_math.py::test_sub".to_string(),
        ];
        let snap = to_snapshot(&requested, &cases, true);
        assert_eq!(snap.get("tests/test_math.py::test_add"), Some(Pass));
        assert_eq!(snap.get("tests/test_math.py::test_sub"), Some(Fail));
    }

    #[test]
    fn unreported_request_is_not_collected() {
        // The safety net: a requested id the run never produced is NotCollected,
        // never silently a pass.
        let cases = parse(SAMPLE).unwrap();
        let requested = vec!["tests/test_math.py::test_deleted".to_string()];
        let snap = to_snapshot(&requested, &cases, true);
        assert_eq!(
            snap.get("tests/test_math.py::test_deleted"),
            Some(NotCollected)
        );
    }

    #[test]
    fn same_leaf_disambiguated_by_classname() {
        let cases = vec![
            JunitCase {
                classname: "pkg.mod_a".into(),
                name: "test_it".into(),
                outcome: Pass,
            },
            JunitCase {
                classname: "pkg.mod_b".into(),
                name: "test_it".into(),
                outcome: Fail,
            },
        ];
        let requested = vec!["pkg/mod_b.py::test_it".to_string()];
        let snap = to_snapshot(&requested, &cases, true);
        // Must pick mod_b's failing case, not mod_a's pass.
        assert_eq!(snap.get("pkg/mod_b.py::test_it"), Some(Fail));
    }
}
