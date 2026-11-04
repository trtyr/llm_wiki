//! Agent loop tool dispatch: input building, execution and success
//! recording for a single Agent loop tool call.

use super::*;

impl AgentRuntime {
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub(super) async fn execute_agent_loop_tool(
        &self,
        request: &AgentChatRequest,
        action: &AgentLoopAction,
        permission_policy: &PermissionPolicy,
        tool_registry: &tools::BuiltinToolRegistry,
        skills: &[AgentSkill],
        references: &mut Vec<AgentReference>,
        tool_events: &mut Vec<AgentToolEvent>,
        events: &mut Vec<AgentEvent>,
        event_sink: &Option<AgentEventSink>,
        cancellation: Option<&AgentCancellationToken>,
    ) -> Result<AgentObservation, String> {
        let tool = match action
            .tool
            .as_deref()
            .map(str::trim)
            .filter(|tool| !tool.is_empty())
        {
            Some(tool) => tool,
            None => {
                return Ok(AgentObservation {
                    tool: "agent.action".to_string(),
                    summary: "invalid tool action: missing tool name".to_string(),
                });
            }
        };

        let input = match self.agent_loop_tool_input(request, tool, action) {
            Ok(input) => input,
            Err(err) => {
                return Ok(record_loop_tool_rejection(
                    tool,
                    err,
                    tool_events,
                    events,
                    event_sink,
                ));
            }
        };
        let input_detail = summarize_tool_input(tool, &input);

        if tool == "shell.exec" {
            if skills.is_empty() {
                return Ok(record_loop_tool_rejection(
                    tool,
                    "shell.exec is only available when at least one skill is active for this turn"
                        .to_string(),
                    tool_events,
                    events,
                    event_sink,
                ));
            }
            let command = input
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim();
            if is_skill_preference_probe_command(command) {
                let summary = skipped_skill_preference_probe_summary(command);
                tool_emit_event(
                    tool_events,
                    events,
                    event_sink,
                    AgentToolEvent {
                        tool: "shell.exec".to_string(),
                        status: "completed".to_string(),
                        detail: Some("skipped optional skill preference probe".to_string()),
                    },
                );
                emit_event(
                    events,
                    event_sink,
                    AgentEvent::tool_start("shell.exec", Some(command.to_string())),
                );
                emit_event(
                    events,
                    event_sink,
                    AgentEvent::tool_end("shell.exec", Some(summary.clone())),
                );
                return Ok(AgentObservation {
                    tool: "shell.exec.preference_probe_skipped".to_string(),
                    summary,
                });
            }
            if let Err(err) = permission_policy.require(AgentCapability::Process) {
                return Ok(record_loop_tool_rejection(
                    tool,
                    err,
                    tool_events,
                    events,
                    event_sink,
                ));
            }
            if !is_shell_command_allowed_without_prompt(
                command,
                &request.approved_shell_commands,
                &self.project_path,
            ) {
                let detail = format!("approval required: {command}");
                emit_event(
                    events,
                    event_sink,
                    AgentEvent::tool_start("shell.exec", Some(command.to_string())),
                );
                tool_emit_event(
                    tool_events,
                    events,
                    event_sink,
                    AgentToolEvent {
                        tool: "shell.exec".to_string(),
                        status: "available".to_string(),
                        detail: Some(detail.clone()),
                    },
                );
                emit_event(
                    events,
                    event_sink,
                    AgentEvent::tool_end("shell.exec", Some(detail.clone())),
                );
                return Ok(AgentObservation {
                    tool: SHELL_APPROVAL_REQUIRED_OBSERVATION.to_string(),
                    summary: format!(
                        "The Agent needs approval before it can run this command:\n\n`{command}`\n\nApprove the command if you want the Agent to continue with this skill."
                    ),
                });
            }
        }

        if tool == "skill.read_file" {
            if let Err(err) = permission_policy.require(AgentCapability::ReadProject) {
                return Ok(record_loop_tool_rejection(
                    tool,
                    err,
                    tool_events,
                    events,
                    event_sink,
                ));
            }
            tool_emit_event(
                tool_events,
                events,
                event_sink,
                AgentToolEvent {
                    tool: tool.to_string(),
                    status: "started".to_string(),
                    detail: input_detail.clone(),
                },
            );
            emit_event(
                events,
                event_sink,
                AgentEvent::tool_start(tool, input_detail),
            );
            return match read_active_skill_file(skills, &input) {
                Ok(value) => {
                    let summary =
                        self.record_loop_tool_success(tool, value, references, events, event_sink)?;
                    tool_emit_event(
                        tool_events,
                        events,
                        event_sink,
                        AgentToolEvent {
                            tool: tool.to_string(),
                            status: "completed".to_string(),
                            detail: Some(summary.clone()),
                        },
                    );
                    emit_event(
                        events,
                        event_sink,
                        AgentEvent::tool_end(tool, Some(summary.clone())),
                    );
                    Ok(AgentObservation {
                        tool: tool.to_string(),
                        summary,
                    })
                }
                Err(err) => {
                    tool_emit_event(
                        tool_events,
                        events,
                        event_sink,
                        AgentToolEvent {
                            tool: tool.to_string(),
                            status: "failed".to_string(),
                            detail: Some(err.clone()),
                        },
                    );
                    emit_event(
                        events,
                        event_sink,
                        AgentEvent::tool_end(tool, Some(format!("failed: {err}"))),
                    );
                    Ok(AgentObservation {
                        tool: tool.to_string(),
                        summary: format!("failed: {err}"),
                    })
                }
            };
        }

        if let Err(err) = require_tool_permission(tool, request, permission_policy) {
            return Ok(record_loop_tool_rejection(
                tool,
                err,
                tool_events,
                events,
                event_sink,
            ));
        }
        tool_emit_event(
            tool_events,
            events,
            event_sink,
            AgentToolEvent {
                tool: tool.to_string(),
                status: "started".to_string(),
                detail: input_detail.clone(),
            },
        );
        emit_event(
            events,
            event_sink,
            AgentEvent::tool_start(tool, input_detail),
        );

        let result = execute_tool_with_cancellation(
            tool_registry.execute(tool, input, self.tool_context()),
            cancellation,
        )
        .await;

        match result {
            Ok(value) => {
                let summary =
                    self.record_loop_tool_success(tool, value, references, events, event_sink)?;
                tool_emit_event(
                    tool_events,
                    events,
                    event_sink,
                    AgentToolEvent {
                        tool: tool.to_string(),
                        status: "completed".to_string(),
                        detail: Some(summary.clone()),
                    },
                );
                emit_event(
                    events,
                    event_sink,
                    AgentEvent::tool_end(tool, Some(summary.clone())),
                );
                Ok(AgentObservation {
                    tool: tool.to_string(),
                    summary,
                })
            }
            Err(err) => {
                tool_emit_event(
                    tool_events,
                    events,
                    event_sink,
                    AgentToolEvent {
                        tool: tool.to_string(),
                        status: "failed".to_string(),
                        detail: Some(err.clone()),
                    },
                );
                emit_event(
                    events,
                    event_sink,
                    AgentEvent::tool_end(tool, Some(format!("failed: {err}"))),
                );
                Ok(AgentObservation {
                    tool: tool.to_string(),
                    summary: format!("failed: {err}"),
                })
            }
        }
    }

