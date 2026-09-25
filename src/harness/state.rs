use super::*;

pub fn scalar_json(state: &HarnessState) -> Result<String, String> {
    let mut probe = state.clone();
    probe.messages = Vec::new();
    probe.events = Vec::new();
    probe.history_rewritten = false;
    serde_json::to_string(&probe).map_err(|e| format!("serialize session scalar: {e}"))
}

/// Rebuild a session from its stored scalar plus the logs loaded from their
/// tables.
pub fn state_from_scalar(
    scalar: &str,
    messages: Vec<HarnessMessage>,
    events: Vec<HarnessEvent>,
) -> Result<HarnessState, String> {
    let mut state: HarnessState = serde_json::from_str(scalar)
        .map_err(|e| format!("deserialize session scalar: {e}"))?;
    state.messages = messages;
    state.events = events;
    state.history_rewritten = false;
    Ok(state)
}

pub fn serialize_state(state: &HarnessState) -> Result<Vec<u8>, String> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    // `to_vec_named` encodes structs as field-name → value maps. The positional
    // `to_vec` is NOT safe here: `HarnessState`'s `skip_serializing_if` fields
    // drop array elements when empty, which shifts every later field and breaks
    // the round-trip on read.
    let raw_bytes = rmp_serde::to_vec_named(state)
        .map_err(|e| format!("failed to serialize state to MessagePack: {e}"))?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(&raw_bytes)
        .map_err(|e| format!("failed to compress state with Gzip: {e}"))?;
    let compressed_bytes = encoder
        .finish()
        .map_err(|e| format!("failed to finalize Gzip compression: {e}"))?;
    Ok(compressed_bytes)
}

pub fn deserialize_state(bytes: &[u8]) -> Result<HarnessState, String> {
    use flate2::read::GzDecoder;
    use std::io::Read;

    // Try parsing as compressed MessagePack first
    let mut decoder = GzDecoder::new(bytes);
    let mut decompressed_bytes = Vec::new();
    if decoder.read_to_end(&mut decompressed_bytes).is_ok() {
        if let Ok(state) = rmp_serde::from_slice::<HarnessState>(&decompressed_bytes) {
            let mut state = state;
            normalize_state_title(&mut state);
            return Ok(state);
        }
    }

    // Fallback: try parsing as legacy JSON
    if let Ok(mut state) = serde_json::from_slice::<HarnessState>(bytes) {
        normalize_state_title(&mut state);
        return Ok(state);
    }

    Err(
        "failed to deserialize state: not a valid compressed MessagePack or legacy JSON"
            .to_string(),
    )
}

