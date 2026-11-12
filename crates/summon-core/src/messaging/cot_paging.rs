//! Bounded native CoT carriers for a continuous logical event stream.
use super::CotEvent;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;

/// Leaves headroom below the observed 1,000-event display boundary.
pub const COT_CARRIER_EVENTS: usize = 900;
pub const COT_REQUEST_EVENTS: usize = 50;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CotWireState {
    events: usize,
    run: Option<CotEvent>,
    reasoning: Option<CotEvent>,
    #[serde(default)]
    reasoning_stage: Option<CotEvent>,
    text: Option<CotEvent>,
    tools: BTreeMap<String, CotEvent>,
    page_tools: BTreeMap<String, bool>,
    timestamp: u64,
    page: u64,
}

impl CotWireState {
    pub fn events(&self) -> usize {
        self.events
    }

    pub fn unknown(run: CotEvent, reasoning: Option<CotEvent>) -> Self {
        let reasoning_stage = reasoning.as_ref().map(|event| {
            let content: serde_json::Value =
                serde_json::from_str(&event.content).expect("generated CoT content");
            CotEvent::new(
                "REASONING_START",
                json!({"messageId":content["messageId"]}),
                &event.timestamp,
            )
        });
        Self {
            events: COT_CARRIER_EVENTS,
            run: Some(run),
            reasoning,
            reasoning_stage,
            ..Self::default()
        }
    }

    /// A new carrier repeats only structural headers, never prior text or arguments.
    pub fn next_page(&self, timestamp: &str) -> (Vec<CotEvent>, Self) {
        let mut next = self.clone();
        next.events = 0;
        next.page_tools.clear();
        next.page += 1;
        let mut events = Vec::new();
        if let Some(run) = &self.run {
            events.push(run.clone());
        }
        let id = format!("cot-continuation-{}", next.page);
        events.push(CotEvent::new(
            "TEXT_MESSAGE_START",
            json!({"messageId":id,"role":"assistant"}),
            timestamp,
        ));
        events.push(CotEvent::new(
            "TEXT_MESSAGE_CONTENT",
            json!({"messageId":id,"delta":summon_i18n::text!("cot.continuation")}),
            timestamp,
        ));
        events.push(CotEvent::new(
            "TEXT_MESSAGE_END",
            json!({"messageId":id}),
            timestamp,
        ));
        if let Some(stage) = &self.reasoning_stage {
            events.push(stage.clone());
        }
        if let Some(reasoning) = &self.reasoning {
            events.push(reasoning.clone());
        }
        if let Some(text) = &self.text {
            events.push(text.clone());
        }
        next.stamp(timestamp, &mut events);
        next.events = events.len();
        (events, next)
    }

    /// Prepare one bounded request, restoring tool headers when a result crosses carriers.
    pub fn batch(&self, input: &[CotEvent]) -> (Vec<CotEvent>, usize, Self) {
        let mut next = self.clone();
        let mut batch = Vec::new();
        let mut consumed = 0;
        for event in input {
            let mut candidate = next.clone();
            let mut expanded = candidate.expand(event);
            if batch.len() + expanded.len() > COT_REQUEST_EVENTS
                || candidate.events + expanded.len() > COT_CARRIER_EVENTS
            {
                break;
            }
            candidate.stamp(&event.timestamp, &mut expanded);
            candidate.events += expanded.len();
            batch.extend(expanded);
            next = candidate;
            consumed += 1;
        }
        (batch, consumed, next)
    }

    fn stamp(&mut self, timestamp: &str, events: &mut [CotEvent]) {
        let base = timestamp.parse().unwrap_or(0);
        for event in events {
            self.timestamp = base.max(self.timestamp.saturating_add(1));
            event.timestamp = self.timestamp.to_string();
        }
    }

