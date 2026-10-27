//! The new-flag builder: the flag's details, then its attributes (each with a type, its
//! constraints and its value in the chosen environment), listed for review. The module
//! turns the result into a change deployed like any edit.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use serde_json::Value;

use crate::aws::appconfig::{self as ac, Constraints, MAX_ATTRIBUTES, NewAttribute, NewFlag};
use crate::tui::form::{Field, Form, FormEvent};
use crate::tui::picker::{Item, Outcome, Picker};
use crate::tui::{theme, widgets};
use crate::util::ellipsize;

const ON_OFF: &[&str] = &["on", "off"];
const NO_YES: &[&str] = &["no", "yes"];
/// A boolean attribute's value; `—` leaves it without one.
const BOOLEAN_VALUE: &[&str] = &["—", "false", "true"];

/// Attribute types, with what their constraints allow.
const KINDS: &[(&str, &str)] = &[
    ("string", "text · optionally one of a list, or matching a pattern"),
    ("number", "optionally between a minimum and a maximum"),
    ("boolean", "true or false"),
    ("string[]", "a list of texts · optionally each one of a list, or matching a pattern"),
    ("number[]", "a list of numbers · optionally each between a minimum and a maximum"),
];

/// What the module does after a key.
pub enum Step {
    Pending,
    Cancel,
    /// The flag is complete: build the change and choose a strategy.
    Review,
}

enum Sub {
    Details(Form),
    Kind(Picker),
    Attribute { form: Form, index: Option<usize>, kind: &'static str },
}

pub struct Builder {
    /// The environment column it goes to, and the version it is built on.
    pub env: usize,
    pub base: Option<i64>,
    pub flag: NewFlag,
    /// `Feature flags · Development`.
    title: String,
    /// `Development · now v4`.
    from: String,
    /// Keys the profile already uses.
    taken: Vec<String>,
    /// Rows: the flag, its attributes, "add attribute", "review".
    cursor: usize,
    sub: Option<Sub>,
    error: Option<String>,
    /// Esc was pressed once: the next one discards the flag.
    leaving: bool,
}

fn type_hint(kind: &str) -> &'static str {
    match kind {
        "number" => "a number",
        "string[]" => "a, b, c",
        "number[]" => "1, 2, 3",
        _ => "text",
    }
}

fn list(values: &[Value]) -> String {
    values.iter().map(ac::value_text).collect::<Vec<_>>().join(", ")
}

impl Builder {
    pub fn new(title: String, env: usize, base: Option<i64>, from: String, taken: Vec<String>) -> Builder {
        let mut b = Builder {
            env,
            base,
            flag: NewFlag::default(),
            title,
            from,
            taken,
            cursor: 0,
            sub: None,
            error: None,
            leaving: false,
        };
        b.sub = Some(Sub::Details(b.details_form()));
        b
    }

    /// Shows an error (from AppConfig, for example) on the list.
    pub fn fail(&mut self, error: impl Into<String>) {
        self.sub = None;
        self.error = Some(error.into());
    }

    fn details_form(&self) -> Form {
        let f = &self.flag;
        let mut form = Form::new(
            format!("New flag · {}", self.title),
            vec![
                Field::fixed("Environment", self.from.clone()),
                Field::text("Key", f.key.clone()).hint("lowercase, e.g. new_checkout"),
                Field::text("Name", f.name.clone()).optional("the key"),
                Field::text("Description", f.description.clone()).optional("optional"),
                Field::choice("Enabled", ON_OFF, ac::on_off(f.enabled)),
                Field::choice("Short-term", NO_YES, if f.retire_by.is_some() { "yes" } else { "no" }),
                Field::text("Retire by", f.retire_by.clone().unwrap_or_default()).optional("YYYY-MM-DD, for a short-term flag"),
            ],
        )
        .submit_label("next");
        form.note = Some("The key cannot change once the flag exists. A short-term flag is only marked: AppConfig does not turn it off.".into());
        form
    }

