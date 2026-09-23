//! Bounded, deterministic excerpts and byte coverage for a persisted observation.

use crate::archive_reference::ObservationReference;
use crate::archive_reference::content_sha256;
use crate::compaction_plan::ByteRange;
use serde::Deserialize;
use serde::Serialize;

pub const OBSERVATION_PART_FULL_LIMIT_BYTES: usize = 2_048;
pub const OBSERVATION_PART_EDGE_BYTES: usize = 1_024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationPartCoverage {
    pub sha256: String,
    pub total_bytes: usize,
    pub presented_ranges: Vec<ByteRange>,
    pub unpresented_ranges: Vec<ByteRange>,
    pub partial: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ObservationPartExcerpt {
    pub coverage: ObservationPartCoverage,
    pub excerpts: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ObservationSummaryExcerpt {
    pub reference: ObservationReference,
    pub tool_name: String,
    pub call: ObservationPartExcerpt,
    pub output: ObservationPartExcerpt,
}

/// Checkpoint-safe coverage metadata. This type deliberately contains no excerpt text.
///
/// Validation checks digest shape, reference binding, range partition, and the derived
/// `partial` flag. It cannot prove that a digest matches unavailable source bytes; the
/// producer hashes the original text, while rollout owners re-check raw bytes on retrieval.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationCoverage {
    pub reference: ObservationReference,
    pub tool_name: String,
    pub call: ObservationPartCoverage,
    pub output: ObservationPartCoverage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservationProjection {
    pub summary: ObservationSummaryExcerpt,
    pub coverage: ObservationCoverage,
}

impl ObservationPartCoverage {
    pub fn validate(&self) -> Result<(), String> {
        if !valid_sha256(&self.sha256) {
            return Err("observation part has an invalid SHA-256 digest".into());
        }

        let valid = if self.total_bytes == 0 {
            self.presented_ranges.is_empty() && self.unpresented_ranges.is_empty() && !self.partial
        } else if self.presented_ranges.is_empty()
            && self.unpresented_ranges.len() == 1
            && self.unpresented_ranges[0].start == 0
            && self.unpresented_ranges[0].end == self.total_bytes
            && self.partial
        {
            true
        } else if self.total_bytes <= OBSERVATION_PART_FULL_LIMIT_BYTES {
            self.presented_ranges.len() == 1
                && self.presented_ranges[0].start == 0
                && self.presented_ranges[0].end == self.total_bytes
                && self.unpresented_ranges.is_empty()
                && !self.partial
        } else if self.presented_ranges.len() == 2 && self.unpresented_ranges.len() == 1 {
            let head = &self.presented_ranges[0];
            let tail = &self.presented_ranges[1];
            let gap = &self.unpresented_ranges[0];
            head.start == 0
                && head.start < head.end
                && head.end <= OBSERVATION_PART_EDGE_BYTES
                && head.end < tail.start
                && tail.start < tail.end
                && tail.end == self.total_bytes
                && self.total_bytes - tail.start <= OBSERVATION_PART_EDGE_BYTES
                && gap.start == head.end
                && gap.end == tail.start
                && self.partial
        } else {
            false
        };
        if !valid {
            return Err("observation part ranges and partial flag are inconsistent".into());
        }
        Ok(())
    }
}

impl ObservationCoverage {
    pub fn validate(&self) -> Result<(), String> {
        validate_identity(&self.reference, &self.tool_name)?;
        self.call.validate()?;
        self.output.validate()?;
        if is_fully_unpresented(&self.call) {
            return Err("observation call coverage must include its bounded projection".into());
        }
        if self.reference.sha256 != self.output.sha256 {
            return Err("observation output coverage does not match its reference digest".into());
        }
        if self.output.total_bytes == 0 && self.reference.sha256 != content_sha256("") {
            return Err("empty observation output has an incorrect digest".into());
        }
        Ok(())
    }
}

impl ObservationProjection {
    pub fn validate(&self) -> Result<(), String> {
        self.coverage.validate()?;
        if self.summary.reference != self.coverage.reference
            || self.summary.tool_name != self.coverage.tool_name
        {
            return Err("summary excerpt identity does not match observation coverage".into());
        }
        validate_part_excerpt(&self.summary.call, &self.coverage.call)?;
        validate_part_excerpt(&self.summary.output, &self.coverage.output)?;
        Ok(())
    }
}

pub fn project_observation(
    reference: &ObservationReference,
    tool_name: &str,
    call_text: &str,
    output_text: &str,
) -> Result<ObservationProjection, String> {
    validate_identity(reference, tool_name)?;
    if !valid_sha256(&reference.sha256) || content_sha256(output_text) != reference.sha256 {
        return Err("observation output does not match its reference digest".into());
    }

    let call = project_part(call_text);
    let output = project_part(output_text);
    let summary = ObservationSummaryExcerpt {
        reference: reference.clone(),
        tool_name: tool_name.to_owned(),
        call: call.clone(),
        output: output.clone(),
    };
    let coverage = ObservationCoverage {
        reference: reference.clone(),
        tool_name: tool_name.to_owned(),
        call: call.coverage,
        output: output.coverage,
    };
    let projection = ObservationProjection { summary, coverage };
    projection.validate()?;
    Ok(projection)
}

/// Project a validated v1 reference using the current selected call and the canonical output
/// byte length. The referenced output body is intentionally absent: this creates all-unpresented
/// output coverage without rehydrating or presenting archived text.
pub fn project_existing_observation(
    reference: &ObservationReference,
    tool_name: &str,
    call_text: &str,
    output_total_bytes: usize,
) -> Result<ObservationProjection, String> {
    validate_identity(reference, tool_name)?;
    if output_total_bytes == 0 && reference.sha256 != content_sha256("") {
        return Err("empty observation output does not match its reference digest".into());
    }

    let call = project_part(call_text);
    let output = ObservationPartExcerpt {
        coverage: ObservationPartCoverage {
            sha256: reference.sha256.clone(),
            total_bytes: output_total_bytes,
            presented_ranges: Vec::new(),
            unpresented_ranges: if output_total_bytes == 0 {
                Vec::new()
            } else {
                vec![ByteRange {
                    start: 0,
                    end: output_total_bytes,
                }]
            },
            partial: output_total_bytes > 0,
        },
        excerpts: Vec::new(),
    };
    let summary = ObservationSummaryExcerpt {
        reference: reference.clone(),
        tool_name: tool_name.to_owned(),
        call: call.clone(),
        output: output.clone(),
    };
    let coverage = ObservationCoverage {
        reference: reference.clone(),
        tool_name: tool_name.to_owned(),
        call: call.coverage,
        output: output.coverage,
    };
    let projection = ObservationProjection { summary, coverage };
    projection.validate()?;
    Ok(projection)
}

fn validate_identity(reference: &ObservationReference, tool_name: &str) -> Result<(), String> {
    if reference.thread_id.trim().is_empty()
        || reference.call_id.trim().is_empty()
        || tool_name.trim().is_empty()
    {
        return Err("observation identity and tool name must be nonempty".into());
    }
    if !valid_sha256(&reference.sha256) {
        return Err("observation reference has an invalid SHA-256 digest".into());
    }
    Ok(())
}

fn valid_sha256(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn project_part(text: &str) -> ObservationPartExcerpt {
    let total_bytes = text.len();
    let (excerpts, presented_ranges, unpresented_ranges) = if total_bytes == 0 {
        (Vec::new(), Vec::new(), Vec::new())
    } else if total_bytes <= OBSERVATION_PART_FULL_LIMIT_BYTES {
        (
            vec![text.to_owned()],
            vec![ByteRange {
                start: 0,
                end: total_bytes,
            }],
            Vec::new(),
        )
    } else {
        let mut head_end = OBSERVATION_PART_EDGE_BYTES.min(total_bytes);
        while !text.is_char_boundary(head_end) {
            head_end -= 1;
        }
        let mut tail_start = total_bytes - OBSERVATION_PART_EDGE_BYTES;
        while !text.is_char_boundary(tail_start) {
            tail_start += 1;
        }
        (
            vec![text[..head_end].to_owned(), text[tail_start..].to_owned()],
            vec![
                ByteRange {
                    start: 0,
                    end: head_end,
                },
                ByteRange {
                    start: tail_start,
                    end: total_bytes,
                },
            ],
            vec![ByteRange {
                start: head_end,
                end: tail_start,
            }],
        )
    };

    ObservationPartExcerpt {
        coverage: ObservationPartCoverage {
            sha256: content_sha256(text),
            total_bytes,
            presented_ranges,
            partial: !unpresented_ranges.is_empty(),
            unpresented_ranges,
        },
        excerpts,
    }
}

fn validate_part_excerpt(
    excerpt: &ObservationPartExcerpt,
    checkpoint: &ObservationPartCoverage,
) -> Result<(), String> {
    excerpt.coverage.validate()?;
    if &excerpt.coverage != checkpoint
        || excerpt.excerpts.len() != checkpoint.presented_ranges.len()
        || excerpt
            .excerpts
            .iter()
            .zip(&checkpoint.presented_ranges)
            .any(|(text, range)| text.len() != range.end - range.start)
    {
        return Err("summary excerpt does not match observation coverage".into());
    }
    Ok(())
}

fn is_fully_unpresented(part: &ObservationPartCoverage) -> bool {
    part.total_bytes > 0
        && part.presented_ranges.is_empty()
        && part.unpresented_ranges.len() == 1
        && part.unpresented_ranges[0].start == 0
        && part.unpresented_ranges[0].end == part.total_bytes
        && part.partial
}

#[cfg(test)]
#[path = "observation_projection_tests.rs"]
mod tests;
