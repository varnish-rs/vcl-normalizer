//! Parses `varnishadm vcl.show -v <name>` dumps (as captured by
//! `varnishgather`): a whole VCL include-tree concatenated into one text
//! blob, each file's raw content prefixed with a `// VCL.SHOW X Y FILENAME`
//! marker line. See the plan for the full format spec and the pre-order-DFS
//! chunk-ordering rationale.
//!
//! FROZEN CONTRACT: `VclShowChunk`'s shape is depended on by `src/lexer.rs`.
//! If it needs to change, coordinate before editing.

use crate::ast;

#[derive(Debug, Clone, PartialEq)]
pub struct VclShowChunk {
    pub index: u32,
    pub declared_len: u32,
    pub filename: String,
    pub content: String,
}

/// Parses a full `vcl.show`-dump text into its ordered chunks. Strips an
/// optional leading cartouche. Validates: index sequence is 0,1,2,... with
/// no gaps; every chunk's content is exactly `declared_len` bytes.
pub fn parse_chunks(text: &str) -> ast::Result<Vec<VclShowChunk>> {
    let mut pos = 0usize;
    let bytes = text.as_bytes();
    let len = bytes.len();

    // Check for optional 3-line cartouche: dash line, "Item..." line, dash line.
    // Cartouche must be at the very start of the file.
    if let Some(first_newline_pos) = find_line_end(bytes, pos) {
        let first_line = &text[pos..first_newline_pos];
        if is_all_dashes(first_line) && first_line.len() >= 3 {
            // Looks like a cartouche start. Check for Item line and another dash line.
            let after_first = first_newline_pos + 1;
            if let Some(second_newline_pos) = find_line_end(bytes, after_first) {
                let second_line = &text[after_first..second_newline_pos];
                if second_line.starts_with("Item") {
                    let after_second = second_newline_pos + 1;
                    if let Some(third_newline_pos) = find_line_end(bytes, after_second) {
                        let third_line = &text[after_second..third_newline_pos];
                        if is_all_dashes(third_line) && third_line.len() >= 3 {
                            // Valid cartouche found, skip past it.
                            pos = third_newline_pos + 1;
                        }
                    }
                }
            }
        }
    }

    let mut chunks = Vec::new();
    let mut expected_index = 0u32;

    while pos < len {
        // Check if we're at the start of a marker line.
        if bytes[pos..].starts_with(b"// VCL.SHOW ") {
            // Find the end of this line.
            let marker_line_end = find_line_end(bytes, pos).ok_or_else(|| {
                ast::VclError::new(ast::Span::dummy(), "marker line has no newline".to_string())
            })?;

            // Parse the marker line (skip the prefix "// VCL.SHOW ").
            let marker_content_start = pos + 12; // len("// VCL.SHOW ")
            let marker_content_end = marker_line_end;
            let marker_line_text = &text[marker_content_start..marker_content_end];

            // Parse: <index> <declared_len> <filename>
            let parts: Vec<&str> = marker_line_text.split_whitespace().collect();
            if parts.len() < 3 {
                return Err(ast::VclError::new(
                    ast::Span::dummy(),
                    format!(
                        "malformed VCL.SHOW marker: expected at least 3 fields, got {}",
                        parts.len()
                    ),
                ));
            }

            let index: u32 = parts[0].parse().map_err(|_| {
                ast::VclError::new(
                    ast::Span::dummy(),
                    format!("invalid chunk index '{}': expected u32", parts[0]),
                )
            })?;

            let declared_len: u32 = parts[1].parse().map_err(|_| {
                ast::VclError::new(
                    ast::Span::dummy(),
                    format!("invalid declared length '{}': expected u32", parts[1]),
                )
            })?;

            // Filename is everything after the second space (may contain spaces).
            let filename = parts[2..].join(" ");

            // Validate index sequence.
            if index != expected_index {
                return Err(ast::VclError::new(
                    ast::Span::dummy(),
                    format!(
                        "chunk index mismatch: expected {}, got {} (filename: {})",
                        expected_index, index, filename
                    ),
                ));
            }
            expected_index += 1;

            // Content starts right after the marker line's newline.
            let content_start = marker_line_end + 1;

            // Content length is exactly declared_len bytes.
            let content_end = content_start + declared_len as usize;

            // Validate that we don't read past the end of the text.
            if content_end > len {
                return Err(ast::VclError::new(
                    ast::Span::dummy(),
                    format!(
                        "chunk {} ({}): content extends past end of input (declared length {})",
                        index, filename, declared_len
                    ),
                ));
            }

            // Extract the content as a byte slice (exact substring).
            let content = &text[content_start..content_end];

            chunks.push(VclShowChunk {
                index,
                declared_len,
                filename,
                content: content.to_string(),
            });

            // Move past the content to the next chunk.
            // Skip any whitespace (newlines/carriage returns) between content and next marker.
            pos = content_end;
            while pos < len && (bytes[pos] == b'\n' || bytes[pos] == b'\r') {
                pos += 1;
            }
        } else {
            return Err(ast::VclError::new(
                ast::Span::dummy(),
                "no VCL.SHOW marker found in input".to_string(),
            ));
        }
    }

    if chunks.is_empty() {
        return Err(ast::VclError::new(
            ast::Span::dummy(),
            "no VCL.SHOW chunks found in input".to_string(),
        ));
    }

    Ok(chunks)
}