    fn apply_details(&mut self, form: &Form) -> Result<(), String> {
        let mut flag = self.flag.clone();
        flag.key = form.get("Key").trim().to_string();
        flag.name = form.get("Name").trim().to_string();
        flag.description = form.get("Description").trim().to_string();
        flag.enabled = form.get("Enabled") == "on";
        let retire = form.get("Retire by").trim();
        flag.retire_by = (form.get("Short-term") == "yes" || !retire.is_empty()).then(|| retire.to_string());
        if self.taken.contains(&flag.key) {
            return Err(format!("This profile already has a flag {}: Enter on its cell sets it here", flag.key));
        }
        flag.validate()?;
        self.flag = flag;
        Ok(())
    }

    fn attribute_form(&self, kind: &'static str, attr: Option<&NewAttribute>) -> Form {
        let a = attr.cloned().unwrap_or_default();
        let c = &a.constraints;
        let mut fields = vec![
            Field::text("Name", a.name.clone()).hint("lowercase, e.g. rollout_percent"),
            Field::fixed("Type", kind),
            Field::choice("Required", NO_YES, if c.required { "yes" } else { "no" }),
        ];
        fields.push(if kind == "boolean" {
            let value = match a.value {
                Some(Value::Bool(true)) => "true",
                Some(Value::Bool(false)) => "false",
                _ => "—",
            };
            Field::choice("Value", BOOLEAN_VALUE, value)
        } else {
            Field::text("Value", a.value.as_ref().map(ac::value_text).unwrap_or_default()).optional(type_hint(kind))
        });
        match kind {
            "string" | "string[]" => {
                fields.push(Field::text("Allowed values", list(&c.allowed)).optional("a, b, c — or a pattern"));
                fields.push(Field::text("Pattern", c.pattern.clone().unwrap_or_default()).optional("a regular expression"));
            }
            "number" | "number[]" => {
                let n = |v: Option<i64>| v.map(|n| n.to_string()).unwrap_or_default();
                fields.push(Field::text("Minimum", n(c.minimum)).optional("a whole number"));
                fields.push(Field::text("Maximum", n(c.maximum)).optional("a whole number"));
            }
            _ => {}
        }
        fields.push(Field::text("Description", a.description.clone()).optional("optional"));
        let verb = if attr.is_some() { "Edit attribute" } else { "New attribute" };
        let mut form = Form::new(format!("{verb} · {} · {kind}", self.flag.key), fields).submit_label("save");
        form.note = Some(if attr.is_some() {
            "The value is what this environment gets. To change the type, remove the attribute (d) and add it again.".into()
        } else {
            "The value is what this environment gets; AppConfig checks it against the constraints.".into()
        });
        form
    }

