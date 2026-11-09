//! Durable MCP elicitation tickets and their frame-owned answer state.

use crate::adapters::tui::presentation::text_field::TextField;
use crate::domain::models::{AgentId, ArtifactId};
use serde_json::{Map, Value};

use super::{
    Arrival, DecisionDraft, DecisionKey, DecisionKind, DecisionSubject, Raised, Responder,
    StaleCopy, Validity,
};
use crate::adapters::tui::state::TuiState;

pub const MISSED_TICKET: StaleCopy = StaleCopy("Missed: the tool stopped waiting for this answer.");

/// Refresh metadata for one assignment, or reset disclosure for a new incarnation.
pub fn raise_ticket(state: &mut TuiState, payload: TicketPayload, responder: Responder) -> Raised {
    let source = format!(
        "{}:{}{}",
        payload.node.as_str().len(),
        payload.node,
        payload.key
    );
    if let Some(item) = state
        .decisions
        .items
        .iter_mut()
        .find(|item| item.kind == DecisionKind::Ticket && item.source == source)
    {
        let new_incarnation = item.is_stale()
            || matches!(&item.subject, DecisionSubject::Ticket(existing)
                if existing.artifact != payload.artifact
                    || (existing.seq != 0 && payload.seq > existing.seq));
        if new_incarnation {
            item.summary =
                crate::domain::services::peer_text::sanitize_display_line(payload.message())
                    .chars()
                    .take(80)
                    .collect();
            item.subject = DecisionSubject::Ticket(Box::new(payload));
            item.validity = Validity::Live;
            item.responder = responder;
            item.deferred = false;
            let key = item.key();
            state.decisions.prepare(&key);
        } else if let DecisionSubject::Ticket(existing) = &mut item.subject
            && payload.seq != 0
        {
            existing.seq = payload.seq;
            existing.recorded_at_ms = payload.recorded_at_ms;
        }
        *state.decisions.body_layout.get_mut() = None;
        sort_tickets(state);
        return Raised::Waiting;
    }
    let summary: String =
        crate::domain::services::peer_text::sanitize_display_line(payload.message())
            .chars()
            .take(80)
            .collect();
    let draft = DecisionDraft::new(DecisionKind::Ticket, "")
        .source(source)
        .summary(summary)
        .responder(responder)
        .subject(DecisionSubject::Ticket(Box::new(payload)));
    let raised = super::raise(state, draft, Arrival::Accumulate);
    sort_tickets(state);
    raised
}

fn sort_tickets(state: &mut TuiState) {
    // Preserve the other kinds' slots while sorting ticket arrivals by journal sequence.
    let mut indices = Vec::new();
    for (index, item) in state.decisions.items.iter().enumerate() {
        if item.kind == DecisionKind::Ticket {
            indices.push(index);
        }
    }
    for index in 1..indices.len() {
        let mut cursor = index;
        while cursor > 0 {
            let seq = |at: usize| match &state.decisions.items[at].subject {
                DecisionSubject::Ticket(ticket) if ticket.seq != 0 => ticket.seq,
                _ => u64::MAX,
            };
            if seq(indices[cursor - 1]) <= seq(indices[cursor]) {
                break;
            }
            state
                .decisions
                .items
                .swap(indices[cursor - 1], indices[cursor]);
            cursor -= 1;
        }
    }
    state.needs_redraw = true;
}

/// Resolution is visible as a stale, close-only ticket, even if not currently open.
pub fn resolve_ticket(state: &mut TuiState, node: &AgentId, artifact: &ArtifactId, expired: bool) {
    for item in &mut state.decisions.items {
        if matches!(&item.subject, DecisionSubject::Ticket(ticket) if ticket.node == *node && ticket.artifact == *artifact)
        {
            item.validity = Validity::Stale(if expired {
                MISSED_TICKET
            } else {
                super::NO_LONGER_PENDING
            });
            state.needs_redraw = true;
        }
    }
}

pub fn set_ticket_error(state: &mut TuiState, key: &DecisionKey, error: String) {
    if let Some(DecisionSubject::Ticket(ticket)) =
        state.decisions.item_mut(key).map(|item| &mut item.subject)
    {
        ticket.error = Some(error);
        state.decisions.scroll = 0;
        *state.decisions.body_layout.get_mut() = None;
        state.needs_redraw = true;
    }
}

