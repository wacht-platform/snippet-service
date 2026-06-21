use futures_util::StreamExt;

/// Read a `text/event-stream` response body line by line, invoking `on_data`
/// with the payload of each `data:` line (the text after `data:`). Non-data
/// lines (`event:`, comments, blank separators) are ignored — both Anthropic
/// and OpenAI carry the full event JSON, including its own `type`, on the data
/// line, so the `event:` line is redundant. Bytes are buffered until a newline
/// so a multi-byte char split across chunks never corrupts a decoded line.
pub async fn for_each_event<F>(response: reqwest::Response, mut on_data: F) -> Result<(), String>
where
    F: FnMut(&str),
{
    let mut stream = response.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        buf.extend_from_slice(&chunk);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = line.strip_suffix(b"\n").unwrap_or(&line);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if let Ok(text) = std::str::from_utf8(line) {
                if let Some(data) = text.strip_prefix("data:") {
                    on_data(data.trim_start());
                }
            }
        }
    }
    Ok(())
}
