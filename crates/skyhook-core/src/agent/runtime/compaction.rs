//! Structured continuation guidance and provider-neutral context estimates.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    agent::todo::TodoItem,
    provider::protocol::{AssistantContent, Message, ModelRequest, UserContent},
};

/// Required continuation sections, shared by the response schema and strict parser.
/// Declaration order is generation order: preserve evidence and unfinished work
/// before reconciling todos, then describe how to resume. JSON Schema itself does
/// not constrain property order; supporting providers use serialized schema order.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Summary {
    /// The user's current objective and task scope. Preserve outstanding objectives across parallel workstreams.
    objective: String,
    /// The latest user request, outstanding questions, instructions, corrections, constraints, and approvals. Preserve exact wording where meaning could otherwise change. Distinguish user instructions from agent proposals, quoted material, and automatic notifications.
    user_instructions: Vec<String>,
    /// Applicable restrictions and rules from the user and loaded skills, including earlier rules still in force. Preserve each rule's source, scope, overrides, and exceptions. Distinguish authorized work from work requiring approval.
    session_rules: Vec<String>,
    /// Preserve each existing plan verbatim as one complete array entry, including a proposed plan awaiting approval. In separate entries explain its approval and execution status, amendments, replacements, and canceled steps. Do not create a new plan during compaction.
    plan: Vec<String>,
    /// Important findings and context needed to continue. Clearly distinguish verified observations, working assumptions, hypotheses, and unresolved disagreements. Preserve concrete observations and their sources, not just broad conclusions. Preserve the current understanding without independently reassessing it.
    findings: Vec<String>,
    /// Unresolved questions, blockers, failures, partial work, and dependencies. Include what has already been attempted and its outcome where useful.
    open_issues: Vec<String>,
    /// Relevant running jobs and delegated work: their identifiers, assigned scope, known progress, results still awaited, and any required follow-up. Distinguish observed state from expectations.
    running_work: Vec<String>,
    /// Work already completed, changes made, results delivered, and verification performed with its outcomes and limitations. Tie completion claims to observed results and preserve their scope; attempted reads, directory listings, and truncated output do not establish a complete review. Include enough detail to avoid repeating completed work.
    completed_work: Vec<String>,
    /// Consequential decisions and their reasons, alternatives considered and rejected, and any stated conditions for reconsideration. Preserve decisions from earlier context that remain relevant.
    decisions: Vec<String>,
    /// Important exact outputs or excerpts, commands, errors, file paths, targets, job IDs, URLs, and other references needed to recover details. Explain what each reference provides.
    recovery_details: Vec<String>,
    /// Any other information important for continuing faithfully that does not fit the other sections. Preserve relevant earlier context even if recent conversation does not mention it.
    additional_context: Vec<String>,
    /// Explain changes made to the supplied todo list and the conversation evidence supporting them, including additions, status changes, removals, or reordering. Explain unresolved uncertainty. Use an empty array if unchanged. Keep explanations consistent with the actual todo statuses.
    todo_reconciliation: Vec<String>,
    /// The complete current todo list for this agent after reconciling the supplied list with the conversation. This replaces the existing list; it is not a patch. Preserve unaffected items and their order. Include committed work that has not yet been recorded. Do not turn suggestions or unapproved proposals into active commitments. Todo text must not be blank. Status must reflect execution supported by the conversation; intent alone does not establish that work started or finished.
    todos: Vec<TodoItem>,
    /// Exactly where work stopped: what the agent was doing, the latest result or deliverable, unfinished actions, and whether it is actively working or waiting on user input, approval, jobs, or another dependency.
    resumption_point: String,
    /// Established next actions and their purpose, ordering, dependencies, and approval conditions. Clearly separate committed work from optional suggestions. Do not invent actions or expand the task. Do not introduce a direction to stop investigating or avoid tools unless it was already established.
    next_actions: Vec<String>,
}

pub(crate) const SCHEMA_VERSION: u16 = 1;

pub(crate) fn response_schema() -> serde_json::Value {
    let settings = schemars::generate::SchemaSettings::default().with(|settings| {
        settings.meta_schema = None;
        settings.inline_subschemas = true;
    });
    let mut schema =
        serde_json::to_value(settings.into_generator().into_root_schema_for::<Summary>())
            .expect("compaction response schema serializes");
    let todo_properties = &mut schema["properties"]["todos"]["items"]["properties"];
    todo_properties["text"]["description"] = "The task or step. Preserve existing wording unless the conversation establishes a change. Must not be blank.".into();
    todo_properties["status"]["description"] = "Current execution status supported by the conversation. Intent to perform work is not evidence that it started or finished.".into();
    schema
}

