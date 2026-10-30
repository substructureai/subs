// Adapted from OpenAI Codex, codex-rs/code-mode-protocol/src/json_schema_types_tests.rs.
// Copyright 2025 OpenAI. Licensed under the Apache License, Version 2.0.

use super::*;
use serde_json::json;

fn render(schema: &JsonValue) -> Rendered {
    render_json_schema_to_typescript(schema, "", &mut BTreeSet::new())
}

fn body(schema: &JsonValue) -> String {
    render(schema).body
}

#[test]
fn renders_recursive_local_refs_as_named_types() {
    let schema = json!({
        "type": "object",
        "properties": {
            "clauses": {
                "type": "array",
                "items": { "$ref": "#/$defs/Boolean~1Clause~0v1" }
            }
        },
        "$defs": {
            "Boolean/Clause~v1": {
                "type": "object",
                "properties": {
                    "query": { "$ref": "#/$defs/Query" }
                }
            },
            "Query": {
                "oneOf": [
                    { "type": "string" },
                    {
                        "type": "object",
                        "properties": {
                            "clauses": {
                                "type": "array",
                                "items": { "$ref": "#/$defs/Boolean~1Clause~0v1" }
                            }
                        }
                    }
                ]
            }
        }
    });

    assert_eq!(
        render_json_schema_to_typescript(&schema, "find", &mut BTreeSet::new()),
        Rendered {
            types: vec![
                "type FindQuery = string | { clauses?: Array<FindBooleanClauseV1>; };".to_string(),
                "type FindBooleanClauseV1 = { query?: FindQuery; };".to_string(),
            ],
            body: "{ clauses?: Array<FindBooleanClauseV1>; }".to_string(),
        }
    );
}

#[test]
fn renders_ref_siblings_uri_fragments_and_all_of_precedence() {
    assert_eq!(
        body(&json!({
            "$ref": "#/$defs/Label",
            "enum": ["A"],
            "$defs": { "Label": { "type": "string" } }
        })),
        r#"(Label) & ("A")"#
    );
    assert_eq!(
        render(&json!({
            "$ref": "#/$defs/Foo%20Bar",
            "$defs": { "Foo Bar": { "type": "string", "description": "A name." } }
        })),
        Rendered {
            types: vec!["// A name.\ntype FooBar = string;".to_string()],
            body: "FooBar".to_string(),
        }
    );
    assert_eq!(
        body(&json!({
            "allOf": [
                { "$ref": "#/$defs/Choice" },
                { "type": "object", "properties": { "value": { "type": "string" } } }
            ],
            "$defs": {
                "Choice": { "oneOf": [{ "type": "string" }, { "type": "number" }] }
            }
        })),
        "Choice & { value?: string; }"
    );
}

#[test]
fn renders_an_objects_properties_beside_its_combinators() {
    assert_eq!(
        body(&json!({
            "type": "object",
            "properties": { "operator": { "enum": ["in", "exists"] }, "value": {} },
            "required": ["operator"],
            "allOf": [
                { "if": { "properties": { "operator": { "const": "in" } } },
                  "then": { "required": ["value"] } }
            ]
        })),
        r#"{ operator: "in" | "exists"; value?: unknown; }"#
    );
    assert_eq!(
        body(&json!({
            "type": "object",
            "properties": { "id": { "type": "string" } },
            "oneOf": [{ "required": ["id"] }, { "properties": { "name": { "type": "string" } } }]
        })),
        "{ id?: string; } & ({ [key: string]: unknown; } | { name?: string; })"
    );
    assert_eq!(
        body(&json!({ "type": "string", "oneOf": [{ "format": "date" }, { "format": "uuid" }] })),
        "string"
    );
}

#[test]
fn leaves_local_refs_under_nested_schema_resources_unresolved() {
    let schema = json!({
        "$defs": {
            "Choice": { "type": "string" }
        },
        "type": "object",
        "properties": {
            "nested": {
                "$id": "urn:nested",
                "$defs": {
                    "Choice": { "type": "number" }
                },
                "type": "object",
                "properties": {
                    "value": { "$ref": "#/$defs/Choice" }
                }
            }
        }
    });

    assert_eq!(body(&schema), "{ nested?: { value?: unknown; }; }");
}

#[test]
fn declares_a_repeated_ref_once_and_leaves_dangling_refs_unknown() {
    let rendered = render(&json!({
        "type": "object",
        "properties": {
            "a": { "$ref": "#/$defs/Item" },
            "b": { "$ref": "#/$defs/Item" },
            "c": { "$ref": "#/$defs/Missing" }
        },
        "$defs": {
            "Item": {
                "type": "object",
                "properties": {
                    "value": {
                        "type": "string",
                        "description": "x".repeat(DEFAULT_INPUT_SCHEMA_MAX_BYTES / 2)
                    }
                }
            }
        }
    }));
    assert_eq!(rendered.types.len(), 1);
    assert_eq!(rendered.body, "{ a?: Item; b?: Item; c?: unknown; }");
}

#[test]
fn names_stay_unique_across_schemas() {
    let schema = json!({ "$ref": "#/$defs/Item", "$defs": { "Item": { "type": "string" } } });
    let mut taken = BTreeSet::new();
    let first = render_json_schema_to_typescript(&schema, "tool", &mut taken);
    let second = render_json_schema_to_typescript(&schema, "tool", &mut taken);
    assert_eq!(
        (first.body.as_str(), second.body.as_str()),
        ("ToolItem", "ToolItem2")
    );
}

#[test]
fn oversized_ref_literal_exhausts_render_work_budget() {
    let schema = json!({
        "$ref": "#/$defs/Value",
        "$defs": {
            "Value": {
                "const": "x".repeat(DEFAULT_INPUT_SCHEMA_MAX_BYTES * RENDER_WORK_MULTIPLIER)
            }
        }
    });

    let mut renderer = JsonSchemaTypeRenderer::new(&schema, DEFAULT_INPUT_SCHEMA_MAX_BYTES);
    assert_eq!(renderer.render(&schema), "unknown");
    assert!(renderer.render_work_budget_exhausted);
}

#[test]
fn rendered_schema_has_a_hard_size_cap() {
    let description = "x".repeat(DEFAULT_INPUT_SCHEMA_MAX_BYTES);
    let schema = json!({
        "type": "object",
        "properties": {
            "value": { "type": "string", "description": description }
        }
    });

    assert_eq!(body(&schema), "unknown");
}
