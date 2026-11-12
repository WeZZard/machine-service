//! Minimal XML property-list reader and writer.
//!
//! The installer reads the installed LaunchAgent plist, merges it, and writes it
//! back. The Python source uses `plistlib`; this module covers the XML subset
//! that `plistlib.dumps` emits for the installer's own dictionaries:
//! dictionaries, arrays, strings, booleans, integers, reals and data. A file
//! that uses a different property-list encoding is rejected rather than
//! silently reinterpreted.

use serde_json::{Map, Value};

/// Parse an XML property list into the same JSON-shaped tree the rest of the
/// crate manipulates. Returns the Python-visible reason text on malformed input.
pub fn parse(data: &[u8]) -> Result<Value, String> {
    let text = std::str::from_utf8(data).map_err(|error| error.to_string())?;
    let mut parser = Parser {
        chars: text.chars().collect(),
        pos: 0,
    };
    let value = parser.parse_document()?;
    Ok(value)
}

/// Serialize a plist-shaped [`Value`] as an XML property list.
pub fn dumps(value: &Value) -> Vec<u8> {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
    );
    out.push_str("<plist version=\"1.0\">\n");
    write_value(&mut out, value, 1);
    out.push_str("</plist>\n");
    out.into_bytes()
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let current = self.peek();
        if current.is_some() {
            self.pos += 1;
        }
        current
    }

    fn starts_with(&self, needle: &str) -> bool {
        for (offset, character) in needle.chars().enumerate() {
            if self.chars.get(self.pos + offset) != Some(&character) {
                return false;
            }
        }
        true
    }

    fn expect(&mut self, needle: &str) -> Result<(), String> {
        if self.starts_with(needle) {
            self.pos += needle.chars().count();
            Ok(())
        } else {
            Err(format!("expected {needle:?}"))
        }
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(character) if character.is_whitespace()) {
            self.pos += 1;
        }
    }

    fn skip_misc(&mut self) -> Result<(), String> {
        loop {
            self.skip_ws();
            if self.starts_with("<?") {
                while !self.starts_with("?>") {
                    if self.bump().is_none() {
                        return Err("unterminated declaration".to_string());
                    }
                }
                self.pos += 2;
            } else if self.starts_with("<!--") {
                while !self.starts_with("-->") {
                    if self.bump().is_none() {
                        return Err("unterminated comment".to_string());
                    }
                }
                self.pos += 3;
            } else if self.starts_with("<!") {
                while self.peek() != Some('>') {
                    if self.bump().is_none() {
                        return Err("unterminated declaration".to_string());
                    }
                }
                self.pos += 1;
            } else {
                return Ok(());
            }
        }
    }

    fn parse_document(&mut self) -> Result<Value, String> {
        self.skip_misc()?;
        self.expect("<plist")?;
        self.finish_open_tag()?;
        let value = self.parse_value()?;
        self.skip_misc()?;
        self.expect("</plist>")?;
        Ok(value)
    }

    fn read_name(&mut self) -> String {
        let start = self.pos;
        while matches!(self.peek(), Some(character)
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' || character == '.')
        {
            self.pos += 1;
        }
        self.chars[start..self.pos].iter().collect()
    }

    /// Consume the remainder of an opening tag; report whether it self-closed.
    fn finish_open_tag(&mut self) -> Result<bool, String> {
        let mut quote: Option<char> = None;
        loop {
            let Some(character) = self.bump() else {
                return Err("unterminated tag".to_string());
            };
            if let Some(open) = quote {
                if character == open {
                    quote = None;
                }
                continue;
            }
            match character {
                '\'' | '"' => quote = Some(character),
                '/' => {
                    if self.peek() == Some('>') {
                        self.pos += 1;
                        return Ok(true);
                    }
                }
                '>' => return Ok(false),
                _ => {}
            }
        }
    }

    fn read_text(&mut self, closing: &str) -> Result<String, String> {
        let mut raw = String::new();
        loop {
            if self.starts_with(closing) {
                self.pos += closing.chars().count();
                break;
            }
            match self.bump() {
                Some(character) => raw.push(character),
                None => return Err(format!("missing closing tag {closing}")),
            }
        }
        Ok(unescape(&raw))
    }

    fn parse_value(&mut self) -> Result<Value, String> {
        self.skip_misc()?;
        if self.peek() != Some('<') {
            return Err("expected element".to_string());
        }
        self.pos += 1;
        let name = self.read_name();
        let self_closing = self.finish_open_tag()?;
        match name.as_str() {
            "dict" => {
                if self_closing {
                    return Ok(Value::Object(Map::new()));
                }
                let mut map = Map::new();
                loop {
                    self.skip_misc()?;
                    if self.starts_with("</dict>") {
                        self.pos += "</dict>".chars().count();
                        break;
                    }
                    if !self.starts_with("<key") {
                        return Err("expected <key>".to_string());
                    }
                    self.pos += "<key".chars().count();
                    if self.finish_open_tag()? {
                        return Err("empty <key/>".to_string());
                    }
                    let key = self.read_text("</key>")?;
                    let value = self.parse_value()?;
                    map.insert(key, value);
                }
                Ok(Value::Object(map))
            }
            "array" => {
                if self_closing {
                    return Ok(Value::Array(Vec::new()));
                }
                let mut items = Vec::new();
                loop {
                    self.skip_misc()?;
                    if self.starts_with("</array>") {
                        self.pos += "</array>".chars().count();
                        break;
                    }
                    items.push(self.parse_value()?);
                }
                Ok(Value::Array(items))
            }
            "string" => {
                if self_closing {
                    return Ok(Value::String(String::new()));
                }
                Ok(Value::String(self.read_text("</string>")?))
            }
            "integer" => {
                if self_closing {
                    return Err("empty <integer/>".to_string());
                }
                let text = self.read_text("</integer>")?;
                text.trim()
                    .parse::<i64>()
                    .map(Value::from)
                    .map_err(|error| error.to_string())
            }
            "real" => {
                if self_closing {
                    return Err("empty <real/>".to_string());
                }
                let text = self.read_text("</real>")?;
                text.trim()
                    .parse::<f64>()
                    .map(Value::from)
                    .map_err(|error| error.to_string())
            }
            "true" => {
                if !self_closing {
                    self.expect("</true>")?;
                }
                Ok(Value::Bool(true))
            }
            "false" => {
                if !self_closing {
                    self.expect("</false>")?;
                }
                Ok(Value::Bool(false))
            }
            "data" => {
                if self_closing {
                    return Ok(Value::String(String::new()));
                }
                // The installer never interprets data values; preserve them as
                // their base64 text so an unrelated key survives a rewrite.
                Ok(Value::String(self.read_text("</data>")?))
            }
            other => Err(format!("unsupported plist element: {other}")),
        }
    }
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '&' {
            out.push(character);
            continue;
        }
        let mut entity = String::new();
        let mut terminated = false;
        while let Some(&next) = characters.peek() {
            if next == ';' {
                characters.next();
                terminated = true;
                break;
            }
            if next.is_whitespace() || next == '&' || next == '<' {
                break;
            }
            entity.push(next);
            characters.next();
        }
        if !terminated {
            out.push('&');
            out.push_str(&entity);
            continue;
        }
        let decoded = match entity.as_str() {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                u32::from_str_radix(&entity[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
            }
            _ if entity.starts_with('#') => {
                entity[1..].parse::<u32>().ok().and_then(char::from_u32)
            }
            _ => None,
        };
        match decoded {
            Some(decoded) => out.push(decoded),
            None => {
                out.push('&');
                out.push_str(&entity);
                out.push(';');
            }
        }
    }
    out
}

