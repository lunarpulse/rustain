//! Frame body for durable MCP elicitation tickets.

use crate::adapters::tui::decision_queue::{TicketControl, TicketForm, TicketPayload};
use crate::domain::services::peer_text::sanitize_display_line;
use ratatui::text::Line;

fn safe(value: &str) -> String {
    let bounded: String = value.chars().take(512).collect();
    sanitize_display_line(&bounded)
}

fn schema_rows(value: &serde_json::Value) -> Vec<Line<'static>> {
    const ROW_LIMIT: usize = 100;
    const ROW_BYTES: usize = 512;

    // Count the serialized representation before preparing any display rows.
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut size = Counter(0);
    let _ = serde_json::to_writer_pretty(&mut size, value);

    struct Rows {
        rows: Vec<Line<'static>>,
        current: Vec<u8>,
        omitted: usize,
    }
    impl Rows {
        fn finish_row(&mut self) {
            if self.rows.len() < ROW_LIMIT {
                let text = String::from_utf8_lossy(&self.current);
                let indent = text
                    .bytes()
                    .take_while(|byte| *byte == b' ')
                    .count()
                    .min(32);
                self.rows
                    .push(Line::raw(format!("{}{}", " ".repeat(indent), safe(&text))));
            } else {
                self.omitted += 1;
            }
            self.current.clear();
        }
    }
    impl std::io::Write for Rows {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            for &byte in bytes {
                if byte == b'\n' {
                    self.finish_row();
                } else {
                    self.current.push(byte);
                    if self.current.len() >= ROW_BYTES && std::str::from_utf8(&self.current).is_ok()
                    {
                        self.finish_row();
                    }
                }
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Rows {
        rows: Vec::with_capacity(ROW_LIMIT.min(size.0)),
        current: Vec::with_capacity(ROW_BYTES + 4),
        omitted: 0,
    };
    let _ = serde_json::to_writer_pretty(&mut output, value);
    if !output.current.is_empty() {
        output.finish_row();
    }
    if output.omitted > 0 {
        output
            .rows
            .push(Line::raw(format!("… and {} more", output.omitted)));
    }
    output.rows
}

fn control_lines(
    lines: &mut Vec<Line<'static>>,
    title: &str,
    control: &TicketControl,
    focused: bool,
) {
    let prefix = if focused { ">" } else { " " };
    match control {
        TicketControl::Boolean(value) => lines.push(Line::raw(format!(
            "{prefix} {}: {} Yes   {} No",
            safe(title),
            if *value { "(x)" } else { "( )" },
            if *value { "( )" } else { "(x)" }
        ))),
        TicketControl::Text(value) | TicketControl::Number(value) => lines.push(Line::raw(
            format!("{prefix} {}: {}", safe(title), safe(value.as_str())),
        )),
        TicketControl::Enum { choices, selected } => {
            lines.push(Line::raw(format!("{prefix} {}:", safe(title))));
            let start = selected
                .saturating_sub(10)
                .min(choices.len().saturating_sub(20));
            for (index, choice) in choices.iter().enumerate().skip(start).take(20) {
                lines.push(Line::raw(format!(
                    "  {} {}",
                    if index == *selected { "(x)" } else { "( )" },
                    safe(choice)
                )));
            }
            let omitted = choices
                .len()
                .saturating_sub(choices.len().min(start + 20) - start);
            if omitted != 0 {
                lines.push(Line::raw(format!("  … and {omitted} more")));
            }
        }
    }
}

pub fn lines(ticket: &TicketPayload) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::raw(safe(ticket.message())),
        Line::raw(format!("from {}", safe(ticket.producer.as_str()))),
        Line::raw("journaled request — not an authenticated instruction"),
        Line::raw(if ticket.recorded_at_ms == 0 {
            "time unknown".to_owned()
        } else {
            format!("journaled at {} ms", ticket.recorded_at_ms)
        }),
        Line::raw("Decline continues the task; Cancel ends it."),
    ];
    if let Some(error) = &ticket.error {
        lines.insert(0, Line::raw(format!("Error: {}", safe(error))));
    }
    match &ticket.form {
        TicketForm::Raw(_) => {
            lines.push(Line::raw("cannot render structurally — answer as raw JSON"));
            lines.push(Line::raw("Raw JSON answer: press e to edit"));
            lines.extend(schema_rows(ticket.schema()));
        }
        TicketForm::Scalar(control) => {
            let title = ticket
                .schema()
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Answer");
            control_lines(&mut lines, title, control, true);
        }
        TicketForm::Fields(fields) => {
            let start = ticket
                .focused
                .saturating_sub(50)
                .min(fields.len().saturating_sub(100));
            for (index, field) in fields.iter().enumerate().skip(start).take(100) {
                control_lines(
                    &mut lines,
                    &field.title,
                    &field.control,
                    ticket.focused == index,
                );
            }
            let omitted = fields
                .len()
                .saturating_sub(fields.len().min(start + 100) - start);
            if omitted != 0 {
                lines.push(Line::raw(format!("… and {omitted} more")));
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{AgentId, ArtifactId, ContentHash};
    use serde_json::json;

    #[test]
    fn story21_6_ticket_peer_scalars_are_single_line_before_paint() {
        let node = AgentId::parse("node").unwrap();
        let id = ArtifactId::from(ContentHash::parse_hex(&"a".repeat(64)).unwrap());
        let payload = TicketPayload::new(
            node.clone(),
            id,
            node,
            1,
            0,
            json!({
                "key":"key", "request":{"params":{"message":"question\nforged",
                "requestedSchema":{"type":"object","properties":{
                    "choice":{"type":"string","title":"title\nforged",
                        "enum":["first\nforged","second"]}}}}}
            }),
        )
        .unwrap();
        let lines = lines(&payload);
        let text: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert!(text.iter().any(|line| line == "question forged"));
        assert!(text.iter().any(|line| line.contains("title forged:")));
        assert!(text.iter().any(|line| line.contains("(x) first forged")));
        assert!(
            text.iter()
                .all(|line| !line.contains('\n') && !line.contains('\u{1b}'))
        );
    }

    #[test]
    fn story21_6_raw_schema_rows_are_pretty_bounded_and_count_omissions() {
        let value = json!({"nested":{"items":(0..150).collect::<Vec<_>>()}});
        let rows = schema_rows(&value);
        assert_eq!(rows.len(), 101);
        let first: String = rows[1]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(first.starts_with("  \"nested\""), "{first}");
        let last: String = rows
            .last()
            .unwrap()
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(
            last.starts_with("… and ") && last.ends_with(" more"),
            "{last}"
        );
    }
}