/// One control in the request's object schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TicketControl {
    Boolean(bool),
    Text(TextField),
    Number(TextField),
    Enum {
        choices: Vec<String>,
        selected: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketField {
    pub name: String,
    pub title: String,
    pub required: bool,
    pub control: TicketControl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TicketForm {
    Fields(Vec<TicketField>),
    Scalar(TicketControl),
    Raw(TextField),
}

/// The original envelope is retained independently of the displayed text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketPayload {
    pub node: AgentId,
    pub artifact: ArtifactId,
    pub key: String,
    pub envelope: Value,
    pub producer: AgentId,
    pub seq: u64,
    pub recorded_at_ms: i64,
    pub form: TicketForm,
    pub focused: usize,
    pub editing: bool,
    pub error: Option<String>,
}

impl TicketPayload {
    pub fn new(
        node: AgentId,
        artifact: ArtifactId,
        producer: AgentId,
        seq: u64,
        recorded_at_ms: i64,
        envelope: Value,
    ) -> Result<Self, String> {
        let key = envelope
            .get("key")
            .and_then(Value::as_str)
            .ok_or("ticket envelope has no request key")?
            .to_owned();
        let schema = envelope
            .pointer("/request/params/requestedSchema")
            .unwrap_or(&Value::Null);
        let form = TicketForm::from_schema(schema);
        Ok(Self {
            node,
            artifact,
            key,
            envelope,
            producer,
            seq,
            recorded_at_ms,
            form,
            focused: 0,
            editing: false,
            error: None,
        })
    }

    pub fn schema(&self) -> &Value {
        self.envelope
            .pointer("/request/params/requestedSchema")
            .unwrap_or(&Value::Null)
    }

    pub fn message(&self) -> &str {
        self.envelope
            .pointer("/request/params/message")
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    pub fn editing_field(&self) -> Option<&TextField> {
        if !self.editing {
            return None;
        }
        match &self.form {
            TicketForm::Raw(field) => Some(field),
            TicketForm::Scalar(TicketControl::Text(field) | TicketControl::Number(field)) => {
                Some(field)
            }
            TicketForm::Scalar(_) => None,
            TicketForm::Fields(fields) => match &fields.get(self.focused)?.control {
                TicketControl::Text(field) | TicketControl::Number(field) => Some(field),
                _ => None,
            },
        }
    }

    pub fn answer(&self) -> Result<Value, String> {
        let value = self.form.value()?;
        if matches!(&self.form, TicketForm::Raw(_)) {
            validate_raw(&value, self.schema())?;
        } else {
            validate(&value, self.schema())?;
        }
        Ok(value)
    }
}

impl TicketForm {
    pub fn from_schema(schema: &Value) -> Self {
        if schema.get("type").and_then(Value::as_str) != Some("object") {
            return control_for(schema)
                .map_or_else(|| Self::Raw(TextField::default()), Self::Scalar);
        }
        let Some(properties) = schema
            .get("properties")
            .and_then(Value::as_object)
            .filter(|_| schema.get("type").and_then(Value::as_str) == Some("object"))
        else {
            return Self::Raw(TextField::default());
        };
        if schema.get("oneOf").is_some()
            || schema.get("anyOf").is_some()
            || schema.get("allOf").is_some()
        {
            return Self::Raw(TextField::default());
        }
        let required = schema.get("required").and_then(Value::as_array);
        let mut fields = Vec::with_capacity(properties.len());
        for (name, property) in properties {
            if property.get("oneOf").is_some()
                || property.get("anyOf").is_some()
                || property.get("allOf").is_some()
            {
                return Self::Raw(TextField::default());
            }
            let Some(control) = control_for(property) else {
                return Self::Raw(TextField::default());
            };
            fields.push(TicketField {
                name: name.clone(),
                title: property
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or(name)
                    .to_owned(),
                required: required.is_some_and(|r| r.iter().any(|v| v.as_str() == Some(name))),
                control,
            });
        }
        Self::Fields(fields)
    }

    /// Bound the JSON representation before cloning any selected values into an answer.
    fn check_size(&self) -> Result<(), String> {
        struct Counter(usize);
        impl std::io::Write for Counter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self.0.saturating_add(bytes.len());
                if self.0 > super::question::ANSWER_MAX_BYTES {
                    return Err(std::io::Error::other("answer too large"));
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        fn write_control(out: &mut Counter, control: &TicketControl) -> Result<(), String> {
            let result = match control {
                TicketControl::Boolean(value) => serde_json::to_writer(out, value),
                TicketControl::Text(value) => serde_json::to_writer(out, value.as_str()),
                TicketControl::Number(value) => {
                    let number: Value = serde_json::from_str(value.as_str())
                        .map_err(|_| "Answer must be a number".to_owned())?;
                    serde_json::to_writer(out, &number)
                }
                TicketControl::Enum { choices, selected } => {
                    serde_json::to_writer(out, choices.get(*selected).ok_or("No choice selected")?)
                }
            };
            result.map_err(|_| "Answer exceeds the 65536-byte serialized limit".to_owned())
        }
        let mut out = Counter(0);
        let too_large = || "Answer exceeds the 65536-byte serialized limit".to_owned();
        use std::io::Write as _;
        match self {
            Self::Raw(field) => {
                let value: Value = serde_json::from_str(field.as_str())
                    .map_err(|error| format!("Invalid JSON: {error}"))?;
                serde_json::to_writer(&mut out, &value).map_err(|_| too_large())?;
            }
            Self::Scalar(control) => write_control(&mut out, control)?,
            Self::Fields(fields) => {
                out.write_all(b"{").map_err(|_| too_large())?;
                let mut first = true;
                for field in fields {
                    if matches!(&field.control, TicketControl::Number(value) if value.is_empty() && !field.required)
                    {
                        continue;
                    }
                    if !first {
                        out.write_all(b",").map_err(|_| too_large())?;
                    }
                    first = false;
                    serde_json::to_writer(&mut out, &field.name).map_err(|_| too_large())?;
                    out.write_all(b":").map_err(|_| too_large())?;
                    write_control(&mut out, &field.control)?;
                }
                out.write_all(b"}").map_err(|_| too_large())?;
            }
        }
        Ok(())
    }

    pub fn value(&self) -> Result<Value, String> {
        self.check_size()?;
        match self {
            Self::Raw(field) => serde_json::from_str(field.as_str())
                .map_err(|error| format!("Invalid JSON: {error}")),
            Self::Scalar(control) => control_value(control, "answer"),
            Self::Fields(fields) => {
                let mut object = Map::new();
                for field in fields {
                    let value = match &field.control {
                        TicketControl::Boolean(value) => Value::Bool(*value),
                        TicketControl::Text(value) => Value::String(value.as_str().to_owned()),
                        TicketControl::Number(value) if value.is_empty() && !field.required => {
                            continue;
                        }
                        TicketControl::Number(value) => {
                            serde_json::from_str::<Value>(value.as_str())
                                .map_err(|_| format!("{} must be a number", field.name))?
                        }
                        TicketControl::Enum { choices, selected } => Value::String(
                            choices.get(*selected).ok_or("No choice selected")?.clone(),
                        ),
                    };
                    object.insert(field.name.clone(), value);
                }
                Ok(Value::Object(object))
            }
        }
    }
}

fn control_for(schema: &Value) -> Option<TicketControl> {
    if schema.get("oneOf").is_some()
        || schema.get("anyOf").is_some()
        || schema.get("allOf").is_some()
    {
        return None;
    }
    if schema
        .get("pattern")
        .and_then(Value::as_str)
        .is_some_and(|pattern| regex::Regex::new(pattern).is_err())
    {
        return None;
    }
    match schema.get("type").and_then(Value::as_str)? {
        "boolean" => Some(TicketControl::Boolean(false)),
        "string" if schema.get("enum").is_some() => {
            let choices: Vec<String> = schema
                .get("enum")?
                .as_array()?
                .iter()
                .map(|value| value.as_str().map(str::to_owned))
                .collect::<Option<_>>()?;
            if choices.is_empty() {
                return None;
            }
            Some(TicketControl::Enum {
                choices,
                selected: 0,
            })
        }
        "string" => Some(TicketControl::Text(TextField::default())),
        "number" | "integer" => Some(TicketControl::Number(TextField::default())),
        _ => None,
    }
}

fn control_value(control: &TicketControl, name: &str) -> Result<Value, String> {
    match control {
        TicketControl::Boolean(value) => Ok(Value::Bool(*value)),
        TicketControl::Text(value) => Ok(Value::String(value.as_str().to_owned())),
        TicketControl::Number(value) => {
            serde_json::from_str(value.as_str()).map_err(|_| format!("{name} must be a number"))
        }
        TicketControl::Enum { choices, selected } => Ok(Value::String(
            choices.get(*selected).ok_or("No choice selected")?.clone(),
        )),
    }
}

/// Raw escape hatch mirrors the driver's existing minimum object checks; schema
/// constructs the driver does not interpret cannot make the ticket unanswerable.
fn validate_raw(value: &Value, schema: &Value) -> Result<(), String> {
    let Some(schema) = schema.as_object() else {
        return Ok(());
    };
    if schema
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind != "object")
    {
        return Ok(());
    }
    let object = value.as_object().ok_or("Answer must be an object")?;
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Ok(());
    };
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            if !object.contains_key(name) {
                return Err(format!("Missing required field: {name}"));
            }
        }
    }
    for (name, item) in object {
        let Some(property) = properties.get(name) else {
            continue;
        };
        if property
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|choices| !choices.contains(item))
        {
            return Err(format!("{name}: answer is not an allowed choice"));
        }
        let valid = match property.get("type").and_then(Value::as_str) {
            Some("boolean") => item.is_boolean(),
            Some("string") => item.is_string(),
            Some("integer") => item.is_i64() || item.is_u64(),
            Some("number") => item.is_number(),
            _ => true,
        };
        if !valid {
            return Err(format!("{name}: answer has the wrong type"));
        }
    }
    Ok(())
}