fn write_value(out: &mut String, value: &Value, level: usize) {
    match value {
        Value::Object(map) => {
            indent(out, level);
            out.push_str("<dict>\n");
            for (key, item) in map {
                indent(out, level + 1);
                out.push_str("<key>");
                out.push_str(&escape(key));
                out.push_str("</key>\n");
                write_value(out, item, level + 1);
            }
            indent(out, level);
            out.push_str("</dict>\n");
        }
        Value::Array(items) => {
            indent(out, level);
            out.push_str("<array>\n");
            for item in items {
                write_value(out, item, level + 1);
            }
            indent(out, level);
            out.push_str("</array>\n");
        }
        Value::String(text) => {
            indent(out, level);
            out.push_str("<string>");
            out.push_str(&escape(text));
            out.push_str("</string>\n");
        }
        Value::Bool(true) => {
            indent(out, level);
            out.push_str("<true/>\n");
        }
        Value::Bool(false) => {
            indent(out, level);
            out.push_str("<false/>\n");
        }
        Value::Number(number) => {
            indent(out, level);
            if let Some(integer) = number.as_i64() {
                out.push_str(&format!("<integer>{integer}</integer>\n"));
            } else if let Some(integer) = number.as_u64() {
                out.push_str(&format!("<integer>{integer}</integer>\n"));
            } else if let Some(real) = number.as_f64() {
                out.push_str(&format!("<real>{real}</real>\n"));
            }
        }
        Value::Null => {
            indent(out, level);
            out.push_str("<string></string>\n");
        }
    }
}

fn indent(out: &mut String, level: usize) {
    for _ in 0..level {
        out.push('\t');
    }
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(character),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_dictionaries_arrays_and_scalars() {
        let value = json!({
            "Label": "com.wezzard.machine-service",
            "RunAtLoad": true,
            "KeepAlive": false,
            "Count": 3,
            "Arguments": ["/bin/vm-service"],
            "EnvironmentVariables": {"CUSTOM": "A&B <C>", "HOME": "/Users/example"},
        });
        let encoded = dumps(&value);
        let decoded = parse(&encoded).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn parses_plistlib_style_document() {
        let document = br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>com.wezzard.machine-service</string>
	<key>ProgramArguments</key>
	<array>
		<string>/bin/true</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
</dict>
</plist>
"#;
        let value = parse(document).unwrap();
        assert_eq!(value["Label"], json!("com.wezzard.machine-service"));
        assert_eq!(value["ProgramArguments"][0], json!("/bin/true"));
        assert_eq!(value["RunAtLoad"], json!(true));
    }

    #[test]
    fn rejects_malformed_document() {
        assert!(parse(b"not a plist").is_err());
    }
}