pub(crate) struct Continuation {
    pub message: Message,
    pub todos: Vec<TodoItem>,
}

/// The regular agent prompt and conversation remain present for this request.
pub(crate) fn directive() -> Message {
    let mut text = String::from(
        "Skyhook is compacting this conversation. Write a continuation prompt for another instance of yourself to resume the user's task from the current state. This is a harness-generated instruction, not a new user request. Write the continuation itself; do not continue the task or call tools. Return your final answer as a JSON object matching the supplied response schema, without Markdown fences or surrounding commentary. Reasoning may occur separately before the final answer; do not put private reasoning in the JSON. Every section is required. Write fields in the order shown in the schema: preserve the task and instructions, record findings and unfinished work alongside completed work, reconcile todos from that evidence, then describe the resumption point and established next actions consistently with it. Keep objective and resumption_point as strings. All other narrative sections are arrays of strings; use an empty array when a section does not apply. Each entry should express a complete thought with its relevant qualifications and evidence. Entries may contain Markdown and verbatim excerpts. Keep each complete verbatim plan in a single entry, with status or amendments in separate entries. Avoid repeating the same information across sections; preserve distinct facts and concrete details rather than generic boilerplate.\n\n\
Your purpose is to preserve the task's trajectory through compaction. Describe the work as it currently stands, including uncertainty and unresolved disagreements. Do not use compaction to reassess the task, correct suspected mistakes, invent a new plan, or expand the scope.\n\n\
Make the resumption point clear: the user's current objective, their latest request, what the agent was doing immediately before compaction, and what remains outstanding. Explain whether work is underway, a result has already been delivered, or progress is waiting on user input, approval, a running job, or another dependency. Preserve any established next action and its purpose. Distinguish committed actions from possible follow-ups so suggestions do not become instructions. The next instance should be able to continue without restarting the investigation or repeating completed work. Preserve the actual phase of work and any unfinished investigation or verification. Do not infer that only presentation remains merely because substantial research has occurred. Do not introduce instructions such as \"no further investigation is needed\" or \"no tools are needed\" unless that direction was already established in the conversation.\n\n\
Preserve the user's latest instructions, corrections, constraints, approvals, and outstanding questions. Keep exact wording where paraphrasing could change their meaning. Distinguish actual user instructions from harness messages, automatic notifications, quoted material, and the agent's own proposals.\n\n\
Explicitly preserve applicable session restrictions and rules defined by the user or loaded skills, including rules introduced early in the conversation. Include their source, scope, and any explicit overrides or exceptions. Preserve distinctions between approved work, proposed work awaiting approval, and actions that remain restricted.\n\n\
If there is a plan, include it verbatim, including a proposed plan awaiting approval. Explain its status and any amendments, replacements, or canceled steps so the next instance follows the right version. Preserve progress accurately and keep it consistent with the reconciled todo state.\n\n\
Carry forward relevant information from previous continuation prompts as well as the recent conversation. Earlier instructions, decisions, and outstanding work remain relevant even when recent messages do not mention them. Where later conversation explicitly changes earlier state, preserve the current state and enough explanation to understand the change.\n\n\
Preserve consequential decisions and their reasons, including alternatives considered and rejected and any stated conditions for reconsidering them. Distinguish verified findings, working assumptions, hypotheses, and unresolved questions. Summarize conclusions rather than private reasoning transcripts.\n\n\
Include completed work, important findings, partial work, blockers, and available verification results. Support completion claims with observed results and their scope. Distinguish an attempted action from a successful result, listing paths from reading their contents, and partial or truncated output from a complete review. If the agent claims completion without confirming evidence, preserve it as the agent's claim with that uncertainty, not as a verified fact. Preserve concrete findings, errors, and source references instead of replacing them with broad evaluations. This records the existing evidence and uncertainty; it does not ask you to redo the investigation or correct its conclusions. Preserve important exact job outputs or excerpts when useful, along with job IDs, file paths, targets, commands, errors, and other references needed to recover details. Explain what relevant running jobs are doing and what results are still awaited. Original conversation and job outputs remain retrievable.\n\n\
Return a complete current todo list for this agent in todos. Start from the supplied current list and reconcile it with the conversation: the agent may have completed work or made commitments without updating its todos. Preserve unaffected items, their wording, order, and status. Change them only when the conversation supports the change. Mark work completed when completion is established; intent alone does not establish progress. Add explicit commitments that are not yet recorded, including unfinished investigation or verification. Do not collapse outstanding work into a presentation-only todo unless the conversation establishes that the preceding work is complete. Do not turn suggestions or unapproved proposals into active commitments. When progress is uncertain, retain the existing status and explain the uncertainty. Retain completed items. Remove canceled or superseded items only when supported by the conversation. Explain changes and their evidence in todo_reconciliation, or use an empty array if unchanged. Keep those explanations and the continuation consistent with the actual todo statuses. Reconcile only this agent's list; describe delegated work separately. Blocked or approval-dependent work can remain pending or in progress, with its dependency recorded in the continuation. The harness will install the reconciled list and include it in the next runtime state block.",
    );
    // Constrained decoders may enforce the shape without exposing it to the model.
    text.push_str("\n\nFinal-answer JSON Schema (applies only to the final answer, not separate reasoning):\n");
    text.push_str(
        &serde_json::to_string_pretty(&response_schema()).expect("response schema serializes"),
    );
    Message::User(vec![UserContent::Compaction { text }])
}