    pub(super) fn agent_loop_tool_input(
        &self,
        request: &AgentChatRequest,
        tool: &str,
        action: &AgentLoopAction,
    ) -> Result<Value, String> {
        let top_k = action
            .top_k
            .or(request.top_k)
            .unwrap_or(DEFAULT_CHAT_SEARCH_RESULTS)
            .clamp(1, MAX_CHAT_SEARCH_RESULTS);
        match tool {
            "wiki.search" | "source.search" | "graph.search" | "web.search" | "anytxt.search" => {
                let query = action
                    .query
                    .as_deref()
                    .map(str::trim)
                    .filter(|query| !query.is_empty())
                    .ok_or_else(|| format!("{tool} requires query"))?;
                Ok(serde_json::json!({
                    "query": query,
                    "topK": top_k,
                    "includeContent": action.include_content.or(request.include_content).unwrap_or(false),
                }))
            }
            "wiki.read_page" => {
                let path = action
                    .path
                    .as_deref()
                    .map(str::trim)
                    .filter(|path| !path.is_empty())
                    .ok_or_else(|| "wiki.read_page requires path".to_string())?;
                Ok(serde_json::json!({ "path": path }))
            }
            "web.fetch" => {
                // The model carries the URL in `query` (like a search term)
                // or `path` (like a page location); accept both.
                let url = action
                    .url
                    .as_deref()
                    .or(action.query.as_deref())
                    .or(action.path.as_deref())
                    .map(str::trim)
                    .filter(|url| !url.is_empty())
                    .ok_or_else(|| "web.fetch requires url".to_string())?;
                Ok(serde_json::json!({ "url": url }))
            }
            "skill.read_file" => {
                let path = action
                    .path
                    .as_deref()
                    .map(str::trim)
                    .filter(|path| !path.is_empty())
                    .ok_or_else(|| "skill.read_file requires path".to_string())?;
                Ok(serde_json::json!({
                    "skill": action.skill.as_deref().map(str::trim).filter(|skill| !skill.is_empty()),
                    "path": path,
                }))
            }
            "wiki.write_page" => {
                let path = action
                    .path
                    .as_deref()
                    .map(str::trim)
                    .filter(|path| !path.is_empty())
                    .ok_or_else(|| "wiki.write_page requires path".to_string())?;
                let content = action
                    .content
                    .as_deref()
                    .map(str::trim)
                    .filter(|content| !content.is_empty())
                    .ok_or_else(|| "wiki.write_page requires content".to_string())?;
                Ok(serde_json::json!({
                    "path": path,
                    "content": content,
                    "allowOverwrite": action.allow_overwrite.unwrap_or(false),
                }))
            }
            "workspace.write_file" | "workspace.append_file" => {
                let tool_name = action.tool.as_deref().unwrap_or("workspace.write_file");
                let path = action
                    .path
                    .as_deref()
                    .map(str::trim)
                    .filter(|path| !path.is_empty())
                    .ok_or_else(|| format!("{tool_name} requires path"))?;
                let content = action
                    .content
                    .as_deref()
                    .ok_or_else(|| format!("{tool_name} requires content"))?;
                Ok(serde_json::json!({
                    "path": path,
                    "content": content,
                }))
            }
            "shell.exec" => {
                let command = action
                    .command
                    .as_deref()
                    .or(action.query.as_deref())
                    .map(str::trim)
                    .filter(|command| !command.is_empty())
                    .ok_or_else(|| "shell.exec requires command".to_string())?;
                Ok(serde_json::json!({
                    "command": command,
                    "timeoutSeconds": action.timeout_seconds,
                }))
            }
            other => Err(format!("Unknown Agent tool: {other}")),
        }
    }

