//! Effective schema for Responses function tools with omitted/null strictness.
use serde_json::{json, Value};

pub(crate) fn implicit_strict_schema(parameters: Option<&Value>) -> Option<Value> {
    let mut schema = parameters
        .filter(|value| !value.is_null())
        .cloned()
        .unwrap_or_else(|| json!({"type":"object"}));
    let root = schema.as_object_mut()?;
    if root.get("type").and_then(Value::as_str) != Some("object") || root.contains_key("anyOf") {
        return None;
    }
    normalize(&mut schema, 1, &mut SchemaBudget::default())?;
    Some(schema)
}

#[derive(Default)]
struct SchemaBudget {
    properties: usize,
    enum_values: usize,
    characters: usize,
}

impl SchemaBudget {
    fn add_characters(&mut self, count: usize) -> Option<()> {
        self.characters += count;
        (self.characters <= 120_000).then_some(())
    }
}

fn normalize(schema: &mut Value, depth: usize, budget: &mut SchemaBudget) -> Option<()> {
    if depth > 10 {
        return None;
    }
    let object = schema.as_object_mut()?;
    // Only infer strictness for the supported subset. Fallback preserves the
    // original schema instead of removing constraints to obtain strict mode.
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "type"
                | "properties"
                | "required"
                | "additionalProperties"
                | "items"
                | "anyOf"
                | "$ref"
                | "$defs"
                | "definitions"
                | "enum"
                | "const"
                | "title"
                | "description"
                | "$schema"
                | "format"
                | "pattern"
                | "minimum"
                | "maximum"
                | "exclusiveMinimum"
                | "exclusiveMaximum"
                | "multipleOf"
                | "minItems"
                | "maxItems"
        )
    }) {
        return None;
    }
    let types: Vec<_> = match object.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().map(Value::as_str).collect::<Option<_>>()?,
        None if object.contains_key("$ref") || object.contains_key("anyOf") => Vec::new(),
        _ => return None,
    };
    if types.iter().any(|kind| {
        !matches!(
            *kind,
            "object" | "array" | "string" | "number" | "integer" | "boolean" | "null"
        )
    }) {
        return None;
    }
    if let Some(format) = object.get("format") {
        if !matches!(
            format.as_str()?,
            "date-time"
                | "time"
                | "date"
                | "duration"
                | "email"
                | "hostname"
                | "ipv4"
                | "ipv6"
                | "uuid"
        ) {
            return None;
        }
    }
    if let Some(values) = object.get("enum") {
        let values = values.as_array()?;
        budget.enum_values += values.len();
        let characters: usize = values
            .iter()
            .filter_map(Value::as_str)
            .map(|value| value.chars().count())
            .sum();
        if budget.enum_values > 1000 || (values.len() > 250 && characters > 15_000) {
            return None;
        }
        budget.add_characters(characters)?;
    }
    if let Some(value) = object.get("const").and_then(Value::as_str) {
        budget.add_characters(value.chars().count())?;
    }
    let is_object = types.contains(&"object");
    let is_array = types.contains(&"array");
    if is_object {
        if object
            .get("additionalProperties")
            .is_some_and(|value| value != &Value::Bool(false))
        {
            return None;
        }
        let properties = object
            .entry("properties")
            .or_insert_with(|| json!({}))
            .as_object_mut()?;
        budget.properties += properties.len();
        if budget.properties > 5000 {
            return None;
        }
        for (name, child) in properties.iter_mut() {
            budget.add_characters(name.chars().count())?;
            normalize(child, depth + 1, budget)?;
        }
        let mut names: Vec<_> = properties.keys().cloned().collect();
        names.sort_unstable();
        if let Some(required) = object.get("required") {
            let required = required.as_array()?;
            for name in required {
                let name = name.as_str()?;
                if names
                    .binary_search_by(|candidate| candidate.as_str().cmp(name))
                    .is_err()
                {
                    return None;
                }
            }
        }
        object.insert("required".into(), json!(names));
        object.insert("additionalProperties".into(), json!(false));
    } else if object.contains_key("properties")
        || object.contains_key("required")
        || object.contains_key("additionalProperties")
    {
        return None;
    }
    if is_array {
        normalize(object.get_mut("items")?, depth + 1, budget)?;
    } else if object.contains_key("items") {
        return None;
    }
    for key in ["$defs", "definitions"] {
        if let Some(children) = object.get_mut(key) {
            for (name, child) in children.as_object_mut()?.iter_mut() {
                budget.add_characters(name.chars().count())?;
                normalize(child, depth, budget)?;
            }
        }
    }
    if let Some(children) = object.get_mut("anyOf") {
        let children = children.as_array_mut()?;
        if children.is_empty() {
            return None;
        }
        for child in children {
            normalize(child, depth, budget)?;
        }
    }
    Some(())
}
