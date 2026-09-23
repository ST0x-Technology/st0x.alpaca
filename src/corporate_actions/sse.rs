//! Bounded, incremental decoder for the corporate-action SSE stream.
//!
//! Frames are capped at 64 KiB and may end in any mix of CR, LF, and CRLF.
//! The SSE `id`/`event` fields and the payload `event_id`/`action` must agree
//! when both are present. A frame that fails to decode poisons the batch:
//! frames completed before it are still returned, and the decoder keeps none
//! of the rejected input.

use chrono::NaiveDate;
use serde::Deserialize;

use super::event::{
    CorporateActionEventId, CorporateActionId, CorporateActionMutation,
    CorporateActionMutationKind, CorporateActionSymbol, DividendCorporateAction,
};

const MAX_SSE_FRAME_BYTES: usize = 64 * 1024;
const MAX_SSE_SEPARATOR_BYTES: usize = 4;

/// Incrementally decodes bounded SSE frames into corporate-action mutations.
#[derive(Debug, Default)]
pub struct CorporateActionSseDecoder {
    buffer: Vec<u8>,
}

/// The mutations decoded from one pushed chunk.
#[derive(Debug)]
pub enum CorporateActionDecodeBatch {
    /// Every complete frame decoded; any trailing partial frame stays buffered.
    Complete(Vec<CorporateActionMutation>),
    /// A frame was rejected. `completed` holds the mutations decoded before
    /// it; the decoder buffer is released.
    Poison {
        completed: Vec<CorporateActionMutation>,
        error: CorporateActionStreamDecodeError,
    },
}

/// A frame rejected by [`CorporateActionSseDecoder::push`].
#[derive(Debug, thiserror::Error)]
pub enum CorporateActionStreamDecodeError {
    #[error("corporate-action SSE frame exceeded {MAX_SSE_FRAME_BYTES} bytes")]
    FrameTooLarge,
    #[error("corporate-action SSE frame for event {event_id:?} was not UTF-8")]
    InvalidUtf8 {
        event_id: Option<CorporateActionEventId>,
        #[source]
        source: std::str::Utf8Error,
    },
    #[error("{source}")]
    Event {
        event_id: Option<CorporateActionEventId>,
        #[source]
        source: CorporateActionDecodeError,
    },
}

impl CorporateActionStreamDecodeError {
    /// The validated event id of the rejected frame, when it carried one: its
    /// SSE `id`, or else a valid payload `event_id`.
    #[must_use]
    pub const fn event_id(&self) -> Option<&CorporateActionEventId> {
        match self {
            Self::InvalidUtf8 { event_id, .. } | Self::Event { event_id, .. } => event_id.as_ref(),
            Self::FrameTooLarge => None,
        }
    }
}