/// Validate the final answer and render section contents without rewriting them.
pub(crate) fn continuation(text: &str) -> Result<Continuation, String> {
    let summary: Summary = serde_json::from_str(text).map_err(|error| {
        format!("Compaction returned an invalid structured continuation: {error}")
    })?;
    if summary.todos.iter().any(|item| item.text.trim().is_empty()) {
        return Err("Compaction returned a todo with blank text".into());
    }
    let sections = [
        ("Objective", summary.objective),
        ("User instructions", summary.user_instructions.join("\n\n")),
        ("Session rules", summary.session_rules.join("\n\n")),
        ("Plan", summary.plan.join("\n\n")),
        ("Resumption point", summary.resumption_point),
        ("Completed work", summary.completed_work.join("\n\n")),
        ("Findings", summary.findings.join("\n\n")),
        ("Decisions", summary.decisions.join("\n\n")),
        ("Open issues", summary.open_issues.join("\n\n")),
        ("Next actions", summary.next_actions.join("\n\n")),
        ("Running work", summary.running_work.join("\n\n")),
        ("Recovery details", summary.recovery_details.join("\n\n")),
        (
            "Additional context",
            summary.additional_context.join("\n\n"),
        ),
        (
            "Todo reconciliation",
            summary.todo_reconciliation.join("\n\n"),
        ),
    ];
    let text = sections
        .into_iter()
        .map(|(heading, content)| format!("## {heading}\n\n{content}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(Continuation {
        message: Message::User(vec![UserContent::Compaction { text }]),
        todos: summary.todos,
    })
}