/// Validate the structural constraints used by MCP elicitation schemas.
pub fn validate(value: &Value, schema: &Value) -> Result<(), String> {
    if schema == &Value::Bool(false) {
        return Err("Schema rejects every answer".into());
    }
    if schema == &Value::Bool(true) || schema.is_null() {
        return Ok(());
    }
    if schema
        .get("const")
        .is_some_and(|constant| constant != value)
    {
        return Err("Answer does not match the required value".into());
    }
    if schema.get("type").is_some_and(|kind| !kind.is_string()) {
        return Err("Cannot validate this schema type".into());
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        return Err("Answer is not an allowed choice".into());
    }
    if let Some(kind) = schema.get("type").and_then(Value::as_str) {
        let matches = match kind {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "boolean" => value.is_boolean(),
            "number" => value.is_number(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "null" => value.is_null(),
            _ => false,
        };
        if !matches {
            return Err(format!("Answer must be {kind}"));
        }
    }
    if let Some(n) = value.as_f64() {
        if schema
            .get("minimum")
            .and_then(Value::as_f64)
            .is_some_and(|min| n < min)
            || schema
                .get("maximum")
                .and_then(Value::as_f64)
                .is_some_and(|max| n > max)
        {
            return Err("Answer is outside the permitted range".into());
        }
        if schema
            .get("exclusiveMinimum")
            .and_then(Value::as_f64)
            .is_some_and(|min| n <= min)
            || schema
                .get("exclusiveMaximum")
                .and_then(Value::as_f64)
                .is_some_and(|max| n >= max)
        {
            return Err("Answer is outside the permitted range".into());
        }
    }
    if let Some(s) = value.as_str() {
        let length = s.chars().count() as u64;
        if schema
            .get("minLength")
            .and_then(Value::as_u64)
            .is_some_and(|min| length < min)
            || schema
                .get("maxLength")
                .and_then(Value::as_u64)
                .is_some_and(|max| length > max)
        {
            return Err("Answer has an invalid length".into());
        }
        if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
            let regex = regex::Regex::new(pattern).map_err(|_| "Cannot validate schema pattern")?;
            if !regex.is_match(s) {
                return Err("Answer does not match the required pattern".into());
            }
        }
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(key) {
                    return Err(format!("Missing required field: {key}"));
                }
            }
        }
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for (key, item) in object {
                if let Some(property) = properties.get(key) {
                    validate(item, property).map_err(|error| format!("{key}: {error}"))?;
                } else if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                    return Err(format!("Unexpected field: {key}"));
                }
            }
        }
    }
    if let Some(values) = value.as_array() {
        let length = values.len() as u64;
        if schema
            .get("minItems")
            .and_then(Value::as_u64)
            .is_some_and(|min| length < min)
            || schema
                .get("maxItems")
                .and_then(Value::as_u64)
                .is_some_and(|max| length > max)
        {
            return Err("Answer has an invalid number of items".into());
        }
    }
    if let (Some(items), Some(values)) = (schema.get("items"), value.as_array()) {
        for item in values {
            validate(item, items)?;
        }
    }
    for (name, predicate) in [("allOf", 0), ("anyOf", 1), ("oneOf", 2)] {
        if let Some(variants) = schema.get(name).and_then(Value::as_array) {
            let count = variants
                .iter()
                .filter(|variant| validate(value, variant).is_ok())
                .count();
            if (predicate == 0 && count != variants.len())
                || (predicate == 1 && count == 0)
                || (predicate == 2 && count != 1)
            {
                return Err(format!("Answer does not satisfy {name}"));
            }
        }
    }
    if let Some(negated) = schema.get("not")
        && validate(value, negated).is_ok()
    {
        return Err("Answer matches a forbidden schema".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn story21_6_schema_controls_and_validation() {
        let schema = json!({"type":"object","required":["flag","name","amount","choice"],
            "properties":{"flag":{"type":"boolean"},"name":{"type":"string","minLength":2},
            "amount":{"type":"integer","minimum":1,"maximum":4},
            "choice":{"type":"string","enum":["a","b"]}}});
        let TicketForm::Fields(mut fields) = TicketForm::from_schema(&schema) else {
            panic!("structured form")
        };
        assert!(
            fields
                .iter()
                .any(|f| matches!(f.control, TicketControl::Boolean(_)))
        );
        assert!(
            fields
                .iter()
                .any(|f| matches!(f.control, TicketControl::Text(_)))
        );
        assert!(
            fields
                .iter()
                .any(|f| matches!(f.control, TicketControl::Number(_)))
        );
        assert!(
            fields
                .iter()
                .any(|f| matches!(f.control, TicketControl::Enum { .. }))
        );
        let valid = json!({"flag":true,"name":"ok","amount":2,"choice":"a"});
        for field in &mut fields {
            match &mut field.control {
                TicketControl::Boolean(value) => *value = true,
                TicketControl::Text(value) => value.set("ok"),
                TicketControl::Number(value) => value.set("2"),
                TicketControl::Enum { selected, .. } => *selected = 0,
            }
        }
        assert_eq!(TicketForm::Fields(fields).value().unwrap(), valid);
        assert!(validate(&valid, &schema).is_ok());
        for invalid in [
            json!({"flag":true,"name":"ok","amount":0,"choice":"a"}),
            json!({"flag":true,"name":"ok","amount":5,"choice":"a"}),
            json!({"flag":true,"name":"ok","amount":2.5,"choice":"a"}),
            json!({"flag":true,"name":"ok","amount":2,"choice":"c"}),
            json!({"flag":true,"name":"x","amount":2,"choice":"a"}),
        ] {
            assert!(validate(&invalid, &schema).is_err());
        }
        let number = json!({"type":"number","minimum":0.5,"maximum":1.5});
        assert!(matches!(
            TicketForm::from_schema(&number),
            TicketForm::Scalar(TicketControl::Number(_))
        ));
        assert!(validate(&json!(1.25), &number).is_ok());
        assert!(validate(&json!(1.75), &number).is_err());
    }

    #[test]
    fn story21_6_nested_schema_uses_answerable_raw_json() {
        let schema = json!({"type":"object","properties":{"nested":{"type":"object",
            "required":["count"],"properties":{"count":{"type":"number","minimum":1}}}}});
        let TicketForm::Raw(mut field) = TicketForm::from_schema(&schema) else {
            panic!("raw form")
        };
        field.set(r#"{"nested":{"count":2}}"#);
        let form = TicketForm::Raw(field);
        assert!(validate(&form.value().unwrap(), &schema).is_ok());
        assert!(validate(&json!({"nested":{"count":0}}), &schema).is_err());
    }

    #[test]
    fn story21_6_raw_escape_hatch_does_not_reject_unknown_keywords() {
        let schema = json!({"type":"object","properties":{"nested":{"type":"object","$ref":"#/defs/nested"}}});
        assert!(matches!(
            TicketForm::from_schema(&schema),
            TicketForm::Raw(_)
        ));
        assert!(validate_raw(&json!({"nested":{"value":true}}), &schema).is_ok());
        let constrained = json!({"type":"object","required":["choice"],
            "properties":{"choice":{"type":"string","enum":["a","b"]},"nested":{"type":"object"}}});
        assert!(validate_raw(&json!({"choice":"a","nested":{}}), &constrained).is_ok());
        assert!(validate_raw(&json!({"choice":"c","nested":{}}), &constrained).is_err());
        assert!(validate_raw(&json!({"nested":{}}), &constrained).is_err());
    }

    #[test]
    fn story21_6_answer_limit_counts_escaped_property_names_and_values() {
        let max = super::super::question::ANSWER_MAX_BYTES;
        let name = "\"";
        let empty = std::collections::BTreeMap::from([(name, "")]);
        let overhead = serde_json::to_vec(&empty).unwrap().len();
        for extra in [0, 1] {
            let form = TicketForm::Fields(vec![TicketField {
                name: name.to_owned(),
                title: String::new(),
                required: true,
                control: TicketControl::Enum {
                    choices: vec![format!(
                        "{}{}",
                        "\\".repeat((max - overhead) / 2 + extra),
                        "a".repeat((max - overhead) % 2)
                    )],
                    selected: 0,
                },
            }]);
            if extra == 0 {
                assert_eq!(
                    serde_json::to_vec(&form.value().unwrap()).unwrap().len(),
                    max
                );
            } else {
                assert!(form.value().unwrap_err().contains("serialized limit"));
            }
        }
    }
}