/// Why one complete SSE frame could not become a mutation.
#[derive(Debug, thiserror::Error)]
pub enum CorporateActionDecodeError {
    #[error("corporate-action SSE frame is missing its event id")]
    MissingEventId,
    #[error("invalid corporate-action event id {0}")]
    InvalidEventId(String),
    #[error("corporate-action SSE frame is missing its mutation event")]
    MissingMutation,
    #[error("unsupported corporate-action mutation {0}")]
    UnsupportedMutation(String),
    #[error(
        "corporate-action SSE event id {sse_event_id} does not match payload event id \
         {payload_event_id}"
    )]
    EventIdMismatch {
        sse_event_id: CorporateActionEventId,
        payload_event_id: String,
    },
    #[error(
        "corporate-action SSE mutation {sse_mutation} does not match payload action \
         {payload_action}"
    )]
    MutationMismatch {
        sse_mutation: String,
        payload_action: String,
    },
    #[error("corporate-action SSE field was not UTF-8")]
    InvalidFieldUtf8(#[source] std::str::Utf8Error),
    #[error("corporate-action SSE frame is missing its data payload")]
    MissingData,
    #[error("invalid corporate-action payload for event {event_id}: {source}")]
    InvalidPayload {
        event_id: CorporateActionEventId,
        #[source]
        source: serde_json::Error,
    },
    #[error("invalid corporate-action payload without an event id: {0}")]
    InvalidPayloadWithoutEventId(#[source] serde_json::Error),
    #[error("corporate-action stream returned non-US event")]
    NonUsRegion,
    #[error("invalid corporate-action id {0}")]
    InvalidActionId(String),
    #[error("invalid corporate-action symbol {0}")]
    InvalidUnderlying(String),
}

impl CorporateActionSseDecoder {
    /// True while a partial frame is buffered. A bounded replay that reaches
    /// EOF in this state ended mid-frame.
    #[must_use]
    pub const fn has_pending_frame(&self) -> bool {
        !self.buffer.is_empty()
    }

    /// Incrementally decodes bounded SSE frames without retaining poisoned
    /// input. Complete frames preceding a poison boundary are returned so the
    /// caller can commit them before stopping at the rejected event.
    pub fn push(&mut self, chunk: &[u8]) -> CorporateActionDecodeBatch {
        let mut remaining = chunk;
        let mut mutations = Vec::new();

        while !remaining.is_empty() {
            let buffer_limit = MAX_SSE_FRAME_BYTES + MAX_SSE_SEPARATOR_BYTES;
            let available = buffer_limit.saturating_sub(self.buffer.len());
            if available == 0 {
                self.buffer = Vec::new();
                return CorporateActionDecodeBatch::Poison {
                    completed: mutations,
                    error: CorporateActionStreamDecodeError::FrameTooLarge,
                };
            }
            let accepted = remaining.len().min(available);
            self.buffer.extend_from_slice(&remaining[..accepted]);
            remaining = &remaining[accepted..];

            while let Some((frame_end, separator_len)) = frame_boundary(&self.buffer) {
                if frame_end > MAX_SSE_FRAME_BYTES {
                    self.buffer = Vec::new();
                    return CorporateActionDecodeBatch::Poison {
                        completed: mutations,
                        error: CorporateActionStreamDecodeError::FrameTooLarge,
                    };
                }
                let frame = self.buffer[..frame_end].to_vec();
                self.buffer.drain(..frame_end + separator_len);
                let event_identity = sse_event_identity(&frame);
                let frame = match std::str::from_utf8(&frame) {
                    Ok(frame) => frame,
                    Err(source) => {
                        self.buffer = Vec::new();
                        let event_id = match event_identity {
                            SseEventIdentity::Valid(event_id) => Some(event_id),
                            SseEventIdentity::Absent | SseEventIdentity::Invalid => None,
                        };
                        return CorporateActionDecodeBatch::Poison {
                            completed: mutations,
                            error: CorporateActionStreamDecodeError::InvalidUtf8 {
                                event_id,
                                source,
                            },
                        };
                    }
                };
                if sse_lines(frame.as_bytes()).all(|line| line.is_empty() || line.starts_with(b":"))
                {
                    continue;
                }
                let event_id = match event_identity {
                    SseEventIdentity::Absent => validated_payload_event_id(frame),
                    SseEventIdentity::Valid(event_id) => Some(event_id),
                    SseEventIdentity::Invalid => None,
                };
                let mutation = match decode_sse_frame(frame) {
                    Ok(mutation) => mutation,
                    Err(source) => {
                        self.buffer = Vec::new();
                        return CorporateActionDecodeBatch::Poison {
                            completed: mutations,
                            error: CorporateActionStreamDecodeError::Event { event_id, source },
                        };
                    }
                };
                mutations.push(mutation);
            }

            if !can_still_terminate_within_limit(&self.buffer) {
                self.buffer = Vec::new();
                return CorporateActionDecodeBatch::Poison {
                    completed: mutations,
                    error: CorporateActionStreamDecodeError::FrameTooLarge,
                };
            }
        }

        CorporateActionDecodeBatch::Complete(mutations)
    }
}

#[derive(Debug, Deserialize)]
struct CorporateActionIdentityEnvelope {
    event_id: Option<CorporateActionEventId>,
}

#[derive(Debug, Deserialize)]
struct CorporateActionEnvelope {
    event_id: Option<String>,
    action: Option<String>,
    event_type: DividendCorporateActionEventType,
    region: CorporateActionRegion,
    ca: DividendCorporateActionPayload,
}

#[derive(Debug, Deserialize)]
enum DividendCorporateActionEventType {
    #[serde(rename = "cash_dividend_corporateaction_event")]
    CashDividend,
    #[serde(rename = "stock_dividend_corporateaction_event")]
    StockDividend,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CorporateActionRegion {
    Us,
    NonUs,
}

#[derive(Debug, Deserialize)]
struct DividendCorporateActionPayload {
    id: String,
    symbol: String,
    ex_date: NaiveDate,
}

#[derive(Debug, Clone)]
enum SseEventIdentity {
    Absent,
    Valid(CorporateActionEventId),
    Invalid,
}

fn sse_event_identity(frame: &[u8]) -> SseEventIdentity {
    let Some(value) = sse_lines(frame)
        .filter_map(|line| {
            let (field, value) = sse_field(line);
            (field == b"id").then_some(value)
        })
        .last()
    else {
        return SseEventIdentity::Absent;
    };
    let value = value.strip_prefix(b" ").unwrap_or(value);
    let Ok(value) = std::str::from_utf8(value) else {
        return SseEventIdentity::Invalid;
    };

    CorporateActionEventId::new(value).map_or(SseEventIdentity::Invalid, SseEventIdentity::Valid)
}

fn validated_payload_event_id(frame: &str) -> Option<CorporateActionEventId> {
    let data = sse_lines(frame.as_bytes())
        .filter_map(|line| {
            let (field, value) = sse_field(line);
            (field == b"data").then_some(value)
        })
        .map(|value| value.strip_prefix(b" ").unwrap_or(value))
        .map(std::str::from_utf8)
        .collect::<Result<Vec<_>, _>>()
        .ok()?
        .join("\n");
    if data.is_empty() {
        return None;
    }

    serde_json::from_str::<CorporateActionIdentityEnvelope>(&data)
        .ok()
        .and_then(|envelope| envelope.event_id)
}

fn sse_lines(frame: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut remaining = frame;

    std::iter::from_fn(move || {
        if remaining.is_empty() {
            return None;
        }
        let Some(line_end) = remaining
            .iter()
            .position(|byte| matches!(*byte, b'\r' | b'\n'))
        else {
            let line = remaining;
            remaining = &[];
            return Some(line);
        };
        let line = &remaining[..line_end];
        let ending_len = line_ending_len(&remaining[line_end..])?;
        remaining = &remaining[line_end + ending_len..];
        Some(line)
    })
}

fn sse_field(line: &[u8]) -> (&[u8], &[u8]) {
    line.iter()
        .position(|byte| *byte == b':')
        .map_or((line, &[]), |colon| (&line[..colon], &line[colon + 1..]))
}

fn line_ending_len(input: &[u8]) -> Option<usize> {
    match input {
        [b'\r', b'\n', ..] => Some(2),
        [b'\r' | b'\n', ..] => Some(1),
        _ => None,
    }
}

fn can_still_terminate_within_limit(buffer: &[u8]) -> bool {
    const SEPARATORS: [&[u8]; 7] = [
        b"\n\n",
        b"\n\r",
        b"\n\r\n",
        b"\r\r",
        b"\r\n\n",
        b"\r\n\r",
        b"\r\n\r\n",
    ];

    if buffer.len() <= MAX_SSE_FRAME_BYTES {
        return true;
    }

    let separator_prefix = &buffer[MAX_SSE_FRAME_BYTES..];
    SEPARATORS
        .iter()
        .any(|separator| separator.starts_with(separator_prefix))
}

fn frame_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    (0..buffer.len()).find_map(|frame_end| {
        let first_len = line_ending_len(&buffer[frame_end..])?;
        let second_start = frame_end + first_len;
        let second_len = line_ending_len(buffer.get(second_start..)?)?;
        Some((frame_end, first_len + second_len))
    })
}

fn decode_sse_frame(frame: &str) -> Result<CorporateActionMutation, CorporateActionDecodeError> {
    let mut event_id = None;
    let mut mutation = None;
    let mut data = Vec::new();

    for line in sse_lines(frame.as_bytes()) {
        if line.is_empty() || line.starts_with(b":") {
            continue;
        }
        let (field, value) = sse_field(line);
        let value = value.strip_prefix(b" ").unwrap_or(value);
        let value =
            std::str::from_utf8(value).map_err(CorporateActionDecodeError::InvalidFieldUtf8)?;
        match field {
            b"id" => event_id = Some(value.to_string()),
            b"event" => mutation = Some(value.to_string()),
            b"data" => data.push(value),
            _ => {}
        }
    }

    if data.is_empty() {
        return Err(CorporateActionDecodeError::MissingData);
    }
    let sse_event_id = event_id
        .map(|event_id| {
            CorporateActionEventId::new(&event_id)
                .ok_or(CorporateActionDecodeError::InvalidEventId(event_id))
        })
        .transpose()?;
    let envelope: CorporateActionEnvelope =
        serde_json::from_str(&data.join("\n")).map_err(|source| {
            if let Some(event_id) = sse_event_id.clone() {
                CorporateActionDecodeError::InvalidPayload { event_id, source }
            } else {
                CorporateActionDecodeError::InvalidPayloadWithoutEventId(source)
            }
        })?;
    let event_id = match (sse_event_id, envelope.event_id) {
        (Some(sse_event_id), Some(payload_event_id))
            if sse_event_id.as_str() != payload_event_id =>
        {
            return Err(CorporateActionDecodeError::EventIdMismatch {
                sse_event_id,
                payload_event_id,
            });
        }
        (Some(sse_event_id), _) => sse_event_id,
        (None, Some(payload_event_id)) => CorporateActionEventId::new(&payload_event_id)
            .ok_or(CorporateActionDecodeError::InvalidEventId(payload_event_id))?,
        (None, None) => {
            return Err(CorporateActionDecodeError::MissingEventId);
        }
    };
    let mutation = match (mutation, envelope.action) {
        (Some(sse_mutation), Some(payload_action)) if sse_mutation != payload_action => {
            return Err(CorporateActionDecodeError::MutationMismatch {
                sse_mutation,
                payload_action,
            });
        }
        (Some(sse_mutation), _) => sse_mutation,
        (None, Some(payload_action)) => payload_action,
        (None, None) => {
            return Err(CorporateActionDecodeError::MissingMutation);
        }
    };
    let kind = CorporateActionMutationKind::parse(&mutation)
        .ok_or_else(|| CorporateActionDecodeError::UnsupportedMutation(mutation.clone()))?;
    match envelope.event_type {
        DividendCorporateActionEventType::CashDividend
        | DividendCorporateActionEventType::StockDividend => {}
    }
    if matches!(envelope.region, CorporateActionRegion::NonUs) {
        return Err(CorporateActionDecodeError::NonUsRegion);
    }
    let action_id = CorporateActionId::new(&envelope.ca.id)
        .ok_or_else(|| CorporateActionDecodeError::InvalidActionId(envelope.ca.id.clone()))?;
    let underlying = CorporateActionSymbol::new(&envelope.ca.symbol)
        .ok_or_else(|| CorporateActionDecodeError::InvalidUnderlying(envelope.ca.symbol.clone()))?;

    Ok(CorporateActionMutation {
        event_id,
        kind,
        action: DividendCorporateAction {
            id: action_id,
            underlying,
            ex_date: envelope.ca.ex_date,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete(batch: CorporateActionDecodeBatch) -> Vec<CorporateActionMutation> {
        match batch {
            CorporateActionDecodeBatch::Complete(mutations) => mutations,
            CorporateActionDecodeBatch::Poison { error, .. } => {
                panic!("expected a complete decode batch, got {error}")
            }
        }
    }

    fn poison(batch: CorporateActionDecodeBatch) -> CorporateActionStreamDecodeError {
        match batch {
            CorporateActionDecodeBatch::Poison { error, .. } => error,
            CorporateActionDecodeBatch::Complete(mutations) => panic!(
                "expected a poison decode batch, got {} mutations",
                mutations.len()
            ),
        }
    }

    #[test]
    fn decodes_the_documented_cash_dividend_insert_envelope() {
        let mutation = decode_sse_frame(
            "data: {\"action\":\"insert\",\"at\":\"2026-03-20T12:24:58.807230Z\",\"ca\":{\"currency\":\"USD\",\"cusip\":\"037833100\",\"ex_date\":\"2026-08-14\",\"foreign\":false,\"id\":\"ca-1\",\"payable_date\":\"2026-08-20\",\"process_date\":\"2026-08-20\",\"rate\":\"0.25\",\"record_date\":\"2026-08-15\",\"special\":false,\"symbol\":\"AAPL\"},\"event_id\":\"01J9RPMV5TKB8WX3M4F1KZ7QH2\",\"event_type\":\"cash_dividend_corporateaction_event\",\"region\":\"us\"}",
        )
        .unwrap();

        assert_eq!(mutation.kind, CorporateActionMutationKind::Insert);
        assert_eq!(mutation.event_id.as_str(), "01J9RPMV5TKB8WX3M4F1KZ7QH2");
        assert_eq!(mutation.action.id.as_str(), "ca-1");
        assert_eq!(mutation.action.underlying.as_str(), "AAPL");
        assert_eq!(mutation.action.ex_date.to_string(), "2026-08-14");
    }

    #[test]
    fn decodes_a_documented_dividend_delete_payload() {
        let mutation = decode_sse_frame(
            "data: {\"action\":\"delete\",\"at\":\"2026-03-20T12:24:58.807230Z\",\"ca\":{\"currency\":\"USD\",\"cusip\":\"037833100\",\"ex_date\":\"2026-08-14\",\"foreign\":false,\"id\":\"ca-1\",\"process_date\":\"2026-08-20\",\"rate\":\"0.25\",\"special\":false,\"symbol\":\"AAPL\"},\"event_id\":\"01J9RPMV5TKB8WX3M4F1KZ7QH2\",\"event_type\":\"cash_dividend_corporateaction_event\",\"region\":\"us\"}",
        )
        .unwrap();

        assert_eq!(mutation.kind, CorporateActionMutationKind::Delete);
        assert_eq!(mutation.action.id.as_str(), "ca-1");
        assert_eq!(mutation.action.underlying.as_str(), "AAPL");
        assert_eq!(mutation.action.ex_date.to_string(), "2026-08-14");
    }

    #[test]
    fn rejects_mismatched_sse_and_payload_identity() {
        let error = decode_sse_frame(
            "id: 01J9RPMV5TKB8WX3M4F1KZ7QH2\nevent: insert\ndata: {\"action\":\"insert\",\"ca\":{\"id\":\"ca-1\",\"symbol\":\"AAPL\",\"ex_date\":\"2026-08-14\"},\"event_id\":\"01J9RPMV5TKB8WX3M4F1KZ7QH3\",\"event_type\":\"cash_dividend_corporateaction_event\",\"region\":\"us\"}",
        )
        .unwrap_err();

        assert!(matches!(
            error,
            CorporateActionDecodeError::EventIdMismatch {
                sse_event_id,
                ..
            } if sse_event_id.as_str() == "01J9RPMV5TKB8WX3M4F1KZ7QH2"
        ));
    }

    #[test]
    fn rejects_a_bare_final_sse_id_without_payload_fallback() {
        let event_id = "01J9RPMV5TKB8WX3M4F1KZ7QH2";
        let frame = format!(
            "id: {event_id}\nid\nevent: insert\ndata: {{\"event_id\":\"{event_id}\",\"event_type\":\"cash_dividend_corporateaction_event\",\"region\":\"us\",\"ca\":{{\"id\":\"ca-1\",\"symbol\":\"AAPL\",\"ex_date\":\"2026-08-14\"}}}}\n\n"
        );
        let mut decoder = CorporateActionSseDecoder::default();

        let error = poison(decoder.push(frame.as_bytes()));

        assert!(error.event_id().is_none());
    }

    #[test]
    fn rejects_an_undocumented_mutation() {
        let error = decode_sse_frame(
            "id: 01J9RPMV5TKB8WX3M4F1KZ7QH2\nevent: revise\ndata: {\"event_type\":\"cash_dividend_corporateaction_event\",\"region\":\"us\",\"ca\":{\"id\":\"ca-1\",\"symbol\":\"AAPL\",\"ex_date\":\"2026-08-14\"}}",
        )
        .unwrap_err();

        assert!(matches!(
            error,
            CorporateActionDecodeError::UnsupportedMutation(_)
        ));
    }

    #[test]
    fn buffers_fragmented_crlf_frames() {
        let mut decoder = CorporateActionSseDecoder::default();
        assert!(
            complete(decoder.push(
                b"id: 01J9RPMV5TKB8WX3M4F1KZ7QH2\r\nevent: insert\r\ndata: {\"event_type\":\"cash_dividend_corporateaction_event\","
            ))
            .is_empty()
        );

        let mutations = complete(decoder.push(
            b"\"region\":\"us\",\"ca\":{\"id\":\"ca-1\",\"symbol\":\"AAPL\",\"ex_date\":\"2026-08-14\"}}\r\n\r\n",
        ));

        assert_eq!(mutations.len(), 1);
        assert_eq!(mutations[0].action.id.as_str(), "ca-1");
    }

    #[test]
    fn rejects_an_oversized_partial_frame_without_retaining_it() {
        let mut decoder = CorporateActionSseDecoder::default();
        let error = poison(decoder.push(&vec![b'x'; MAX_SSE_FRAME_BYTES + 1]));

        assert!(matches!(
            error,
            CorporateActionStreamDecodeError::FrameTooLarge
        ));
        assert!(
            decoder.buffer.is_empty(),
            "an oversized untrusted frame must not remain allocated"
        );
        assert_eq!(
            decoder.buffer.capacity(),
            0,
            "rejecting an oversized frame must release its retained capacity"
        );
    }

    #[test]
    fn decodes_a_mixed_lf_crlf_frame_separator() {
        let mut decoder = CorporateActionSseDecoder::default();
        let mutations = complete(decoder.push(
            b"id: 01J9RPMV5TKB8WX3M4F1KZ7QH2\nevent: insert\ndata: {\"event_type\":\"cash_dividend_corporateaction_event\",\"region\":\"us\",\"ca\":{\"id\":\"ca-1\",\"symbol\":\"AAPL\",\"ex_date\":\"2026-08-14\"}}\n\r\n",
        ));

        assert_eq!(mutations.len(), 1);
        assert_eq!(mutations[0].action.id.as_str(), "ca-1");
    }

    #[test]
    fn accepts_a_split_crlf_separator_at_the_frame_limit() {
        let mut decoder = CorporateActionSseDecoder::default();
        let mut first_chunk = vec![b'x'; MAX_SSE_FRAME_BYTES - 1];
        first_chunk[0] = b':';
        first_chunk.extend_from_slice(b"\r\n");

        assert!(complete(decoder.push(&first_chunk)).is_empty());
        assert!(complete(decoder.push(b"\r\n")).is_empty());
        assert!(decoder.buffer.is_empty());
    }

    #[test]
    fn invalid_payload_error_retains_the_valid_sse_event_id() {
        let event_id = "01J9RPMV5TKB8WX3M4F1KZ7QH2";
        let frame = format!("id: {event_id}\nevent: insert\ndata: not-json\n\n");
        let mut decoder = CorporateActionSseDecoder::default();
        let error = poison(decoder.push(frame.as_bytes()));

        assert!(
            error.to_string().contains(event_id),
            "a poison-event error must retain its safe replay identity: {error}"
        );
        assert_eq!(
            error.event_id().map(CorporateActionEventId::as_str),
            Some(event_id),
            "the poison log must expose the validated event ID as a structured field"
        );
    }

    #[test]
    fn returns_completed_frames_before_a_poison_frame() {
        let event_id = "01J9RPMV5TKB8WX3M4F1KZ7QH2";
        let valid = format!(
            "id: {event_id}\nevent: insert\ndata: {{\"event_type\":\"cash_dividend_corporateaction_event\",\"region\":\"us\",\"ca\":{{\"id\":\"ca-1\",\"symbol\":\"AAPL\",\"ex_date\":\"2026-08-14\"}}}}\n\n"
        );
        let poison_event_id = "01J9RPMV5TKB8WX3M4F1KZ7QH3";
        let chunk = format!("{valid}id: {poison_event_id}\nevent: insert\ndata: not-json\n\n");
        let mut decoder = CorporateActionSseDecoder::default();

        let CorporateActionDecodeBatch::Poison { completed, error } =
            decoder.push(chunk.as_bytes())
        else {
            panic!("expected the second frame to poison the batch");
        };

        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].event_id.as_str(), event_id);
        assert!(matches!(
            error,
            CorporateActionStreamDecodeError::Event {
                event_id: Some(ref event_id),
                ..
            } if event_id.as_str() == poison_event_id
        ));
        assert_eq!(decoder.buffer.capacity(), 0);
    }

    #[test]
    fn invalid_utf8_error_retains_the_valid_sse_event_id() {
        let event_id = "01J9RPMV5TKB8WX3M4F1KZ7QH2";
        let mut frame = format!("id: {event_id}\nevent: insert\ndata: ").into_bytes();
        frame.push(0xff);
        frame.extend_from_slice(b"\n\n");
        let mut decoder = CorporateActionSseDecoder::default();
        let error = poison(decoder.push(&frame));

        assert_eq!(
            error.event_id().map(CorporateActionEventId::as_str),
            Some(event_id),
            "invalid UTF-8 telemetry must retain a validated ASCII SSE identity"
        );
    }

    #[test]
    fn semantic_poison_error_retains_the_valid_sse_event_id() {
        let event_id = "01J9RPMV5TKB8WX3M4F1KZ7QH2";
        let frame = format!(
            "id: {event_id}\nevent: insert\ndata: {{\"event_type\":\"cash_dividend_corporateaction_event\",\"region\":\"us\",\"ca\":{{\"id\":\"\",\"symbol\":\"AAPL\",\"ex_date\":\"2026-08-14\"}}}}\n\n"
        );
        let mut decoder = CorporateActionSseDecoder::default();
        let error = poison(decoder.push(frame.as_bytes()));

        assert_eq!(
            error.event_id().map(CorporateActionEventId::as_str),
            Some(event_id),
            "semantic poison telemetry must retain the validated SSE identity"
        );
    }

    #[test]
    fn payload_only_semantic_poison_retains_its_valid_event_id() {
        let event_id = "01J9RPMV5TKB8WX3M4F1KZ7QH2";
        let frame = format!(
            "event: insert\ndata: {{\"event_id\":\"{event_id}\",\"event_type\":\"cash_dividend_corporateaction_event\",\"region\":\"us\",\"ca\":{{\"id\":\"\",\"symbol\":\"AAPL\",\"ex_date\":\"2026-08-14\"}}}}\n\n"
        );
        let mut decoder = CorporateActionSseDecoder::default();
        let error = poison(decoder.push(frame.as_bytes()));

        assert_eq!(
            error.event_id().map(CorporateActionEventId::as_str),
            Some(event_id),
            "payload-only poison telemetry must retain its validated replay identity"
        );
    }

    #[test]
    fn rejects_a_non_us_region_and_a_blank_symbol() {
        let non_us = decode_sse_frame(
            "id: 01J9RPMV5TKB8WX3M4F1KZ7QH2\nevent: insert\ndata: {\"event_type\":\"cash_dividend_corporateaction_event\",\"region\":\"non_us\",\"ca\":{\"id\":\"ca-1\",\"symbol\":\"AAPL\",\"ex_date\":\"2026-08-14\"}}",
        )
        .unwrap_err();
        assert!(matches!(non_us, CorporateActionDecodeError::NonUsRegion));

        let blank_symbol = decode_sse_frame(
            "id: 01J9RPMV5TKB8WX3M4F1KZ7QH2\nevent: insert\ndata: {\"event_type\":\"cash_dividend_corporateaction_event\",\"region\":\"us\",\"ca\":{\"id\":\"ca-1\",\"symbol\":\"  \",\"ex_date\":\"2026-08-14\"}}",
        )
        .unwrap_err();
        assert!(matches!(
            blank_symbol,
            CorporateActionDecodeError::InvalidUnderlying(symbol) if symbol == "  "
        ));
    }
}