/// Find the position of the newline character at the end of the current line,
/// trimming a trailing `\r` if present. Returns the position of the `\n`.
fn find_line_end(bytes: &[u8], pos: usize) -> Option<usize> {
    bytes[pos..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|i| pos + i)
}

/// Check if a line (without the newline) consists entirely of `-` characters.
fn is_all_dashes(line: &str) -> bool {
    !line.is_empty() && line.trim_end_matches('\r').chars().all(|c| c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_cartouche_single_chunk() {
        let content = "hello world";
        let text = format!("// VCL.SHOW 0 {} test.vcl\n{}", content.len(), content);
        let chunks = parse_chunks(&text).expect("should parse");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].index, 0);
        assert_eq!(chunks[0].declared_len, content.len() as u32);
        assert_eq!(chunks[0].filename, "test.vcl");
        assert_eq!(chunks[0].content, content);
    }

    #[test]
    fn test_cartouche_present() {
        let content = "hello world";
        let cartouche = "---\nItem 1: test\n---\n";
        let text = format!(
            "{}// VCL.SHOW 0 {} test.vcl\n{}",
            cartouche,
            content.len(),
            content
        );
        let chunks = parse_chunks(&text).expect("should parse");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].index, 0);
        assert_eq!(chunks[0].content, content);
    }

    #[test]
    fn test_multiple_chunks() {
        let content1 = "first";
        let content2 = "second chunk";
        let content3 = "third";
        let text = format!(
            "// VCL.SHOW 0 {} file1.vcl\n{}\n// VCL.SHOW 1 {} file2.vcl\n{}\n// VCL.SHOW 2 {} file3.vcl\n{}",
            content1.len(),
            content1,
            content2.len(),
            content2,
            content3.len(),
            content3
        );
        let chunks = parse_chunks(&text).expect("should parse");
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].index, 0);
        assert_eq!(chunks[0].content, content1);
        assert_eq!(chunks[1].index, 1);
        assert_eq!(chunks[1].content, content2);
        assert_eq!(chunks[2].index, 2);
        assert_eq!(chunks[2].content, content3);
    }

    #[test]
    fn test_multiple_chunks_with_newlines() {
        let content1 = "first\nline\n";
        let content2 = "second\nchunk\nwith\nnewlines\n";
        let text = format!(
            "// VCL.SHOW 0 {} file1.vcl\n{}// VCL.SHOW 1 {} file2.vcl\n{}",
            content1.len(),
            content1,
            content2.len(),
            content2
        );
        let chunks = parse_chunks(&text).expect("should parse");
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].content, content1);
        assert_eq!(chunks[0].content.len() as u32, chunks[0].declared_len);
        assert_eq!(chunks[1].content, content2);
        assert_eq!(chunks[1].content.len() as u32, chunks[1].declared_len);
    }

    #[test]
    fn test_declared_length_mismatch() {
        let content = "hello";
        // Declare length as 10 but content is only 5 bytes
        let text = format!("// VCL.SHOW 0 10 test.vcl\n{}", content);
        let result = parse_chunks(&text);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.msg.contains("declared length 10") || err.msg.contains("extends past end"));
    }

    #[test]
    fn test_index_gap() {
        let text = "// VCL.SHOW 0 2 file0.vcl\naa\n// VCL.SHOW 2 2 file2.vcl\nbb";
        let result = parse_chunks(text);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.msg.contains("expected 1"));
        assert!(err.msg.contains("got 2"));
    }

    #[test]
    fn test_index_duplicate() {
        let text = "// VCL.SHOW 0 2 file0.vcl\naa\n// VCL.SHOW 0 2 file1.vcl\nbb";
        let result = parse_chunks(text);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.msg.contains("expected 1"));
        assert!(err.msg.contains("got 0"));
    }

    #[test]
    fn test_no_marker_line() {
        let text = "this is not a vcl.show dump\nno markers here";
        let result = parse_chunks(text);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.msg.contains("no VCL.SHOW marker found"));
    }

    #[test]
    fn test_empty_input() {
        let text = "";
        let result = parse_chunks(text);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.msg.contains("no VCL.SHOW chunks found"));
    }

    #[test]
    fn test_filename_builtin() {
        let content = "builtin vcl";
        let text = format!("// VCL.SHOW 0 {} Builtin\n{}", content.len(), content);
        let chunks = parse_chunks(&text).expect("should parse");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].filename, "Builtin");
        assert_eq!(chunks[0].content, content);
    }

    #[test]
    fn test_filename_with_spaces() {
        let content = "test content";
        let text = format!(
            "// VCL.SHOW 0 {} /path/to/file with spaces.vcl\n{}",
            content.len(),
            content
        );
        let chunks = parse_chunks(&text).expect("should parse");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].filename, "/path/to/file with spaces.vcl");
    }

    #[test]
    fn test_cartouche_with_many_dashes() {
        let cartouche = &"-".repeat(100);
        let content = "test";
        let text = format!(
            "{}\nItem something\n{}\n// VCL.SHOW 0 {} file.vcl\n{}",
            cartouche,
            cartouche,
            content.len(),
            content
        );
        let chunks = parse_chunks(&text).expect("should parse");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].content, content);
    }

    #[test]
    fn test_content_with_marker_like_text() {
        // Content that looks like a marker but isn't followed by space and index
        let content = "some code\n// VCL.SHOW not a marker";
        let text = format!("// VCL.SHOW 0 {} file.vcl\n{}", content.len(), content);
        let chunks = parse_chunks(&text).expect("should parse");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].content, content);
        // Verify the content length matches
        assert_eq!(chunks[0].declared_len as usize, chunks[0].content.len());
    }

    #[test]
    fn test_content_with_carriage_returns() {
        let content = "line1\r\nline2\r\n";
        let text = format!("// VCL.SHOW 0 {} file.vcl\n{}", content.len(), content);
        let chunks = parse_chunks(&text).expect("should parse");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].content, content);
        assert_eq!(chunks[0].content.len() as u32, chunks[0].declared_len);
    }

    #[test]
    fn test_all_three_chunks_in_sequence() {
        let c0 = "root vcl";
        let c1 = "included file 1";
        let c2 = "included file 2";
        let text = format!(
            "// VCL.SHOW 0 {} root.vcl\n{}\n// VCL.SHOW 1 {} child1.vcl\n{}\n// VCL.SHOW 2 {} child2.vcl\n{}",
            c0.len(),
            c0,
            c1.len(),
            c1,
            c2.len(),
            c2
        );
        let chunks = parse_chunks(&text).expect("should parse");
        assert_eq!(chunks.len(), 3);
        for chunk in &chunks {
            assert_eq!(chunk.declared_len as usize, chunk.content.len());
        }
    }
}