    fn expand(&mut self, event: &CotEvent) -> Vec<CotEvent> {
        let content: serde_json::Value =
            serde_json::from_str(&event.content).expect("generated CoT content");
        let mut output = Vec::new();
        let id = content["toolCallId"].as_str().unwrap_or_default();
        match event.event_type.as_str() {
            "RUN_STARTED" => self.run = Some(event.clone()),
            "REASONING_START" => self.reasoning_stage = Some(event.clone()),
            "REASONING_END" => self.reasoning_stage = None,
            "REASONING_MESSAGE_START" => self.reasoning = Some(event.clone()),
            "REASONING_MESSAGE_END" => self.reasoning = None,
            "TEXT_MESSAGE_START" => self.text = Some(event.clone()),
            "TEXT_MESSAGE_END" => self.text = None,
            "TOOL_CALL_START" => {
                self.tools.insert(id.to_string(), event.clone());
                self.page_tools.insert(id.to_string(), false);
            }
            "TOOL_CALL_ARGS" | "TOOL_CALL_END" | "TOOL_CALL_RESULT" => {
                if !self.page_tools.contains_key(id) {
                    let mut value = self.tools.get(id).map(|event| serde_json::from_str::<serde_json::Value>(&event.content).expect("generated CoT content"))
                        .unwrap_or_else(|| json!({"toolCallId":id,"toolCallName":summon_i18n::text!("cot.continued_tool")}));
                    value
                        .as_object_mut()
                        .expect("generated CoT object")
                        .remove("parentMessageId");
                    if let Some(reasoning) = &self.reasoning {
                        let parent: serde_json::Value = serde_json::from_str(&reasoning.content)
                            .expect("generated CoT content");
                        value["parentMessageId"] = parent["messageId"].clone();
                    }
                    let start = CotEvent::new("TOOL_CALL_START", value, &event.timestamp);
                    output.push(start);
                    self.page_tools.insert(id.to_string(), false);
                }
                if event.event_type == "TOOL_CALL_RESULT" && self.page_tools.get(id) == Some(&false)
                {
                    output.push(CotEvent::new(
                        "TOOL_CALL_END",
                        json!({"toolCallId":id}),
                        &event.timestamp,
                    ));
                }
                if event.event_type == "TOOL_CALL_END" {
                    self.page_tools.insert(id.to_string(), true);
                }
                if event.event_type == "TOOL_CALL_RESULT" {
                    self.tools.remove(id);
                    self.page_tools.remove(id);
                }
            }
            _ => {}
        }
        output.push(event.clone());
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str, content: serde_json::Value) -> CotEvent {
        CotEvent::new(kind, content, "1000")
    }

    fn content(event: &CotEvent) -> serde_json::Value {
        serde_json::from_str(&event.content).unwrap()
    }

