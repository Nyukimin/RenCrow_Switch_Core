use serde::Deserialize;
use serde::Deserializer;
use serde::de::DeserializeSeed;
use serde::de::MapAccess;
use serde::de::Visitor;
use serde::de::value::MapAccessDeserializer;
use serde_json::Map;
use serde_json::Value;
use serde_json::error::Category;
use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

const EVENT_MSG: &str = "event_msg";
const TOKEN_COUNT: &str = "token_count";
const TOKEN_USAGE_RECORD: &str = "token_usage_record";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TelemetryKind {
    TokenUsageRecord,
    TokenCount,
}

impl TelemetryKind {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::TokenUsageRecord => TOKEN_USAGE_RECORD,
            Self::TokenCount => "event_msg.token_count",
        }
    }
}

pub(crate) enum LineInspection {
    Parsed {
        value: Value,
        telemetry: Option<TelemetryKind>,
    },
    Discard {
        kind: TelemetryKind,
        reason: &'static str,
    },
    Invalid {
        reason: String,
    },
}

pub(crate) fn inspect_line(line: &str) -> LineInspection {
    let state = Rc::new(RefCell::new(DiscriminatorState {
        ordinal_valid: true,
        ..DiscriminatorState::default()
    }));
    let mut deserializer = serde_json::Deserializer::from_str(line);
    let parsed = deserializer.deserialize_map(RootVisitor {
        state: Rc::clone(&state),
    });
    let state = Rc::try_unwrap(state)
        .expect("rollout discriminator parser should release its state")
        .into_inner();

    match parsed {
        Ok(value) => match deserializer.end() {
            Ok(()) => {
                if state.ambiguous_discriminator || state.ambiguous_header {
                    return LineInspection::Invalid {
                        reason: "ambiguous duplicate discriminator".to_string(),
                    };
                }
                LineInspection::Parsed {
                    telemetry: telemetry_kind(&state),
                    value,
                }
            }
            Err(error) => LineInspection::Invalid {
                reason: json_error_reason(&error),
            },
        },
        Err(error) if error.classify() == Category::Eof => {
            if let Some(kind) = truncated_telemetry_kind(&state) {
                LineInspection::Discard {
                    kind,
                    reason: "truncated telemetry body",
                }
            } else {
                LineInspection::Invalid {
                    reason: json_error_reason(&error),
                }
            }
        }
        Err(error) => LineInspection::Invalid {
            reason: json_error_reason(&error),
        },
    }
}

fn json_error_reason(error: &serde_json::Error) -> String {
    let category = match error.classify() {
        Category::Io => "io",
        Category::Syntax => "syntax",
        Category::Data => "data",
        Category::Eof => "eof",
    };
    format!(
        "json {category} at line {} column {}",
        error.line(),
        error.column()
    )
}

pub(crate) fn has_valid_rollout_header(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    let timestamp_valid = object.get("timestamp").is_some_and(Value::is_string);
    let ordinal_valid = object
        .get("ordinal")
        .is_none_or(|ordinal| ordinal.is_null() || ordinal.as_u64().is_some());
    timestamp_valid && ordinal_valid
}

#[derive(Debug, Default)]
struct DiscriminatorState {
    root_type: Option<String>,
    root_type_count: usize,
    payload_count: usize,
    payload_is_object: bool,
    payload_type: Option<String>,
    payload_type_count: usize,
    payload_body_started: bool,
    payload_value_in_progress: bool,
    payload_type_after_body: bool,
    payload_completed: bool,
    root_type_before_payload: bool,
    timestamp_valid: bool,
    ordinal_valid: bool,
    timestamp_count: usize,
    ordinal_count: usize,
    ambiguous_header: bool,
    ambiguous_discriminator: bool,
}

impl DiscriminatorState {
    fn header_complete(&self) -> bool {
        self.timestamp_valid && self.ordinal_valid
    }

    fn note_root_type(&mut self) {
        self.root_type_count += 1;
        if self.root_type_count > 1 {
            self.ambiguous_discriminator = true;
        }
    }

    fn note_payload(&mut self) {
        self.payload_count += 1;
        if self.payload_count > 1 {
            self.ambiguous_discriminator = true;
        }
        self.root_type_before_payload = self.root_type_count == 1 && self.root_type.is_some();
    }

    fn note_payload_type_start(&mut self) {
        self.payload_type_count += 1;
        if self.payload_type_count > 1 {
            self.ambiguous_discriminator = true;
        }
        if self.payload_body_started {
            self.payload_type_after_body = true;
        }
    }
}

struct RootVisitor {
    state: Rc<RefCell<DiscriminatorState>>,
}

