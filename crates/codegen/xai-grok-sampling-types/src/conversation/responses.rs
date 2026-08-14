//! Responses API wire format.

use super::*;

/// Flatten `response.output` into `ConversationItem`s, preserving emission
/// order. Replaying that order byte for byte on the next turn is what keeps
/// the server-side prefix cache hot.
pub fn response_to_conversation_items(response: rs::Response) -> Vec<ConversationItem> {
    let model_id = response.model.clone();
    let model_fingerprint = response
        .metadata
        .as_ref()
        .and_then(|m| m.get("system_fingerprint"))
        .cloned()
        .filter(|s| !s.is_empty());
    let reasoning_effort = response
        .reasoning
        .as_ref()
        .and_then(|r| r.effort.clone())
        .map(crate::ReasoningEffort::from_responses_api);

    let mut items: Vec<ConversationItem> = Vec::with_capacity(response.output.len() + 1);
    let mut content = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut backend_tool_count: usize = 0;

    for item in response.output {
        match item {
            rs::OutputItem::Message(msg) => {
                for content_part in msg.content {
                    if let rs::OutputMessageContent::OutputText(text_content) = content_part {
                        if !content.is_empty() {
                            content.push('\n');
                        }
                        content.push_str(&text_content.text);
                    }
                }
            }
            rs::OutputItem::FunctionCall(fc) => {
                // Tied to the assistant turn: a ToolResult must follow each
                // one in conversation order, so they are not siblings.
                tool_calls.push(ToolCall {
                    id: Arc::<str>::from(fc.call_id),
                    name: fc.name,
                    arguments: Arc::<str>::from(fc.arguments),
                });
            }
            rs::OutputItem::Reasoning(r) => {
                items.push(ConversationItem::Reasoning(r));
            }
            // Already run server-side; kept so later turns replay the same
            // context.
            rs::OutputItem::WebSearchCall(ws) => {
                backend_tool_count += 1;
                items.push(ConversationItem::BackendToolCall(BackendToolCallItem {
                    kind: BackendToolKind::WebSearch(ws),
                }));
            }
            rs::OutputItem::CustomToolCall(ct) => {
                backend_tool_count += 1;
                items.push(ConversationItem::BackendToolCall(BackendToolCallItem {
                    kind: BackendToolKind::XSearch(ct),
                }));
            }
            rs::OutputItem::CodeInterpreterCall(ci) => {
                backend_tool_count += 1;
                items.push(ConversationItem::BackendToolCall(BackendToolCallItem {
                    kind: BackendToolKind::CodeInterpreter(ci),
                }));
            }
            rs::OutputItem::McpCall(_) => {
                backend_tool_count += 1;
            }
            _ => {}
        }
    }

    if backend_tool_count > 0 {
        tracing::info!(
            backend_tool_count,
            "response contained backend-executed tool calls"
        );
    }

    tracing::info!(model_id = %model_id, ?model_fingerprint, ?reasoning_effort, "response_to_conversation_items setting model metadata on AssistantItem");
    items.push(ConversationItem::Assistant(AssistantItem {
        content: Arc::<str>::from(content),
        tool_calls,
        model_id: Some(model_id),
        model_fingerprint,
        reasoning_effort,
    }));

    items
}

