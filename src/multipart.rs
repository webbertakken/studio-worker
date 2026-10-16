//! `multipart/form-data` bodies (RFC 7578): the parts of a form a local client posts, such as a
//! picture file and its options.  Whole bodies only; the local API caps their size first.

/// One field of a form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub data: Vec<u8>,
}

/// Why a body is no readable form.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MultipartError {
    #[error("the body never opens with its boundary")]
    NoOpening,
    #[error("part {0} is not closed by the boundary")]
    Unterminated(usize),
    #[error("part {0} has no end to its headers")]
    NoHeaderEnd(usize),
    #[error("part {0} has no Content-Disposition form-data name")]
    NoName(usize),
    #[error("part {0} headers are not UTF-8")]
    HeaderEncoding(usize),
}

/// The `boundary` parameter of a `multipart/form-data` content type, if it is one.
pub fn boundary(content_type: &str) -> Option<String> {
    let (kind, params) = content_type.split_once(';')?;
    if !kind.trim().eq_ignore_ascii_case("multipart/form-data") {
        return None;
    }
    parameters(params)
        .into_iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("boundary"))
        .map(|(_, v)| v)
        .filter(|b| !b.is_empty())
}

/// Every part of `body`, in order.
pub fn parse(body: &[u8], boundary: &str) -> Result<Vec<Part>, MultipartError> {
    let opening = format!("--{boundary}");
    let between = format!("\r\n--{boundary}");
    let mut at = if body.starts_with(opening.as_bytes()) {
        opening.len()
    } else {
        find(body, between.as_bytes(), 0).ok_or(MultipartError::NoOpening)? + between.len()
    };
    let mut parts = Vec::new();
    loop {
        let index = parts.len() + 1;
        let rest = &body[at..];
        if rest.starts_with(b"--") {
            return Ok(parts);
        }
        // Transport padding then CRLF after a boundary.
        let line_end = find(body, b"\r\n", at).ok_or(MultipartError::Unterminated(index))?;
        let headers_start = line_end + 2;
        let headers_end =
            find(body, b"\r\n\r\n", headers_start).ok_or(MultipartError::NoHeaderEnd(index))?;
        let data_start = headers_end + 4;
        let data_end = find(body, between.as_bytes(), data_start)
            .ok_or(MultipartError::Unterminated(index))?;
        let headers = std::str::from_utf8(&body[headers_start..headers_end])
            .map_err(|_| MultipartError::HeaderEncoding(index))?;
        parts.push(part(index, headers, &body[data_start..data_end])?);
        at = data_end + between.len();
    }
}

fn part(index: usize, headers: &str, data: &[u8]) -> Result<Part, MultipartError> {
    let mut name = None;
    let mut filename = None;
    let mut content_type = None;
    for line in headers.split("\r\n") {
        let Some((field, value)) = line.split_once(':') else {
            continue;
        };
        let field = field.trim();
        if field.eq_ignore_ascii_case("content-disposition") {
            let Some((kind, params)) = value.split_once(';') else {
                continue;
            };
            if !kind.trim().eq_ignore_ascii_case("form-data") {
                continue;
            }
            for (key, value) in parameters(params) {
                if key.eq_ignore_ascii_case("name") {
                    name = Some(value);
                } else if key.eq_ignore_ascii_case("filename") {
                    filename = Some(value);
                }
            }
        } else if field.eq_ignore_ascii_case("content-type") {
            content_type = Some(value.trim().to_string());
        }
    }
    Ok(Part {
        name: name.ok_or(MultipartError::NoName(index))?,
        filename,
        content_type,
        data: data.to_vec(),
    })
}

/// `key=value` pairs separated by `;`; a value may be quoted (and then hold `;`).
fn parameters(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = text;
    loop {
        rest = rest.trim_start_matches([';', ' ', '\t']);
        if rest.is_empty() {
            return out;
        }
        let Some(eq) = rest.find('=') else {
            return out;
        };
        let key = rest[..eq].trim().to_string();
        rest = rest[eq + 1..].trim_start();
        let value = if let Some(quoted) = rest.strip_prefix('"') {
            let mut value = String::new();
            let mut chars = quoted.char_indices();
            let mut end = quoted.len();
            while let Some((i, c)) = chars.next() {
                match c {
                    '\\' => {
                        if let Some((_, escaped)) = chars.next() {
                            value.push(escaped);
                        }
                    }
                    '"' => {
                        end = i + 1;
                        break;
                    }
                    c => value.push(c),
                }
            }
            rest = &quoted[end.min(quoted.len())..];
            value
        } else {
            let end = rest.find(';').unwrap_or(rest.len());
            let value = rest[..end].trim().to_string();
            rest = &rest[end..];
            value
        };
        out.push((key, value));
    }
}

