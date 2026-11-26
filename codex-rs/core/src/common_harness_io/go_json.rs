// Modified by RenCrow Switch Core, 2026-11-26: complete Go JSON verifier observations.
use super::ExpectedBaselineFailure;
use super::GoTestExpectation;
use super::GoTestObservation;
use super::GoTestVerdict;
use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

#[derive(Deserialize)]
struct GoJsonEvent {
    #[serde(rename = "Time")]
    _time: Option<String>,
    #[serde(rename = "Action")]
    action: String,
    #[serde(rename = "Package")]
    package: String,
    #[serde(rename = "Test")]
    test: Option<String>,
    #[serde(rename = "Elapsed")]
    _elapsed: Option<f64>,
    #[serde(rename = "Output")]
    output: Option<String>,
}

#[derive(Default)]
struct PackageObservation {
    terminal: Option<String>,
}

#[derive(Default)]
struct CaseObservation {
    terminal: Option<String>,
    output: String,
}

pub(super) fn observe_go_test_json(
    stream: &[u8],
    launch_succeeded: bool,
    raw_exit_code: Option<i32>,
    expectation: &GoTestExpectation,
) -> Result<GoTestObservation> {
    let mut observation = empty_observation(launch_succeeded, raw_exit_code);
    if !launch_succeeded {
        return Ok(observation);
    }
    if stream.is_empty() || !stream.ends_with(b"\n") {
        return Ok(observation);
    }
    let source = match std::str::from_utf8(stream) {
        Ok(source) => source,
        Err(_) => return Ok(observation),
    };
    let mut packages = BTreeMap::<String, PackageObservation>::new();
    let mut cases = BTreeMap::<(String, String), CaseObservation>::new();
    let mut malformed = false;

    for line in source.lines() {
        if line.is_empty() {
            malformed = true;
            break;
        }
        let event = match serde_json::from_str::<GoJsonEvent>(line) {
            Ok(event) if !event.action.is_empty() && !event.package.is_empty() => event,
            _ => {
                malformed = true;
                break;
            }
        };
        match event.action.as_str() {
            "start" => {
                if event.test.is_some() || packages.contains_key(&event.package) {
                    malformed = true;
                    break;
                }
                packages.insert(event.package, PackageObservation::default());
            }
            "run" => {
                let Some(test_name) = event.test.filter(|name| !name.is_empty()) else {
                    malformed = true;
                    break;
                };
                if !package_is_open(&packages, &event.package)
                    || cases
                        .insert((event.package, test_name), CaseObservation::default())
                        .is_some()
                {
                    malformed = true;
                    break;
                }
            }
            "output" => {
                if !package_is_open(&packages, &event.package) {
                    malformed = true;
                    break;
                }
                let Some(output) = event.output else {
                    malformed = true;
                    break;
                };
                if let Some(test_name) = event.test {
                    let Some(case) = cases.get_mut(&(event.package, test_name)) else {
                        malformed = true;
                        break;
                    };
                    if case.terminal.is_some() {
                        malformed = true;
                        break;
                    }
                    case.output.push_str(&output);
                }
            }
            "pass" | "fail" | "skip" => {
                if let Some(test_name) = event.test {
                    if !package_is_open(&packages, &event.package) {
                        malformed = true;
                        break;
                    }
                    let Some(case) = cases.get_mut(&(event.package, test_name)) else {
                        malformed = true;
                        break;
                    };
                    if case.terminal.replace(event.action.clone()).is_some() {
                        malformed = true;
                        break;
                    }
                } else {
                    let Some(package) = packages.get_mut(&event.package) else {
                        malformed = true;
                        break;
                    };
                    if package.terminal.replace(event.action.clone()).is_some() {
                        malformed = true;
                        break;
                    }
                }
            }
            "pause" | "cont" => {
                let Some(test_name) = event.test else {
                    malformed = true;
                    break;
                };
                if !package_is_open(&packages, &event.package)
                    || !cases.contains_key(&(event.package, test_name))
                {
                    malformed = true;
                    break;
                }
            }
            _ => {
                malformed = true;
                break;
            }
        }
    }

    let mut package_terminals = Vec::new();
    let mut log_complete = !malformed && !packages.is_empty();
    for package in packages.values() {
        match package.terminal.as_deref() {
            Some(action @ ("pass" | "fail" | "skip")) => package_terminals.push(action),
            _ => log_complete = false,
        }
    }

    let mut seen_test_names = BTreeMap::<String, usize>::new();
    let mut failed_case_packages = BTreeSet::new();
    let mut case_names = BTreeSet::new();
    for ((package, name), case) in &cases {
        *seen_test_names.entry(name.clone()).or_default() += 1;
        case_names.insert(name.clone());
        if case.terminal.is_none() {
            log_complete = false;
        }
        match case.terminal.as_deref() {
            Some("pass") => observation.passed += 1,
            Some("fail") => {
                observation.failed += 1;
                failed_case_packages.insert(package.as_str());
            }
            Some("skip") => observation.skipped += 1,
            _ => {}
        }
    }
    observation.case_count = cases.len();
    observation.selected_names = case_names.into_iter().collect();
    observation.log_complete = log_complete;
    let duplicate_case_name = seen_test_names.values().any(|count| *count > 1);
    let package_failed = package_terminals.contains(&"fail");
    let package_passed = package_terminals.iter().all(|action| *action == "pass");
    let package_skipped = package_terminals.contains(&"skip");
    let package_failure_mismatch = packages.iter().any(|(package, state)| {
        let package_has_test_failure = failed_case_packages.contains(package.as_str());
        match state.terminal.as_deref() {
            Some("fail") => !package_has_test_failure,
            Some("pass" | "skip") => package_has_test_failure,
            _ => false,
        }
    });
    observation.framework_error = malformed
        || package_failure_mismatch
        || (package_terminals.iter().all(|action| *action != "fail")
            && raw_exit_code.is_some_and(|code| code != 0))
        || (package_failed && raw_exit_code == Some(0));

    if !log_complete
        || observation.framework_error
        || duplicate_case_name
        || raw_exit_code.is_none()
        || !expectation_is_well_formed(expectation)
    {
        return Ok(observation);
    }

    let expected_names = expectation
        .selected_cases
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let selected_cases_match = observation.case_count == expectation.expected_case_count
        && case_names_match(&observation.selected_names, &expected_names);
    if let Some(expected_failure) = &expectation.expected_baseline_failure
        && expected_baseline_failure_matches(
            &cases,
            &package_terminals,
            expected_failure,
            selected_cases_match,
            &observation,
        )
    {
        observation.matched_expected_baseline_failure = true;
        observation.verdict = GoTestVerdict::Pass;
        return Ok(observation);
    }

    if !selected_cases_match {
        observation.verdict = GoTestVerdict::Fail;
    } else if observation.skipped > 0 || package_skipped {
        observation.verdict = GoTestVerdict::Skip;
    } else if observation.failed > 0 {
        observation.verdict = GoTestVerdict::Fail;
    } else if observation.passed == expectation.expected_case_count
        && package_passed
        && raw_exit_code == Some(0)
    {
        observation.verdict = GoTestVerdict::Pass;
    } else {
        observation.verdict = GoTestVerdict::Error;
        observation.framework_error = true;
    }
    Ok(observation)
}