impl From<&ConversationRequest> for rs::CreateResponse {
    fn from(req: &ConversationRequest) -> Self {
        let input = build_responses_input(req);
        let tools = build_responses_tools(req);

        let tool_choice = req.tool_choice.as_ref().map(|tc| match tc {
            ConversationToolChoice::Auto => rs::ToolChoiceParam::Mode(rs::ToolChoiceOptions::Auto),
            ConversationToolChoice::None => rs::ToolChoiceParam::Mode(rs::ToolChoiceOptions::None),
            ConversationToolChoice::Required => {
                rs::ToolChoiceParam::Mode(rs::ToolChoiceOptions::Required)
            }
            ConversationToolChoice::Function(name) => {
                rs::ToolChoiceParam::Function(rs::ToolChoiceFunction { name: name.clone() })
            }
        });

        let text = req
            .json_schema
            .as_ref()
            .map(|schema| rs::ResponseTextParam {
                format: rs::TextResponseFormatConfiguration::JsonSchema(
                    rs::ResponseFormatJsonSchema {
                        description: None,
                        name: STRUCTURED_OUTPUT_SCHEMA_NAME.to_string(),
                        schema: Some(schema.clone()),
                        strict: Some(true),
                    },
                ),
                verbosity: None,
            });

        rs::CreateResponse {
            background: None,
            conversation: None,
            include: None,
            input,
            instructions: None,
            max_output_tokens: req.max_output_tokens,
            max_tool_calls: None,
            metadata: None,
            model: req.model.clone(),
            parallel_tool_calls: None,
            previous_response_id: None,
            prompt: None,
            prompt_cache_key: req.prompt_cache_key.clone(),
            prompt_cache_retention: None,
            reasoning: Some(rs::Reasoning {
                effort: req.reasoning_effort.map(|e| e.to_responses_api()),
                summary: Some(rs::ReasoningSummary::Concise),
            }),
            safety_identifier: None,
            service_tier: None,
            store: None,
            stream: None,
            stream_options: None,
            temperature: req.temperature,
            text,
            tool_choice,
            tools: if tools.is_empty() { None } else { Some(tools) },
            top_logprobs: None,
            top_p: req.top_p,
            truncation: None,
        }
    }
}

/// Reasoning items stay top-level siblings rather than folding into the
/// assistant, so the input replays the model's original order.
pub(super) fn build_responses_input(req: &ConversationRequest) -> rs::InputParam {
    let items: Vec<rs::InputItem> = req
        .items
        .iter()
        .flat_map(conversation_item_to_input_items)
        .collect();
    rs::InputParam::Items(items)
}

/// Apply per-model Codex catalog capabilities to a serialized Responses body.
///
/// The generic `CreateResponse` conversion cannot see catalog flags; the Codex
/// transport calls this after serialize so a Spark entry that rejects
/// `reasoning.summary` does not inherit Sol's wire shape (#245).
pub fn apply_codex_wire_capabilities(
    body: &mut serde_json::Value,
    caps: &crate::CodexWireCapabilities,
) {
    if !caps.include_reasoning_summary()
        && let Some(reasoning) = body.get_mut("reasoning").and_then(|v| v.as_object_mut())
    {
        reasoning.remove("summary");
    }
    if !caps.allows_image_input()
        && let Some(input) = body.get_mut("input").and_then(|v| v.as_array_mut())
    {
        for item in input.iter_mut() {
            if let Some(content) = item.get_mut("content").and_then(|v| v.as_array_mut()) {
                content.retain(|part| {
                    part.get("type")
                        .and_then(|t| t.as_str())
                        .is_none_or(|t| t != "input_image")
                });
            }
        }
    }
}

/// Stable marker injected once into Codex Ultra `instructions`.
///
/// First-party Codex keeps Ultra as a client mode, lowers the wire effort to
/// `max`, and activates proactive multi-agent developer instructions. Generic
/// Responses / Chat Completions must not receive this marker.
pub const CODEX_ULTRA_PROACTIVE_MARKER: &str = "<codex_ultra_proactive_multi_agent>";

const CODEX_ULTRA_PROACTIVE_INSTRUCTION: &str = concat!(
    "<codex_ultra_proactive_multi_agent> ",
    "Proactively delegate independent subtasks to specialized agents when that ",
    "completes the user's work faster or more reliably. Keep coordination, ",
    "synthesis, and final decisions yourself."
);

/// Codex-only Responses boundary: client `ultra` → wire `max`, and inject the
/// proactive multi-agent instruction marker at most once.
pub fn prepare_codex_ultra_responses(
    body: &mut serde_json::Value,
    client_effort: Option<crate::ReasoningEffort>,
) {
    if client_effort != Some(crate::ReasoningEffort::Ultra) {
        return;
    }

    match body.get_mut("reasoning") {
        Some(serde_json::Value::Object(reasoning)) => {
            reasoning.insert(
                "effort".to_string(),
                serde_json::Value::String("max".to_string()),
            );
        }
        _ => {
            body["reasoning"] = serde_json::json!({ "effort": "max" });
        }
    }

    let existing = body
        .get("instructions")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if existing.contains(CODEX_ULTRA_PROACTIVE_MARKER) {
        return;
    }
    body["instructions"] = serde_json::Value::String(if existing.is_empty() {
        CODEX_ULTRA_PROACTIVE_INSTRUCTION.to_string()
    } else {
        format!("{existing}\n\n{CODEX_ULTRA_PROACTIVE_INSTRUCTION}")
    });
}

