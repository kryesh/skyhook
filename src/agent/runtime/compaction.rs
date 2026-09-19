//! Structured continuation guidance and provider-neutral context estimates.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    agent::todo::TodoItem,
    provider::protocol::{BlockContent, Message, ModelRequest, UserContent},
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
    /// Jobs whose original parameters and outputs should be handed over. Select relevant completed or running jobs from this session; use an empty array when none are needed. The harness supplies normally truncated outputs, recoverable in full with job_output.
    jobs: Vec<crate::identity::JobId>,
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

pub(crate) const SCHEMA_VERSION: u16 = 2;

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
    pub jobs: Vec<crate::identity::JobId>,
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
Include completed work, important findings, partial work, blockers, and available verification results. Support completion claims with observed results and their scope. Distinguish an attempted action from a successful result, listing paths from reading their contents, and partial or truncated output from a complete review. If the agent claims completion without confirming evidence, preserve it as the agent's claim with that uncertainty, not as a verified fact. Preserve concrete findings, errors, and source references instead of replacing them with broad evaluations. This records the existing evidence and uncertainty; it does not ask you to redo the investigation or correct its conclusions. Preserve important exact job outputs or excerpts when useful, along with job IDs, file paths, targets, commands, errors, and other references needed to recover details. Explain what relevant running jobs are doing and what results are still awaited. Select useful job IDs in jobs; the harness will include their original parameters and normally truncated outputs. Avoid copying those outputs into narrative sections unless an exact excerpt is needed to explain a finding. Full job results remain retrievable with job_output. Older conversation details must be preserved in this continuation.\n\n\
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
        jobs: summary.jobs,
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
                UserContent::Attachment { attachment } => match attachment {
                    crate::media::AttachmentRef::Image(_) => 2_048,
                    crate::media::AttachmentRef::Text(text) => 4 + text.blob.bytes.div_ceil(4),
                },
            })
            .sum::<u64>(),
        Message::Assistant(items) => items
            .iter()
            .map(|item| {
                item.blocks
                    .iter()
                    .map(|block| match &block.content {
                        BlockContent::Text { text } | BlockContent::Reasoning { text } => {
                            4 + estimate_text(text)
                        }
                        BlockContent::ToolCall(call) => {
                            12 + estimate_text(call.id())
                                + estimate_text(call.name())
                                + estimate_text(&serde_json::Value::Object(call.arguments().clone()).to_string())
                        }
                    })
                    .sum::<u64>()
                    // Opaque replay belongs to the item, not each visible block.
                    + item.replay.as_ref().map_or(0, |replay| {
                        estimate_text(&replay.payload.to_string())
                    })
            })
            .sum::<u64>(),
        Message::Tool(results) => results
            .iter()
            .map(|result| {
                12 + estimate_text(&result.call_id)
                    + estimate_text(&result.name)
                    + estimate_text(&result.result.to_string())
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
        + request.messages().map(estimate_message).sum::<u64>()
        + request.response_schema.as_ref().map_or(0, |response| {
            8 + estimate_text(&response.name) + estimate_text(&response.schema.to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        json!({
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
            "jobs": [],
            "additional_context": [],
            "todo_reconciliation": ["First step completed before the list was updated."],
            "todos": [
                {"text": "First step", "status": "completed"},
                {"text": "Second step", "status": "in_progress"},
                {"text": "Final step", "status": "pending"}
            ]
        })
    }

    fn with(pointer: &str, value: serde_json::Value) -> serde_json::Value {
        let mut summary = summary();
        *summary.pointer_mut(pointer).unwrap() = value;
        summary
    }

    #[test]
    fn generated_schema_accepts_continuations_and_rejects_invalid_contract_data() {
        // Validate after the providers' JSON round trip. Declaration order is a
        // generation hint: reordered valid responses must still validate and parse.
        let schema = serde_json::from_str(&response_schema().to_string()).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let valid = summary();
        let reordered = valid.as_object().unwrap().iter().rev();
        let reordered = reordered.map(|(key, value)| (key.clone(), value.clone()));
        let reordered = serde_json::Value::Object(reordered.collect());
        assert!(validator.is_valid(&reordered));
        let parse = |value: &serde_json::Value| continuation(&value.to_string()).unwrap().message;
        assert_eq!(parse(&reordered), parse(&valid));
        // The derive supplies the rest; one case per kind proves it is wired up.
        let (mut missing, mut unknown) = (valid.clone(), valid.clone());
        missing.as_object_mut().unwrap().remove("plan");
        unknown["todos"][0]["unexpected"] = json!(true);
        let mistyped = with("/todos/0/status", json!("finished"));
        for invalid in [missing, unknown, mistyped] {
            assert!(!validator.is_valid(&invalid), "accepted {invalid}");
        }
    }

    #[test]
    fn continuation_preserves_section_text_and_reconciled_todos() {
        let value = summary();
        let continuation = continuation(&value.to_string()).unwrap();
        let rendered = text(&continuation.message);
        let plan = |index: usize| value["plan"][index].as_str().unwrap();
        let expected = format!(
            "## Plan\n\n{}\n\n{}\n\n## Resumption point",
            plan(0),
            plan(1)
        );
        assert!(rendered.contains(&expected));
        for (key, value) in value.as_object().unwrap() {
            if key == "todos" {
                continue;
            }
            if let Some(entries) = value.as_array() {
                let entries = entries.iter().map(|entry| entry.as_str().unwrap());
                assert!(rendered.contains(&entries.collect::<Vec<_>>().join("\n\n")));
            } else {
                assert!(rendered.contains(value.as_str().unwrap()));
            }
        }
        let found = serde_json::to_value(continuation.todos).unwrap();
        assert_eq!(found, value["todos"]);
    }

    #[test]
    fn continuation_rejects_invalid_structure_and_todos() {
        for output in ["", " ", "prose", "```json\n{}\n```", "{}", "[]", "null"] {
            assert!(continuation(output).is_err(), "accepted {output:?}");
        }
        let mut unknown = summary();
        unknown["unexpected"] = true.into();
        // Beyond serde's structural checks: todo text is not blank, job ids are real.
        let blank_todo = with("/todos/0/text", " \t\n".into());
        for value in [unknown, blank_todo, with("/jobs", json!([0]))] {
            let parsed = continuation(&value.to_string());
            assert!(parsed.is_err(), "accepted {value}");
        }
        let empty_todos = with("/todos", json!([]));
        assert!(
            continuation(&empty_todos.to_string())
                .unwrap()
                .todos
                .is_empty()
        );
    }

    #[test]
    fn estimate_counts_all_visible_blocks_and_opaque_replay_once_per_item() {
        use crate::provider::protocol::{
            AssistantBlock, AssistantItem, ItemKind, ReplayEnvelope, ToolCall,
        };
        let payload = json!({"encrypted_content": "opaque".repeat(100)});
        // The final reasoning item has replay but no visible blocks.
        let blocks = [3, 3, 2, 0];
        let mut items: Vec<_> = (0..)
            .zip(blocks)
            .map(|(position, count)| AssistantItem {
                id: format!("item-{position}"),
                position,
                kind: ItemKind::Reasoning,
                blocks: (0..count)
                    .map(|part| AssistantBlock {
                        id: format!("block-{position}-{part}"),
                        position: part,
                        content: BlockContent::Reasoning {
                            text: "visible summary".into(),
                        },
                    })
                    .collect(),
                replay: Some(ReplayEnvelope {
                    version: 1,
                    protocol: "responses".into(),
                    model: "model".into(),
                    scope: "reasoning".into(),
                    payload: payload.clone(),
                    conversation_bound: false,
                }),
            })
            .collect();
        let reasoning_cost =
            8 * (4 + estimate_text("visible summary")) + 4 * estimate_text(&payload.to_string());
        let found = estimate_message(&Message::Assistant(items.clone()));
        assert_eq!(found, 8 + reasoning_cost);
        items.push(AssistantItem::text("answer", 4, "visible answer"));
        let call = ToolCall::new("call", "read", json!({"path":"file"})).unwrap();
        let arguments = serde_json::Value::Object(call.arguments().clone()).to_string();
        let call_cost =
            12 + estimate_text(call.id()) + estimate_text(call.name()) + estimate_text(&arguments);
        items.push(AssistantItem::tool_call("tool", 5, call));
        assert_eq!(
            estimate_message(&Message::Assistant(items)),
            8 + reasoning_cost + 4 + estimate_text("visible answer") + call_cost
        );
    }

    #[test]
    fn request_estimate_accounts_for_the_response_schema() {
        let mut request = ModelRequest {
            model: "model".into(),
            system: vec![],
            history: Vec::new(),
            tail: Vec::new(),
            history_lifetime: Default::default(),
            tools: vec![],
            response_schema: None,
            reasoning: None,
            max_output_tokens: None,
            correlation: None,
            blobs: Default::default(),
        };
        let without_schema = estimate_request(&request);
        request.response_schema = Some(crate::provider::protocol::ResponseSchema {
            name: "compaction".into(),
            schema: response_schema(),
        });
        assert!(estimate_request(&request) > without_schema + 1_000);
    }

    #[test]
    fn attachment_estimates_fix_image_cost_and_scale_text_with_length() {
        use crate::media::{AttachmentRef, BlobRef, ImageFormat, ImageRef, TextRef};
        let estimate = |attachment| {
            estimate_message(&Message::User(vec![UserContent::Attachment { attachment }]))
        };
        let blob = |bytes| BlobRef {
            bytes,
            ..BlobRef::of(b"")
        };
        let image = |bytes| {
            let format = ImageFormat::Png;
            estimate(AttachmentRef::Image(ImageRef {
                file: None,
                format,
                blob: blob(bytes),
            }))
        };
        let text = |bytes| {
            estimate(AttachmentRef::Text(TextRef {
                file: None,
                blob: blob(bytes),
            }))
        };
        assert_eq!(image(10), image(100_000));
        assert!(image(10) > 1_000);
        assert_eq!(text(4_000) - text(0), 1_000);
        assert_eq!(text(4_001) - text(0), 1_001);
    }
}