fn empty_observation(launch_succeeded: bool, raw_exit_code: Option<i32>) -> GoTestObservation {
    GoTestObservation {
        case_count: 0,
        selected_names: Vec::new(),
        passed: 0,
        failed: 0,
        skipped: 0,
        launch_succeeded,
        framework_error: false,
        log_complete: false,
        raw_exit_code,
        verdict: GoTestVerdict::Error,
        matched_expected_baseline_failure: false,
    }
}

fn package_is_open(packages: &BTreeMap<String, PackageObservation>, package: &str) -> bool {
    packages
        .get(package)
        .is_some_and(|observation| observation.terminal.is_none())
}

fn expectation_is_well_formed(expectation: &GoTestExpectation) -> bool {
    expectation.expected_case_count > 0
        && expectation.expected_case_count == expectation.selected_cases.len()
        && expectation
            .selected_cases
            .iter()
            .all(|name| !name.trim().is_empty())
        && expectation
            .selected_cases
            .iter()
            .collect::<BTreeSet<_>>()
            .len()
            == expectation.selected_cases.len()
        && expectation
            .expected_baseline_failure
            .as_ref()
            .is_none_or(|failure| {
                expectation.selected_cases.contains(&failure.case_name)
                    && !failure.reason.trim().is_empty()
            })
}

fn case_names_match(observed: &[String], expected: &BTreeSet<String>) -> bool {
    observed
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .eq(expected)
}

fn expected_baseline_failure_matches(
    cases: &BTreeMap<(String, String), CaseObservation>,
    package_terminals: &[&str],
    expected: &ExpectedBaselineFailure,
    selected_cases_match: bool,
    observation: &GoTestObservation,
) -> bool {
    if !selected_cases_match
        || observation.failed != 1
        || observation.skipped != 0
        || observation.passed + observation.failed != observation.case_count
        || package_terminals
            .iter()
            .filter(|action| **action == "fail")
            .count()
            != 1
    {
        return false;
    }
    cases.iter().any(|((_, name), case)| {
        name == &expected.case_name
            && case.terminal.as_deref() == Some("fail")
            && case.output.contains(&expected.reason)
    })
}