/// Codex expects system guidance in the top-level `instructions` field rather
/// than as a `role: "system"` item inside `input`. Keep the generic Responses
/// conversion unchanged and apply this only at the Codex transport boundary.
pub fn patch_codex_instructions(body: &mut serde_json::Value) {
    let Some(input) = body
        .get_mut("input")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };

    let mut system_text = Vec::new();
    input.retain(|item| {
        if item.get("role").and_then(serde_json::Value::as_str) != Some("system") {
            return true;
        }
        if let Some(content) = item.get("content").and_then(codex_instruction_text) {
            system_text.push(content);
            return false;
        }
        // Do not silently discard a valid-but-unsupported content shape.
        // Leaving it in `input` is safer than stripping system guidance.
        true
    });

    if system_text.is_empty() {
        return;
    }
    if let Some(existing) = body
        .get("instructions")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
    {
        system_text.insert(0, existing.to_owned());
    }
    body["instructions"] = serde_json::Value::String(system_text.join("\n\n"));
}

fn codex_instruction_text(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Array(parts) if !parts.is_empty() => {
            let mut text = Vec::with_capacity(parts.len());
            for part in parts {
                let kind = part.get("type").and_then(serde_json::Value::as_str);
                if !matches!(kind, Some("input_text" | "output_text")) {
                    return None;
                }
                text.push(part.get("text")?.as_str()?.to_owned());
            }
            Some(text.join("\n\n"))
        }
        _ => None,
    }
}

/// Inject the `type: "reasoning_text"` discriminator the API requires.
/// `async-openai`'s `ReasoningTextContent` has no `type` field, so it
/// serializes to `{"text": ...}` and the API answers 400. Delete this once
/// upstream grows the field.
pub fn patch_reasoning_text_types(body: &mut serde_json::Value) {
    let Some(input) = body.get_mut("input").and_then(|v| v.as_array_mut()) else {
        return;
    };
    for item in input.iter_mut() {
        if item.get("type").and_then(|t| t.as_str()) != Some("reasoning") {
            continue;
        }
        let Some(content) = item.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        for c in content.iter_mut() {
            if let Some(obj) = c.as_object_mut() {
                obj.entry("type")
                    .or_insert_with(|| serde_json::Value::String("reasoning_text".into()));
            }
        }
    }
}

fn conversation_item_to_input_items(item: &ConversationItem) -> Vec<rs::InputItem> {
    match item {
        ConversationItem::System(s) => {
            vec![rs::InputItem::EasyMessage(rs::EasyInputMessage {
                r#type: rs::MessageType::Message,
                role: rs::Role::System,
                content: rs::EasyInputContent::Text(s.content.as_ref().to_owned()),
            })]
        }
        ConversationItem::User(u) => {
            let content = content_parts_to_easy_input_content(&u.content);
            vec![rs::InputItem::EasyMessage(rs::EasyInputMessage {
                r#type: rs::MessageType::Message,
                role: rs::Role::User,
                content,
            })]
        }
        ConversationItem::Reasoning(r) => {
            // `status` is output-only and rejected on input.
            let mut r = r.clone();
            r.status = None;
            vec![rs::InputItem::Item(rs::Item::Reasoning(r))]
        }
        ConversationItem::Assistant(a) => {
            let mut items = Vec::new();

            if !a.content.is_empty() {
                items.push(rs::InputItem::EasyMessage(rs::EasyInputMessage {
                    r#type: rs::MessageType::Message,
                    role: rs::Role::Assistant,
                    content: rs::EasyInputContent::Text(a.content.as_ref().to_owned()),
                }));
            }

            for tc in &a.tool_calls {
                let arguments = sanitize_tool_arguments(&tc.id, &tc.name, tc.arguments.clone());
                items.push(rs::InputItem::Item(rs::Item::FunctionCall(
                    rs::FunctionToolCall {
                        call_id: tc.id.as_ref().to_owned(),
                        name: tc.name.clone(),
                        arguments: arguments.as_ref().to_owned(),
                        id: None,
                        status: None,
                    },
                )));
            }

            items
        }
        ConversationItem::ToolResult(t) => {
            let output = if t.images.is_empty() {
                rs::FunctionCallOutput::Text(t.content.as_ref().to_owned())
            } else {
                let mut parts: Vec<rs::InputContent> =
                    vec![rs::InputContent::InputText(rs::InputTextContent {
                        text: t.content.as_ref().to_owned(),
                    })];
                for img in &t.images {
                    if let ContentPart::Image { url } = img {
                        parts.push(rs::InputContent::InputImage(rs::InputImageContent {
                            detail: rs::ImageDetail::Auto,
                            file_id: None,
                            image_url: Some(url.as_ref().to_owned()),
                        }));
                    }
                }
                rs::FunctionCallOutput::Content(parts)
            };
            vec![rs::InputItem::Item(rs::Item::FunctionCallOutput(
                rs::FunctionCallOutputItemParam {
                    call_id: t.tool_call_id.clone(),
                    output,
                    id: None,
                    status: None,
                },
            ))]
        }
        ConversationItem::BackendToolCall(b) => {
            vec![match &b.kind {
                BackendToolKind::WebSearch(ws) => {
                    rs::InputItem::Item(rs::Item::WebSearchCall(ws.clone()))
                }
                BackendToolKind::XSearch(ct) => {
                    rs::InputItem::Item(rs::Item::CustomToolCall(ct.clone()))
                }
                BackendToolKind::CodeInterpreter(ci) => {
                    rs::InputItem::Item(rs::Item::CodeInterpreterCall(ci.clone()))
                }
            }]
        }
    }
}