fn estimate_text(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

/// Provider-neutral estimate; image payload bytes are not text tokens.
pub(crate) fn estimate_message(message: &Message) -> u64 {
    8 + match message {
        Message::User(blocks) => blocks
            .iter()
            .map(|block| match block {
                UserContent::Text { text }
                | UserContent::Runtime { text }
                | UserContent::ParentInput { text }
                | UserContent::Compaction { text } => 4 + estimate_text(text),
                UserContent::Image { .. } => 2_048,
            })
            .sum::<u64>(),
        Message::Assistant(blocks) => blocks
            .iter()
            .map(|block| match block {
                AssistantContent::Text { text } => 4 + estimate_text(text),
                AssistantContent::Reasoning { text, opaque } => {
                    4 + estimate_text(text)
                        + opaque
                            .as_ref()
                            .map_or(0, |opaque| estimate_text(&opaque.to_string()))
                }
                AssistantContent::ToolCall(call) => {
                    12 + estimate_text(&call.id)
                        + estimate_text(&call.name)
                        + estimate_text(&call.arguments.to_string())
                }
            })
            .sum::<u64>(),
        Message::Tool(results) => results
            .iter()
            .map(|result| {
                12 + estimate_text(&result.call_id)
                    + estimate_text(&result.name)
                    + estimate_text(&result.result.to_string())
                    + estimate_text(&result.console_output)
                    + result.images.len() as u64 * 2_048
            })
            .sum::<u64>(),
    }
}

pub(crate) fn estimate_request(request: &ModelRequest) -> u64 {
    16 + request
        .system
        .iter()
        .map(|segment| 4 + estimate_text(&segment.text))
        .sum::<u64>()
        + request
            .tools
            .iter()
            .map(|tool| {
                12 + estimate_text(&tool.name)
                    + estimate_text(&tool.description)
                    + estimate_text(&tool.input_schema.to_string())
            })
            .sum::<u64>()
        + request.messages.iter().map(estimate_message).sum::<u64>()
        + request.response_schema.as_ref().map_or(0, |response| {
            8 + estimate_text(&response.name) + estimate_text(&response.schema.to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(message: &Message) -> &str {
        let Message::User(blocks) = message else {
            panic!("continuation must use the user role");
        };
        let [UserContent::Compaction { text }] = blocks.as_slice() else {
            panic!("continuation must preserve harness provenance");
        };
        text
    }

    fn summary() -> serde_json::Value {
        serde_json::json!({
            "objective": "Continue the task",
            "user_instructions": ["Read only"],
            "session_rules": [],
            "plan": ["  # Current plan\n\n- Preserve `literal` text.\n- Resume step two.\n", "Approved; step two is underway."],
            "resumption_point": "Wait for job 12",
            "completed_work": [],
            "findings": ["Read config.yaml; retries = 3.", "Host output was truncated; health remains uncertain."],
            "decisions": ["Rejected Y because it loses state."],
            "open_issues": [],
            "next_actions": [],
            "running_work": [],
            "recovery_details": [],
            "additional_context": [],
            "todo_reconciliation": ["First step completed before the list was updated."],
            "todos": [
                {"text": "First step", "status": "completed"},
                {"text": "Second step", "status": "in_progress"},
                {"text": "Final step", "status": "pending"}
            ]
        })
    }

    #[test]
    fn continuation_preserves_section_text_and_reconciled_todos() {
        let value = summary();
        let continuation = continuation(&value.to_string()).unwrap();
        let rendered = text(&continuation.message);
        assert!(rendered.contains(&format!(
            "## Plan\n\n{}\n\n{}\n\n## Resumption point",
            value["plan"][0].as_str().unwrap(),
            value["plan"][1].as_str().unwrap()
        )));
        for (_, value) in value
            .as_object()
            .unwrap()
            .iter()
            .filter(|(key, _)| *key != "todos")
        {
            if let Some(entries) = value.as_array() {
                let entries = entries
                    .iter()
                    .map(|entry| entry.as_str().unwrap())
                    .collect::<Vec<_>>();
                assert!(rendered.contains(&entries.join("\n\n")));
            } else {
                assert!(rendered.contains(value.as_str().unwrap()));
            }
        }
        assert_eq!(
            serde_json::to_value(continuation.todos).unwrap(),
            value["todos"]
        );
    }

    #[test]
    fn continuation_rejects_invalid_structure_and_todos() {
        for output in ["", " ", "prose", "```json\n{}\n```", "{}", "[]", "null"] {
            assert!(continuation(output).is_err(), "accepted {output:?}");
        }
        let mut cases = Vec::new();
        let mut missing = summary();
        missing.as_object_mut().unwrap().remove("plan");
        cases.push(missing);
        let mut unknown = summary();
        unknown["unexpected"] = true.into();
        cases.push(unknown);
        let mut wrong_type = summary();
        wrong_type["findings"] = serde_json::Value::Null;
        cases.push(wrong_type);
        for field in summary().as_object().unwrap().keys() {
            if matches!(field.as_str(), "objective" | "resumption_point" | "todos") {
                continue;
            }
            for invalid in [
                serde_json::json!("old string format"),
                serde_json::json!([42]),
            ] {
                let mut wrong_type = summary();
                wrong_type[field] = invalid;
                cases.push(wrong_type);
            }
        }
        let mut wrong_status = summary();
        wrong_status["todos"][0]["status"] = "blocked".into();
        cases.push(wrong_status);
        let mut unknown_todo_field = summary();
        unknown_todo_field["todos"][0]["unexpected"] = true.into();
        cases.push(unknown_todo_field);
        let mut blank_todo = summary();
        blank_todo["todos"][0]["text"] = " \t\n".into();
        cases.push(blank_todo);
        for value in cases {
            assert!(
                continuation(&value.to_string()).is_err(),
                "accepted {value}"
            );
        }
        let mut empty_todos = summary();
        empty_todos["todos"] = serde_json::json!([]);
        assert!(
            continuation(&empty_todos.to_string())
                .unwrap()
                .todos
                .is_empty()
        );
    }

    #[test]
    fn response_schema_requires_every_section_and_disallows_unknown_fields() {
        let schema = response_schema();
        assert_eq!(schema["additionalProperties"], false);
        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), summary().as_object().unwrap().len());
        for key in summary().as_object().unwrap().keys() {
            assert!(required.contains(&serde_json::Value::String(key.clone())));
            match key.as_str() {
                "objective" | "resumption_point" => {
                    assert_eq!(schema["properties"][key]["type"], "string");
                }
                "todos" => {}
                _ => {
                    assert_eq!(schema["properties"][key]["type"], "array");
                    assert_eq!(schema["properties"][key]["items"]["type"], "string");
                }
            }
        }
        assert_eq!(schema["properties"]["todos"]["type"], "array");
        let todo = &schema["properties"]["todos"]["items"];
        assert_eq!(todo["additionalProperties"], false);
        assert_eq!(todo["required"], serde_json::json!(["text", "status"]));
        assert_eq!(
            todo["properties"]["status"]["enum"],
            serde_json::json!(["pending", "in_progress", "completed"])
        );
        assert!(!schema.to_string().contains("\"$ref\""));
    }

    #[test]
    fn schema_serialization_preserves_evidence_before_todos_and_resumption() {
        let expected = [
            "objective",
            "user_instructions",
            "session_rules",
            "plan",
            "findings",
            "open_issues",
            "running_work",
            "completed_work",
            "decisions",
            "recovery_details",
            "additional_context",
            "todo_reconciliation",
            "todos",
            "resumption_point",
            "next_actions",
        ];
        // Replay and provider adapters deserialize schema Values; order must
        // survive that round trip, not just the original schema generation.
        let schema: serde_json::Value =
            serde_json::from_str(&response_schema().to_string()).unwrap();
        assert_eq!(
            schema["properties"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(schema["required"], serde_json::json!(expected));
        assert_eq!(
            schema["properties"]["todos"]["items"]["properties"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["text", "status"]
        );
    }

    #[test]
    fn directive_preserves_state_and_reconciles_todos_without_a_budget() {
        let message = directive();
        let prompt = text(&message);
        for expected in [
            "continuation prompt",
            "latest instructions",
            "include it verbatim",
            "previous continuation",
            "rejected",
            "exact job outputs",
            "session restrictions and rules",
            "user or loaded skills",
            "final answer as a JSON object",
            "Reasoning may occur separately",
            "complete current todo list",
            "retain the existing status",
            "Reconcile only this agent's list",
        ] {
            assert!(prompt.contains(expected), "missing guidance: {expected}");
        }
        for forbidden in ["30k", "30,000", "30000", "token"] {
            assert!(
                !prompt.contains(forbidden),
                "unexpected budget: {forbidden}"
            );
        }
        let (_, visible_schema) = prompt
            .split_once("Final-answer JSON Schema (applies only to the final answer, not separate reasoning):\n")
            .expect("the model must see the schema even with grammar-only enforcement");
        let visible_schema: serde_json::Value = serde_json::from_str(visible_schema).unwrap();
        assert_eq!(visible_schema, response_schema());
        assert!(
            visible_schema["properties"]["todo_reconciliation"]["description"]
                .as_str()
                .unwrap()
                .contains("conversation evidence")
        );
    }

    #[test]
    fn request_estimate_accounts_for_the_response_schema() {
        let mut request = ModelRequest {
            model: "model".into(),
            system: vec![],
            messages: vec![],
            tools: vec![],
            response_schema: None,
            reasoning: None,
            max_output_tokens: None,
            correlation: None,
        };
        let without_schema = estimate_request(&request);
        request.response_schema = Some(crate::provider::protocol::ResponseSchema {
            name: "compaction".into(),
            schema: response_schema(),
        });
        assert!(estimate_request(&request) > without_schema + 1_000);
    }

    #[test]
    fn image_estimate_ignores_encoded_payload_length() {
        let mut image = crate::media::ImageReference {
            sha256: "hash".into(),
            media_type: "image/png".into(),
            name: "image".into(),
            bytes: 10,
            data_base64: Some("a".repeat(10)),
        };
        let short = estimate_message(&Message::User(vec![UserContent::Image {
            image: image.clone(),
        }]));
        image.data_base64 = Some("a".repeat(100_000));
        let long = estimate_message(&Message::User(vec![UserContent::Image { image }]));
        assert_eq!(short, long);
        assert!(short > 1_000);
    }
}