impl<'de> Visitor<'de> for RootVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a rollout JSON object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut object = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if key == "type" {
                let mut state = self.state.borrow_mut();
                state.note_root_type();
                drop(state);
                let value = map.next_value::<Value>()?;
                if self.state.borrow().root_type_count == 1 {
                    self.state.borrow_mut().root_type = value.as_str().map(str::to_owned);
                }
                object.insert(key, value);
            } else if key == "timestamp" {
                {
                    let mut state = self.state.borrow_mut();
                    state.timestamp_count += 1;
                    if state.timestamp_count > 1 {
                        state.ambiguous_header = true;
                    }
                }
                let value = map.next_value::<Value>();
                match value {
                    Ok(value) => {
                        self.state.borrow_mut().timestamp_valid = value.is_string();
                        object.insert(key, value);
                    }
                    Err(error) => {
                        self.state.borrow_mut().timestamp_valid = false;
                        return Err(error);
                    }
                }
            } else if key == "ordinal" {
                {
                    let mut state = self.state.borrow_mut();
                    state.ordinal_count += 1;
                    if state.ordinal_count > 1 {
                        state.ambiguous_header = true;
                    }
                }
                let value = map.next_value::<Value>();
                match value {
                    Ok(value) => {
                        self.state.borrow_mut().ordinal_valid =
                            value.is_null() || value.as_u64().is_some();
                        object.insert(key, value);
                    }
                    Err(error) => {
                        self.state.borrow_mut().ordinal_valid = false;
                        return Err(error);
                    }
                }
            } else if key == "payload" {
                self.state.borrow_mut().note_payload();
                let payload = map.next_value_seed(PayloadSeed {
                    state: self.state.clone(),
                })?;
                object.insert(key, payload);
            } else {
                let value = map.next_value::<Value>()?;
                object.insert(key, value);
            }
        }
        Ok(Value::Object(object))
    }
}

struct PayloadSeed {
    state: Rc<RefCell<DiscriminatorState>>,
}

impl<'de> DeserializeSeed<'de> for PayloadSeed {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(PayloadVisitor { state: self.state })
    }
}

struct BodyValueSeed {
    state: Rc<RefCell<DiscriminatorState>>,
}

impl<'de> DeserializeSeed<'de> for BodyValueSeed {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(BodyValueVisitor { state: self.state })
    }
}

struct BodyValueVisitor {
    state: Rc<RefCell<DiscriminatorState>>,
}

impl BodyValueVisitor {
    fn value_started(&self) {
        self.state.borrow_mut().payload_value_in_progress = true;
    }
}

impl<'de> Visitor<'de> for BodyValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        self.value_started();
        Value::deserialize(MapAccessDeserializer::new(map))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        self.value_started();
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<Value>()? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        self.value_started();
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        self.value_started();
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        self.value_started();
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E> {
        self.value_started();
        Ok(serde_json::Number::from_f64(value)
            .map(Value::Number)
            .unwrap_or(Value::Null))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        self.value_started();
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        self.value_started();
        Ok(Value::String(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        self.value_started();
        Ok(Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        self.value_started();
        Ok(Value::Null)
    }
}

struct PayloadVisitor {
    state: Rc<RefCell<DiscriminatorState>>,
}

impl<'de> Visitor<'de> for PayloadVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a rollout payload")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let state = self.state;
        state.borrow_mut().payload_is_object = true;
        let mut object = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if key == "type" {
                state.borrow_mut().note_payload_type_start();
                let value = map.next_value_seed(BodyValueSeed {
                    state: state.clone(),
                })?;
                state.borrow_mut().payload_value_in_progress = false;
                if state.borrow().payload_type_count == 1 {
                    state.borrow_mut().payload_type = value.as_str().map(str::to_owned);
                }
                object.insert(key, value);
            } else {
                state.borrow_mut().payload_body_started = true;
                let value = map.next_value_seed(BodyValueSeed {
                    state: state.clone(),
                })?;
                state.borrow_mut().payload_value_in_progress = false;
                object.insert(key, value);
            }
        }
        state.borrow_mut().payload_completed = true;
        Ok(Value::Object(object))
    }
}

fn telemetry_kind(state: &DiscriminatorState) -> Option<TelemetryKind> {
    if state.payload_count != 1 || !state.payload_is_object {
        return None;
    }
    match state.root_type.as_deref() {
        Some(TOKEN_USAGE_RECORD) => Some(TelemetryKind::TokenUsageRecord),
        Some(EVENT_MSG)
            if state.payload_type_count == 1
                && state.payload_type.as_deref() == Some(TOKEN_COUNT) =>
        {
            Some(TelemetryKind::TokenCount)
        }
        _ => None,
    }
}

fn truncated_telemetry_kind(state: &DiscriminatorState) -> Option<TelemetryKind> {
    if state.ambiguous_discriminator
        || state.ambiguous_header
        || !state.header_complete()
        || !state.root_type_before_payload
        || state.payload_count != 1
        || !state.payload_is_object
        || !state.payload_body_started
        || !state.payload_value_in_progress
        || state.payload_completed
    {
        return None;
    }
    match state.root_type.as_deref() {
        Some(TOKEN_USAGE_RECORD) => Some(TelemetryKind::TokenUsageRecord),
        Some(EVENT_MSG)
            if state.payload_type_count == 1
                && state.payload_type.as_deref() == Some(TOKEN_COUNT)
                && !state.payload_type_after_body =>
        {
            Some(TelemetryKind::TokenCount)
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "telemetry_discard_tests.rs"]
mod tests;