fn content_parts_to_easy_input_content(parts: &[ContentPart]) -> rs::EasyInputContent {
    if parts.len() == 1
        && let ContentPart::Text { text } = &parts[0]
    {
        return rs::EasyInputContent::Text(text.as_ref().to_owned());
    }

    let items: Vec<rs::InputContent> = parts
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => rs::InputContent::InputText(rs::InputTextContent {
                text: text.as_ref().to_owned(),
            }),
            ContentPart::Image { url } => rs::InputContent::InputImage(rs::InputImageContent {
                image_url: Some(url.as_ref().to_owned()),
                file_id: None,
                detail: rs::ImageDetail::default(),
            }),
        })
        .collect();

    rs::EasyInputContent::ContentList(items)
}

/// Client function tools plus backend-hosted tools. On a name collision the
/// hosted tool wins, because sending both is rejected as a duplicate.
fn build_responses_tools(req: &ConversationRequest) -> Vec<rs::Tool> {
    let mut tools: Vec<rs::Tool> = req
        .tools
        .iter()
        .filter(|t| {
            let collides = req.hosted_tools.iter().any(|h| h.wire_name() == t.name);
            if collides {
                tracing::warn!(
                    tool = %t.name,
                    "dropping function tool that collides with a backend-hosted tool"
                );
            }
            !collides
        })
        .map(|t| {
            rs::Tool::Function(rs::FunctionTool {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: Some(t.parameters.clone()),
                strict: None,
            })
        })
        .collect();

    for hosted in &req.hosted_tools {
        match hosted {
            HostedTool::WebSearch { options } => {
                // An empty allowlist is unbounded, so it emits no filter.
                let filters = options
                    .as_ref()
                    .and_then(|o| o.allowed_domains.as_deref())
                    .filter(|domains| !domains.is_empty())
                    .map(|domains| rs::WebSearchToolFilters {
                        allowed_domains: Some(domains.to_vec()),
                    });
                tools.push(rs::Tool::WebSearch(rs::WebSearchTool {
                    filters,
                    ..Default::default()
                }));
            }
            // XSearch is xAI-specific — not in async_openai's rs::Tool enum.
            // Injected as raw JSON by the sampler client after serialization.
            HostedTool::XSearch { .. } => {}
        }
    }

    tools
}

/// Hosted tools with no `rs::Tool` variant. The sampler client splices these
/// into the serialized `tools` array.
pub fn extra_tool_entries(hosted_tools: &[HostedTool]) -> Vec<serde_json::Value> {
    let mut entries = Vec::new();
    for tool in hosted_tools {
        match tool {
            // WebSearch ships natively (rs::Tool::WebSearch), so no JSON entry here.
            HostedTool::WebSearch { .. } => {}
            HostedTool::XSearch { options } => {
                entries.push(match options {
                    Some(o) => o.to_tool_entry(),
                    None => XSearchOptions::default().to_tool_entry(),
                });
            }
        }
    }
    entries
}