    #[test]
    fn paging_keeps_all_content_once_with_bounded_requests_and_carriers() {
        let mut input = vec![
            event("RUN_STARTED", json!({"threadId":"thread","runId":"run"})),
            event(
                "TEXT_MESSAGE_START",
                json!({"messageId":"reply","role":"assistant"}),
            ),
        ];
        for i in 0..2400 {
            let mut item = event(
                "TEXT_MESSAGE_CONTENT",
                json!({"messageId":"reply","delta":format!("part-{i}")}),
            );
            item.reply_delivery = Some(super::super::ReplyDelivery {
                generation: 0,
                segment: 0,
                bytes: i + 1,
            });
            input.push(item);
        }
        input.push(event("TEXT_MESSAGE_END", json!({"messageId":"reply"})));
        input.push(event("RUN_FINISHED", json!({"runId":"run"})));
        let mut state = CotWireState::default();
        let mut cursor = 0;
        let mut pages = vec![Vec::new()];
        let mut confirmed = 0;
        while cursor < input.len() {
            let (batch, used, next) = state.batch(&input[cursor..]);
            if used == 0 {
                let (prelude, next) = state.next_page("1000");
                assert!(prelude.iter().all(|e| e.reply_delivery.is_none()));
                pages.push(prelude);
                state = next;
                continue;
            }
            assert!(batch.len() <= 50);
            confirmed += batch.iter().filter(|e| e.reply_delivery.is_some()).count();
            pages.last_mut().unwrap().extend(batch);
            cursor += used;
            // Restore after every successful batch, including active message and capacity state.
            state = serde_json::from_slice(&serde_json::to_vec(&next).unwrap()).unwrap();
        }
        assert_eq!(confirmed, 2400);
        assert_eq!(pages.len(), 3);
        assert!(pages.iter().all(|page| page.len() <= 900));
        let deltas: Vec<_> = pages
            .iter()
            .flatten()
            .filter(|e| e.event_type == "TEXT_MESSAGE_CONTENT")
            .map(content)
            .filter(|v| v["messageId"] == "reply")
            .map(|v| v["delta"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            deltas,
            (0..2400).map(|i| format!("part-{i}")).collect::<Vec<_>>()
        );
        let stamps: Vec<u64> = pages
            .iter()
            .flatten()
            .map(|e| e.timestamp.parse().unwrap())
            .collect();
        assert!(stamps.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(
            pages.last().unwrap().last().unwrap().event_type,
            "RUN_FINISHED"
        );
    }

    #[test]
    fn paging_restores_tool_headers_without_arguments_or_a_dangling_parent() {
        let initial = [
            event("RUN_STARTED", json!({"runId":"run"})),
            event("REASONING_START", json!({"messageId":"old"})),
            event(
                "REASONING_MESSAGE_START",
                json!({"messageId":"old","role":"reasoning"}),
            ),
            event(
                "TOOL_CALL_START",
                json!({"toolCallId":"tool","toolCallName":"read","parentMessageId":"old"}),
            ),
            event(
                "TOOL_CALL_ARGS",
                json!({"toolCallId":"tool","delta":"private-args"}),
            ),
            event("TOOL_CALL_END", json!({"toolCallId":"tool"})),
            event("REASONING_MESSAGE_END", json!({"messageId":"old"})),
            event("REASONING_END", json!({"messageId":"old"})),
        ];
        let (_, used, state) = CotWireState::default().batch(&initial);
        assert_eq!(used, initial.len());
        let (prelude, state) = state.next_page("1001");
        assert!(prelude.iter().all(|e| !e.content.contains("private-args")));
        let (batch, used, _) = state.batch(&[event(
            "TOOL_CALL_RESULT",
            json!({"toolCallId":"tool","content":"result"}),
        )]);
        assert_eq!(used, 1);
        assert_eq!(
            batch
                .iter()
                .map(|e| e.event_type.as_str())
                .collect::<Vec<_>>(),
            ["TOOL_CALL_START", "TOOL_CALL_END", "TOOL_CALL_RESULT"]
        );
        assert_eq!(content(&batch[0])["toolCallId"], "tool");
        assert_eq!(content(&batch[0])["toolCallName"], "read");
        assert!(content(&batch[0]).get("parentMessageId").is_none());
    }

    #[test]
    fn paging_can_split_between_reasoning_start_and_message_start() {
        let (_, _, state) = CotWireState::default().batch(&[
            event("RUN_STARTED", json!({"runId":"run"})),
            event("REASONING_START", json!({"messageId":"thinking"})),
        ]);
        let (prelude, state) = state.next_page("1001");
        assert_eq!(prelude.last().unwrap().event_type, "REASONING_START");
        let (batch, used, _) = state.batch(&[event(
            "REASONING_MESSAGE_START",
            json!({"messageId":"thinking","role":"reasoning"}),
        )]);
        assert_eq!(used, 1);
        assert_eq!(batch[0].event_type, "REASONING_MESSAGE_START");
    }

    #[test]
    fn paging_budgets_restored_tool_headers_before_accepting_a_result() {
        let state = CotWireState {
            events: 899,
            ..Default::default()
        };
        let result = event(
            "TOOL_CALL_RESULT",
            json!({"toolCallId":"late","content":"result"}),
        );
        let (batch, used, next) = state.batch(std::slice::from_ref(&result));
        assert!(batch.is_empty());
        assert_eq!(used, 0);
        assert_eq!(next.events(), 899);
        let (_, state) = state.next_page("1000");
        let (batch, used, next) = state.batch(&[result]);
        assert_eq!(used, 1);
        assert_eq!(batch.len(), 3);
        assert_eq!(next.events(), 6);
    }

    #[test]
    fn paging_moves_unknown_legacy_capacity_to_a_fresh_carrier() {
        let state = CotWireState::unknown(
            event("RUN_STARTED", json!({"runId":"run"})),
            Some(event(
                "REASONING_MESSAGE_START",
                json!({"messageId":"thinking","role":"reasoning"}),
            )),
        );
        let delta = event(
            "REASONING_MESSAGE_CONTENT",
            json!({"messageId":"thinking","delta":"new"}),
        );
        assert_eq!(state.batch(std::slice::from_ref(&delta)).1, 0);
        let (prelude, next) = state.next_page("1001");
        assert_eq!(
            prelude
                .iter()
                .filter(|e| e.event_type == "REASONING_START")
                .count(),
            1
        );
        assert_eq!(
            prelude
                .iter()
                .filter(|e| e.event_type == "REASONING_MESSAGE_START")
                .count(),
            1
        );
        assert_eq!(next.batch(&[delta]).1, 1);
    }
}