    fn read_attribute(&self, form: &Form, kind: &'static str, index: Option<usize>) -> Result<NewAttribute, String> {
        let name = form.get("Name").trim().to_string();
        if self.flag.attributes.iter().enumerate().any(|(i, a)| a.name == name && Some(i) != index) {
            return Err(format!("{} already has an attribute {name}", self.flag.key));
        }
        let mut constraints = Constraints::of(kind);
        constraints.required = form.get("Required") == "yes";
        if matches!(kind, "string" | "string[]") {
            constraints.allowed = form
                .get("Allowed values")
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| Value::String(s.to_string()))
                .collect();
            constraints.pattern = Some(form.get("Pattern").trim().to_string()).filter(|p| !p.is_empty());
        }
        if matches!(kind, "number" | "number[]") {
            let whole = |label: &str| -> Result<Option<i64>, String> {
                match form.get(label).trim() {
                    "" => Ok(None),
                    text => text.parse().map(Some).map_err(|_| format!("{label} takes a whole number")),
                }
            };
            constraints.minimum = whole("Minimum")?;
            constraints.maximum = whole("Maximum")?;
        }
        let value = match form.get("Value").trim() {
            "" | "—" => None,
            text => Some(ac::parse_value(text, Some(kind), None).map_err(|e| format!("Value: {e}"))?),
        };
        let attribute = NewAttribute { name, description: form.get("Description").trim().to_string(), constraints, value };
        attribute.validate()?;
        Ok(attribute)
    }

    fn add(&mut self) {
        if self.flag.attributes.len() >= MAX_ATTRIBUTES {
            self.error = Some(format!("A flag takes at most {MAX_ATTRIBUTES} attributes"));
            return;
        }
        let items = KINDS.iter().map(|(k, about)| Item::new(*k, *about)).collect();
        self.sub = Some(Sub::Kind(Picker::new(format!("Attribute type · {}", self.flag.key), "next", items, false)));
    }

    fn rows(&self) -> usize {
        self.flag.attributes.len() + 3
    }

    pub fn handle(&mut self, key: KeyEvent) -> Step {
        if let Some(sub) = self.sub.take() {
            match sub {
                Sub::Details(mut form) => match form.handle(key) {
                    // Nothing to go back to before the flag has a key.
                    FormEvent::Cancel if self.flag.key.is_empty() => return Step::Cancel,
                    FormEvent::Cancel => {}
                    FormEvent::Pending => self.sub = Some(Sub::Details(form)),
                    FormEvent::Submit => match self.apply_details(&form) {
                        Ok(()) if self.cursor == 0 && self.flag.attributes.is_empty() => self.cursor = 1,
                        Ok(()) => {}
                        Err(e) => {
                            form.fail(e);
                            self.sub = Some(Sub::Details(form));
                        }
                    },
                },
                Sub::Kind(mut picker) => match picker.handle(key) {
                    Outcome::Cancel => {}
                    Outcome::Pending => self.sub = Some(Sub::Kind(picker)),
                    Outcome::Done(chosen) => {
                        let kind = KINDS[chosen[0]].0;
                        self.sub = Some(Sub::Attribute { form: self.attribute_form(kind, None), index: None, kind });
                    }
                },
                Sub::Attribute { mut form, index, kind } => match form.handle(key) {
                    FormEvent::Cancel => {}
                    FormEvent::Pending => self.sub = Some(Sub::Attribute { form, index, kind }),
                    FormEvent::Submit => match self.read_attribute(&form, kind, index) {
                        Ok(attribute) => match index {
                            Some(i) => self.flag.attributes[i] = attribute,
                            None => {
                                self.flag.attributes.push(attribute);
                                self.cursor = self.flag.attributes.len();
                            }
                        },
                        Err(e) => {
                            form.fail(e);
                            self.sub = Some(Sub::Attribute { form, index, kind });
                        }
                    },
                },
            }
            return Step::Pending;
        }
        let n = self.flag.attributes.len();
        let leaving = std::mem::take(&mut self.leaving);
        self.error = None;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.cursor = (self.cursor + 1).min(self.rows() - 1),
            KeyCode::Char('a') => self.add(),
            KeyCode::Char('e') => self.sub = Some(Sub::Details(self.details_form())),
            KeyCode::Char('d') | KeyCode::Delete if (1..=n).contains(&self.cursor) => {
                self.flag.attributes.remove(self.cursor - 1);
            }
            KeyCode::Enter => match self.cursor {
                0 => self.sub = Some(Sub::Details(self.details_form())),
                c if c <= n => {
                    let attr = &self.flag.attributes[c - 1];
                    let kind = ac::ATTRIBUTE_TYPES.iter().copied().find(|k| Some(*k) == attr.constraints.kind.as_deref()).unwrap_or("string");
                    self.sub = Some(Sub::Attribute { form: self.attribute_form(kind, Some(attr)), index: Some(c - 1), kind });
                }
                c if c == n + 1 => self.add(),
                _ => return Step::Review,
            },
            KeyCode::Esc if leaving => return Step::Cancel,
            KeyCode::Esc => self.leaving = true,
            _ => {}
        }
        Step::Pending
    }

    pub fn paste(&mut self, text: &str) {
        match &mut self.sub {
            Some(Sub::Details(form)) | Some(Sub::Attribute { form, .. }) => form.paste(text),
            Some(Sub::Kind(picker)) => picker.paste(text),
            None => {}
        }
    }

    pub fn hints(&self) -> Vec<(&'static str, &'static str)> {
        match &self.sub {
            Some(Sub::Details(_)) | Some(Sub::Attribute { .. }) => vec![("Tab", "next field"), ("←→", "choose"), ("Enter", "next"), ("Esc", "back")],
            Some(Sub::Kind(_)) => vec![("↑↓", "type"), ("Enter", "next"), ("Esc", "back")],
            None => vec![("↑↓", "move"), ("Enter", "open"), ("a", "add attribute"), ("d", "remove"), ("e", "flag details"), ("Esc", "discard")],
        }
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect, color: Color) {
        let n = self.flag.attributes.len();
        let mark = |row: usize, style: Style| if row == self.cursor && self.sub.is_none() { style.patch(theme::selected()) } else { style };
        let flag = &self.flag;
        let mut lines = vec![Line::raw("")];
        if self.sub.as_ref().is_some_and(|s| matches!(s, Sub::Details(_))) && flag.key.is_empty() {
            // The details form opens first: nothing to list behind it yet.
        } else {
            lines.push(Line::styled(" Flag", theme::muted()));
            let on = flag.enabled;
            lines.push(Line::from(vec![
                Span::styled(format!("   {} ", flag.name()), mark(0, theme::bold())),
                Span::styled(format!("({})", flag.key), mark(0, theme::muted())),
                Span::styled(format!("  {}", ac::on_off(on)), mark(0, Style::new().fg(if on { theme::OK } else { theme::MUTED }).add_modifier(Modifier::BOLD))),
            ]));
            if !flag.description.is_empty() {
                lines.push(Line::styled(format!("   {}", flag.description), theme::muted()));
            }
            match flag.retire_by.as_deref() {
                Some("") => lines.push(Line::styled("   short-term flag", theme::muted())),
                Some(date) => lines.push(Line::styled(format!("   short-term flag · retire by {date}"), theme::muted())),
                None => {}
            }
            lines.push(Line::raw(""));
            lines.push(Line::styled(format!(" Attributes · {n} of {MAX_ATTRIBUTES} at most"), theme::muted()));
            let name_w = flag.attributes.iter().map(|a| a.name.chars().count()).max().unwrap_or(4).clamp(4, 24) + 2;
            for (i, a) in flag.attributes.iter().enumerate() {
                let value = a.value.as_ref().map_or_else(|| "—".to_string(), ac::value_text);
                lines.push(Line::from(vec![
                    Span::styled(format!("   {:<name_w$}", ellipsize(&a.name, name_w - 1)), mark(i + 1, theme::bold())),
                    Span::styled(format!("{:<40}", ellipsize(&a.constraints.summary(), 39)), mark(i + 1, theme::muted())),
                    Span::styled(format!(" = {}", ellipsize(&value, 24)), mark(i + 1, Style::new())),
                ]));
            }
            lines.push(Line::styled("   + add attribute", mark(n + 1, theme::accent())));
            lines.push(Line::raw(""));
            let env = self.from.split(" · ").next().unwrap_or_default().to_string();
            lines.push(Line::styled(format!("   ▶ Review and deploy to {env}"), mark(n + 2, theme::bold())));
            if let Some(e) = &self.error {
                lines.push(Line::raw(""));
                lines.push(Line::styled(format!("✗ {e}"), Style::new().fg(theme::ERR)));
            } else if self.leaving {
                lines.push(Line::raw(""));
                lines.push(Line::styled("Esc again discards this flag", Style::new().fg(theme::WARN)));
            }
            lines.push(Line::raw(""));
            lines.push(widgets::hints_line(&[("Enter", "open"), ("a", "add attribute"), ("d", "remove"), ("Esc", "discard")]));
            let rect = widgets::popup_area(area, 92, widgets::wrapped_height(&lines, 88) + 2);
            widgets::popup(f, rect, &format!("New flag · {}", self.title), color, Text::from(lines));
        }
        match &mut self.sub {
            Some(Sub::Details(form)) | Some(Sub::Attribute { form, .. }) => form.render(f, area, color),
            Some(Sub::Kind(picker)) => {
                let rect = widgets::popup_area(area, 84, picker.height(area.height.saturating_sub(4)));
                picker.render(f, rect, color);
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;
    use serde_json::json;

    fn press(b: &mut Builder, code: KeyCode) -> Step {
        b.handle(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn typing(b: &mut Builder, text: &str) {
        for c in text.chars() {
            press(b, KeyCode::Char(c));
        }
    }

    fn tab(b: &mut Builder, times: usize) {
        for _ in 0..times {
            press(b, KeyCode::Tab);
        }
    }

    fn builder() -> Builder {
        Builder::new("checkout-flags · Development".into(), 0, Some(4), "Development · now v4".into(), vec!["dark_mode".into()])
    }

    #[test]
    fn builds_a_flag_with_attributes() {
        let mut b = builder();
        // Details: a taken key is refused, then a new one.
        typing(&mut b, "dark_mode");
        tab(&mut b, 5);
        assert!(matches!(press(&mut b, KeyCode::Enter), Step::Pending));
        assert!(b.flag.key.is_empty());
        for _ in 0..5 {
            press(&mut b, KeyCode::Up);
        }
        for _ in 0.."dark_mode".len() {
            press(&mut b, KeyCode::Backspace);
        }
        typing(&mut b, "new_search");
        tab(&mut b, 1);
        typing(&mut b, "New search");
        tab(&mut b, 4);
        press(&mut b, KeyCode::Enter);
        assert_eq!(b.flag.key, "new_search");
        assert_eq!(b.flag.name(), "New search");
        assert!(!b.flag.enabled);
        assert_eq!(b.cursor, 1);

        // A number attribute, required, 0..100, value 150 refused then 10.
        press(&mut b, KeyCode::Char('a'));
        press(&mut b, KeyCode::Down);
        press(&mut b, KeyCode::Enter);
        typing(&mut b, "rollout");
        tab(&mut b, 1);
        press(&mut b, KeyCode::Right);
        tab(&mut b, 1);
        typing(&mut b, "150");
        tab(&mut b, 1);
        typing(&mut b, "0");
        tab(&mut b, 1);
        typing(&mut b, "100");
        tab(&mut b, 1);
        press(&mut b, KeyCode::Enter);
        assert!(b.flag.attributes.is_empty());
        for _ in 0..3 {
            press(&mut b, KeyCode::Up);
        }
        press(&mut b, KeyCode::Backspace);
        press(&mut b, KeyCode::Backspace);
        tab(&mut b, 3);
        press(&mut b, KeyCode::Enter);
        assert_eq!(b.flag.attributes.len(), 1);
        let a = &b.flag.attributes[0];
        assert_eq!((a.name.as_str(), a.value.clone()), ("rollout", Some(json!(1))));
        assert_eq!(a.constraints.summary(), "number · required · 0..100");

        // A list of allowed texts.
        press(&mut b, KeyCode::Char('a'));
        typing(&mut b, "string[]");
        press(&mut b, KeyCode::Enter);
        typing(&mut b, "carriers");
        tab(&mut b, 2);
        typing(&mut b, "dhl, ups");
        tab(&mut b, 1);
        typing(&mut b, "dhl, fedex");
        tab(&mut b, 2);
        press(&mut b, KeyCode::Enter);
        assert!(b.flag.attributes.len() == 1, "ups is not allowed");
        for _ in 0..3 {
            press(&mut b, KeyCode::Up);
        }
        for _ in 0..3 {
            press(&mut b, KeyCode::Backspace);
        }
        typing(&mut b, "fedex");
        tab(&mut b, 3);
        press(&mut b, KeyCode::Enter);
        assert_eq!(b.flag.attributes[1].value, Some(json!(["dhl", "fedex"])));
        assert_eq!(b.cursor, 2);

        // Remove the first, then review.
        press(&mut b, KeyCode::Up);
        press(&mut b, KeyCode::Char('d'));
        assert_eq!(b.flag.attributes.len(), 1);
        for _ in 0..5 {
            press(&mut b, KeyCode::Down);
        }
        assert!(matches!(press(&mut b, KeyCode::Enter), Step::Review));
        assert!(b.flag.validate().is_ok());
    }

    #[test]
    fn esc_twice_discards() {
        let mut b = builder();
        assert!(matches!(press(&mut b, KeyCode::Esc), Step::Cancel), "nothing typed yet");
        let mut b = builder();
        typing(&mut b, "quick");
        tab(&mut b, 5);
        press(&mut b, KeyCode::Enter);
        assert!(matches!(press(&mut b, KeyCode::Esc), Step::Pending));
        assert!(matches!(press(&mut b, KeyCode::Esc), Step::Cancel));
    }
}