    pub(crate) fn record_loop_tool_success(
        &self,
        tool: &str,
        value: Value,
        references: &mut Vec<AgentReference>,
        events: &mut Vec<AgentEvent>,
        event_sink: &Option<AgentEventSink>,
    ) -> Result<String, String> {
        match tool {
            "wiki.search" => {
                let search: tools::WikiSearchToolOutput = serde_json::from_value(value)
                    .map_err(|err| format!("Invalid wiki.search result: {err}"))?;
                let count = search.references.len();
                let added = search
                    .references
                    .into_iter()
                    .filter(|reference| {
                        push_unique_reference(references, events, event_sink, reference.clone())
                    })
                    .count();
                Ok(format!(
                    "{count} result(s), {added} new, mode={}, tokenHits={}, vectorHits={}, graphHits={}",
                    search.mode, search.token_hits, search.vector_hits, search.graph_hits
                ))
            }
            "web.fetch" => {
                let bytes = value.get("bytes").and_then(Value::as_u64).unwrap_or(0);
                let truncated = value
                    .get("truncated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let final_url = value
                    .get("finalUrl")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown url");
                Ok(format!(
                    "fetched {bytes} bytes from {final_url}{}",
                    if truncated { " (truncated)" } else { "" }
                ))
            }
            "source.search" | "graph.search" | "web.search" | "anytxt.search" => {
                let found: Vec<AgentReference> = serde_json::from_value(value)
                    .map_err(|err| format!("Invalid {tool} result: {err}"))?;
                let count = found.len();
                let added = found
                    .into_iter()
                    .filter(|reference| {
                        push_unique_reference(references, events, event_sink, reference.clone())
                    })
                    .count();
                Ok(format!("{count} result(s), {added} new"))
            }
            "wiki.read_page" => {
                let path = value
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or("wiki page");
                let content = value.get("content").and_then(Value::as_str).unwrap_or("");
                let mut summary = format!(
                    "read {path}\n{}",
                    trim_chars(&collapse_whitespace(content), 4_000)
                );
                if let Some(context) = value
                    .get("knowledgeContext")
                    .filter(|value| !value.is_null())
                {
                    summary.push_str("\nKnowledge context: ");
                    summary.push_str(&trim_chars(&context.to_string(), 2_000));
                }
                Ok(summary)
            }
            "skill.read_file" => {
                let skill = value
                    .get("skill")
                    .and_then(Value::as_str)
                    .unwrap_or("skill");
                let path = value.get("path").and_then(Value::as_str).unwrap_or("file");
                let content = value.get("content").and_then(Value::as_str).unwrap_or("");
                Ok(format!(
                    "read {skill}:{path}\n{}",
                    trim_chars(&collapse_whitespace(content), 4_000)
                ))
            }
            "wiki.write_page" => {
                let output: tools::WikiWriteOutput = serde_json::from_value(value)
                    .map_err(|err| format!("Invalid wiki.write_page result: {err}"))?;
                emit_event(
                    events,
                    event_sink,
                    AgentEvent::FileChanged {
                        path: output.reference.path.clone(),
                        tool: "wiki.write_page".to_string(),
                        existed_before: output.existed_before,
                        previous_content: output.previous_content,
                    },
                );
                let reference = output.reference;
                let path = reference.path.clone();
                push_unique_reference(references, events, event_sink, reference);
                Ok(format!("wrote {path}"))
            }
            "workspace.write_file" | "workspace.append_file" => {
                let output: tools::WorkspaceWriteOutput = serde_json::from_value(value)
                    .map_err(|err| format!("Invalid {tool} result: {err}"))?;
                let path = output.path.as_str();
                let bytes = output.bytes as u64;
                let action = if tool == "workspace.append_file" {
                    "updated"
                } else {
                    "written"
                };
                emit_event(
                    events,
                    event_sink,
                    AgentEvent::FileChanged {
                        path: output.path.clone(),
                        tool: tool.to_string(),
                        existed_before: output.existed_before,
                        previous_content: output.previous_content,
                    },
                );
                let reference = AgentReference {
                    title: Path::new(path)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or(path)
                        .to_string(),
                    path: path.to_string(),
                    kind: "workspace".to_string(),
                    snippet: Some(format!("Generated file {action} by Agent ({bytes} bytes).")),
                    score: None,
                    knowledge_context: None,
                };
                push_unique_reference(references, events, event_sink, reference);
                Ok(format!(
                    "{} {path} ({bytes} bytes)",
                    if tool == "workspace.append_file" {
                        "appended"
                    } else {
                        "wrote"
                    }
                ))
            }
            "shell.exec" => {
                let output: tools::ShellExecToolOutput = serde_json::from_value(value)
                    .map_err(|err| format!("Invalid shell.exec result: {err}"))?;
                for generated in &output.generated_files {
                    let reference = AgentReference {
                        title: Path::new(&generated.path)
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or(&generated.path)
                            .to_string(),
                        path: generated.path.clone(),
                        kind: "workspace".to_string(),
                        snippet: Some(format!(
                            "Generated file written by shell.exec ({} bytes).",
                            generated.bytes
                        )),
                        score: None,
                        knowledge_context: None,
                    };
                    push_unique_reference(references, events, event_sink, reference);
                }
                let generated_summary = if output.generated_files.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\nGenerated files:\n{}",
                        output
                            .generated_files
                            .iter()
                            .map(|file| format!("- {}", file.path))
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                };
                Ok(format!(
                    "`{}` exit={:?} timedOut={}\nstdout:\n{}\nstderr:\n{}",
                    output.command,
                    output.exit_code,
                    output.timed_out,
                    trim_chars(&output.stdout, 8_000),
                    trim_chars(&output.stderr, 4_000)
                ) + &generated_summary)
            }
            _ => Ok(value.to_string()),
        }
    }
}

pub(crate) fn emit_event(
    events: &mut Vec<AgentEvent>,
    event_sink: &Option<AgentEventSink>,
    event: AgentEvent,
) {
    if let Some(sink) = event_sink {
        sink(event.clone());
    }
    events.push(event);
}

pub(crate) fn tool_emit_event(
    tool_events: &mut Vec<AgentToolEvent>,
    events: &mut Vec<AgentEvent>,
    event_sink: &Option<AgentEventSink>,
    tool_event: AgentToolEvent,
) {
    // Mirror tool progress into the AgentEvent stream so SSE clients see
    // the started/completed/failed lifecycle (the blocking response carries
    // the full toolEvents list; this is the streaming counterpart).
    match tool_event.status.as_str() {
        "started" => emit_event(
            events,
            event_sink,
            AgentEvent::ToolStart {
                tool: tool_event.tool.clone(),
                input: tool_event.detail.clone(),
            },
        ),
        "completed" | "failed" => emit_event(
            events,
            event_sink,
            AgentEvent::ToolEnd {
                tool: tool_event.tool.clone(),
                output: tool_event.detail.clone(),
            },
        ),
        _ => {} // informational statuses (e.g. "available") are not lifecycle events
    }
    tool_events.push(tool_event);
}