/// Where `needle` first occurs in `haystack` at or after `from`.
fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    let first = *needle.first()?;
    let last_start = haystack.len().checked_sub(needle.len())?;
    (from..=last_start).find(|&i| haystack[i] == first && &haystack[i..i + needle.len()] == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(boundary: &str, parts: &[(&str, Option<&str>, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (name, filename, data) in parts {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            match filename {
                Some(f) => body.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{name}\"; filename=\"{f}\"\r\n\
                         Content-Type: image/jpeg\r\n\r\n"
                    )
                    .as_bytes(),
                ),
                None => body.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
                ),
            }
            body.extend_from_slice(data);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        body
    }

    #[test]
    fn the_boundary_is_read_from_the_content_type() {
        assert_eq!(
            boundary("multipart/form-data; boundary=----abc").as_deref(),
            Some("----abc")
        );
        assert_eq!(
            boundary("Multipart/Form-Data; charset=utf-8; boundary=\"a b;c\"").as_deref(),
            Some("a b;c")
        );
        assert_eq!(boundary("multipart/form-data"), None);
        assert_eq!(boundary("multipart/form-data; boundary="), None);
        assert_eq!(boundary("application/json; boundary=x"), None);
    }

    #[test]
    fn a_form_gives_its_files_and_fields_in_order() {
        let picture: &[u8] = b"\xff\xd8binary\r\n--not-the-boundary\x00\xff";
        let body = form(
            "XyZ",
            &[("image", Some("bird.jpg"), picture), ("topK", None, b"3")],
        );
        let parts = parse(&body, "XyZ").unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].name, "image");
        assert_eq!(parts[0].filename.as_deref(), Some("bird.jpg"));
        assert_eq!(parts[0].content_type.as_deref(), Some("image/jpeg"));
        assert_eq!(parts[0].data, picture);
        assert_eq!(parts[1].name, "topK");
        assert_eq!(parts[1].filename, None);
        assert_eq!(parts[1].data, b"3");
    }

    #[test]
    fn a_preamble_before_the_first_boundary_is_skipped() {
        let mut body = b"ignored preamble".to_vec();
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(&form("b", &[("a", None, b"1")]));
        assert_eq!(parse(&body, "b").unwrap()[0].data, b"1");
    }

    #[test]
    fn an_empty_form_has_no_parts() {
        assert!(parse(b"--b--\r\n", "b").unwrap().is_empty());
    }

    #[test]
    fn broken_forms_say_what_is_wrong() {
        assert_eq!(parse(b"nothing here", "b"), Err(MultipartError::NoOpening));
        assert_eq!(
            parse(
                b"--b\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nno end",
                "b"
            ),
            Err(MultipartError::Unterminated(1))
        );
        assert_eq!(
            parse(b"--b\r\nContent-Disposition: form-data; name=\"a\"", "b"),
            Err(MultipartError::NoHeaderEnd(1))
        );
        assert_eq!(
            parse(b"--b\r\nContent-Type: text/plain\r\n\r\nx\r\n--b--", "b"),
            Err(MultipartError::NoName(1))
        );
        assert_eq!(
            parse(b"--b\r\nX: \xff\r\n\r\nx\r\n--b--", "b"),
            Err(MultipartError::HeaderEncoding(1))
        );
        assert_eq!(parse(b"--b", "b"), Err(MultipartError::Unterminated(1)));
    }

    #[test]
    fn a_disposition_other_than_form_data_names_nothing() {
        let body = b"--b\r\nContent-Disposition: attachment; name=\"a\"\r\nOdd header line\r\n\r\nx\r\n--b--";
        assert_eq!(parse(body, "b"), Err(MultipartError::NoName(1)));
    }

    #[test]
    fn quoted_parameters_may_hold_separators_and_escapes() {
        let params = parameters(r#" name="a;b"; filename="say \"hi\".jpg"; bare=x ; broken"#);
        assert_eq!(
            params,
            vec![
                ("name".to_string(), "a;b".to_string()),
                ("filename".to_string(), "say \"hi\".jpg".to_string()),
                ("bare".to_string(), "x".to_string()),
            ]
        );
        assert_eq!(
            parameters("name=\"unclosed"),
            vec![("name".into(), "unclosed".into())]
        );
    }

    #[test]
    fn find_locates_needles_past_the_start() {
        assert_eq!(find(b"abcabc", b"bc", 2), Some(4));
        assert_eq!(find(b"ab", b"abc", 0), None);
        assert_eq!(find(b"abc", b"", 0), None);
    }
}
