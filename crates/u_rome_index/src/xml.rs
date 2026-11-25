//! Name extraction from XML payloads.
//!
//! RAGE content manifests (`CDataFileMgr__ContentDef`, usually `content.xml`)
//! and DLC setup files (`SSetupData`, usually `setup2.xml`) list the archives
//! and data files a pack mounts, e.g.
//!
//! ```xml
//! <CDataFileMgr__ContentDef>
//!   <dataFiles>
//!     <Item>
//!       <filename>dlc_pack:/x64/levels/vehicles.rpf</filename>
//!       <fileType>RPF_FILE</fileType>
//!     </Item>
//!   </dataFiles>
//!   <filesToEnable><Item>dlc_pack:/x64/levels/vehicles.rpf</Item></filesToEnable>
//! </CDataFileMgr__ContentDef>
//! ```
//!
//! The element layout above is the GTA V convention (documented publicly by
//! the modding community); GTA VI is assumed to keep it, which is why the
//! extractor is generic: it takes every text and attribute value that looks
//! like a file path, from any element, and emits the path, the path without
//! its device prefix, the basename and the stem. Values of a few known
//! name-carrying elements (`deviceName`, `nameHash`, ...) are taken even when
//! they do not look like paths. Malformed XML is tolerated: whatever was
//! collected before an unrecoverable error is kept, and a raw token scan
//! fills in after the error.

use std::collections::HashSet;

use quick_xml::XmlVersion;
use quick_xml::events::Event;
use quick_xml::reader::Reader;

/// Longest single value considered; longer text is not a file name.
const MAX_VALUE_LEN: usize = 512;
/// Upper bound on names returned from one document.
const MAX_NAMES: usize = 1_000_000;
/// Upper bound on element nesting tracked.
const MAX_DEPTH: usize = 256;
/// Parse errors tolerated before giving up on structured parsing.
const MAX_ERRORS: usize = 64;

/// Elements whose values are names even when they do not look like paths.
/// From GTA V `content.xml` / `setup2.xml` layouts (community documentation).
const NAME_ELEMENTS: &[&str] = &[
    "filename",
    "datfile",
    "devicename",
    "namehash",
    "name",
    "changesetname",
    "dependency",
    "path",
    "file",
];

struct Collector {
    seen: HashSet<String>,
    out: Vec<String>,
}

impl Collector {
    fn push(&mut self, s: &str) {
        if s.is_empty() || self.out.len() >= MAX_NAMES || self.seen.contains(s) {
            return;
        }
        self.seen.insert(s.to_owned());
        self.out.push(s.to_owned());
    }

    /// Consider one raw value (text node or attribute value).
    fn value(&mut self, raw: &str, element: Option<&str>) {
        let name_element = element.is_some_and(|e| {
            NAME_ELEMENTS
                .iter()
                .any(|n| n.eq_ignore_ascii_case(local_part(e)))
        });
        for token in raw.split_whitespace() {
            let token = token.trim_matches(|c| matches!(c, '"' | '\'' | ',' | ';'));
            if token.is_empty() || token.len() > MAX_VALUE_LEN || !token.chars().all(path_char) {
                continue;
            }
            if looks_like_path(token) {
                self.path(token);
            } else if name_element && is_identifier(token) {
                self.push(token);
            }
        }
    }

    /// Emit a path and its derived forms.
    fn path(&mut self, token: &str) {
        let norm = token.replace('\\', "/");
        self.push(&norm);
        // Device prefix: "dlc_pack:/x64/a.rpf" -> "x64/a.rpf".
        if let Some(pos) = norm.find(":/") {
            let rest = norm.get(pos + 2..).unwrap_or("").trim_start_matches('/');
            self.push(rest);
        }
        let base = norm.rsplit('/').next().unwrap_or("");
        if !base.is_empty() && base != norm {
            self.push(base);
        }
        if let Some((stem, ext)) = base.rsplit_once('.')
            && is_extension(ext)
            && stem.chars().any(|c| c.is_ascii_alphabetic())
        {
            self.push(stem);
        }
    }
}

fn local_part(tag: &str) -> &str {
    tag.rsplit(':').next().unwrap_or(tag)
}

fn path_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '_' | '-' | '.' | '/' | '\\' | ':' | '%' | '$' | '@' | '~' | '+'
        )
}

fn is_extension(ext: &str) -> bool {
    (1..=10).contains(&ext.len())
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
        && ext.chars().any(|c| c.is_ascii_alphabetic())
}

fn is_identifier(token: &str) -> bool {
    token.len() >= 2
        && token.chars().any(|c| c.is_ascii_alphabetic())
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn looks_like_path(token: &str) -> bool {
    if !token.chars().any(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    if token.contains('/') || token.contains('\\') {
        return true;
    }
    match token.rsplit_once('.') {
        Some((stem, ext)) => !stem.is_empty() && is_extension(ext),
        None => false,
    }
}

/// Extract file paths and names from an XML payload.
///
/// Returns each distinct value once, in document order. Never fails: on
/// malformed input it returns what it could recover (see module docs).
pub fn names_from_xml(bytes: &[u8]) -> Vec<String> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let text = String::from_utf8_lossy(bytes);
    let mut c = Collector {
        seen: HashSet::new(),
        out: Vec::new(),
    };

    let mut reader = Reader::from_str(&text);
    {
        let cfg = reader.config_mut();
        cfg.check_end_names = false;
        cfg.allow_unmatched_ends = true;
        cfg.trim_text(true);
    }

    let mut stack: Vec<String> = Vec::new();
    let mut pending = String::new();
    let mut errors = 0usize;
    let mut failed_at: Option<u64> = None;

    loop {
        let before = reader.buffer_position();
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                flush(&mut c, &mut pending, &stack);
                attributes(&mut c, &e);
                if stack.len() < MAX_DEPTH {
                    stack.push(e.local_name().as_ref().to_owned());
                }
            }
            Ok(Event::Empty(e)) => {
                flush(&mut c, &mut pending, &stack);
                attributes(&mut c, &e);
            }
            Ok(Event::End(_)) => {
                flush(&mut c, &mut pending, &stack);
                stack.pop();
            }
            Ok(Event::Text(t)) => {
                if pending.len() < MAX_VALUE_LEN * 4 {
                    pending.push_str(&t.xml10_content());
                }
            }
            Ok(Event::CData(t)) => {
                if pending.len() < MAX_VALUE_LEN * 4 {
                    pending.push_str(&t.into_inner());
                }
            }
            Ok(Event::GeneralRef(r)) => {
                let resolved = match r.resolve_char_ref() {
                    Ok(Some(ch)) => Some(ch.to_string()),
                    _ => quick_xml::escape::resolve_predefined_entity(&r.into_inner())
                        .map(str::to_owned),
                };
                if let Some(s) = resolved {
                    pending.push_str(&s);
                }
            }
            Ok(Event::Eof) => {
                flush(&mut c, &mut pending, &stack);
                break;
            }
            Ok(_) => {}
            Err(e) => {
                errors += 1;
                let pos = reader.error_position();
                log::debug!("names_from_xml: parse error at byte {pos}: {e}");
                // The raw scan below re-reads from the first error onwards.
                failed_at.get_or_insert(pos);
                if errors >= MAX_ERRORS || reader.buffer_position() <= before {
                    log::warn!(
                        "names_from_xml: giving up structured parse at byte {pos} \
                         after {errors} error(s): {e}"
                    );
                    break;
                }
            }
        }
    }

    if let Some(pos) = failed_at {
        // Lenient fallback: scan the unparsed remainder for path-like tokens.
        let start = usize::try_from(pos).unwrap_or(usize::MAX);
        let rest = text.get(start..).unwrap_or("");
        let cleaned: String = rest
            .chars()
            .map(|ch| if path_char(ch) { ch } else { ' ' })
            .collect();
        for token in cleaned.split_whitespace() {
            if token.len() <= MAX_VALUE_LEN && looks_like_path(token) {
                c.path(token);
            }
        }
    }
    c.out
}

fn flush(c: &mut Collector, pending: &mut String, stack: &[String]) {
    if !pending.trim().is_empty() {
        c.value(pending, stack.last().map(String::as_str));
    }
    pending.clear();
}

fn attributes(c: &mut Collector, e: &quick_xml::events::BytesStart<'_>) {
    for attr in e.attributes().with_checks(false).flatten() {
        let value = attr
            .normalized_value(XmlVersion::Implicit1_0)
            .map(|v| v.into_owned())
            .unwrap_or_else(|_| attr.value.clone().into_owned());
        let key = attr.key.local_name();
        c.value(&value, Some(key.as_ref()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_def() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<CDataFileMgr__ContentDef>
  <disabledFiles />
  <includedXmlFiles itemType="CDataFileMgr__DataFileArray" />
  <dataFiles>
    <Item>
      <filename>dlc_pack:/%PLATFORM%/levels/gta6/vehicles.rpf</filename>
      <fileType>RPF_FILE</fileType>
      <overlay value="false" />
      <disabled value="true" />
    </Item>
    <Item>
      <filename>dlc_pack:\common\data\handling.meta</filename>
      <fileType>HANDLING_FILE</fileType>
    </Item>
  </dataFiles>
  <contentChangeSets>
    <Item>
      <changeSetName>PACK_AUTOGEN</changeSetName>
      <filesToEnable>
        <Item>dlc_pack:/x64/levels/gta6/vehicles.rpf</Item>
      </filesToEnable>
    </Item>
  </contentChangeSets>
</CDataFileMgr__ContentDef>"#;
        let names = names_from_xml(xml);
        for want in [
            "dlc_pack:/%PLATFORM%/levels/gta6/vehicles.rpf",
            "%PLATFORM%/levels/gta6/vehicles.rpf",
            "vehicles.rpf",
            "vehicles",
            "dlc_pack:/common/data/handling.meta",
            "common/data/handling.meta",
            "handling.meta",
            "handling",
            "x64/levels/gta6/vehicles.rpf",
            "PACK_AUTOGEN",
        ] {
            assert!(names.iter().any(|n| n == want), "missing {want}: {names:?}");
        }
        for unwanted in ["RPF_FILE", "false", "true", "CDataFileMgr__DataFileArray"] {
            assert!(
                !names.iter().any(|n| n == unwanted),
                "unexpected {unwanted}"
            );
        }
        let unique: HashSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len());
    }

    #[test]
    fn setup_data_and_malformed() {
        let xml = br#"<SSetupData><deviceName>dlc_pack</deviceName>
<datFile>content.xml</datFile><nameHash>pack</nameHash><timeStamp>01/02/2026 10:00:00</timeStamp>
<order value="12"/></SSetupData>"#;
        let names = names_from_xml(xml);
        assert!(names.contains(&"dlc_pack".to_owned()));
        assert!(names.contains(&"content.xml".to_owned()));
        assert!(names.contains(&"content".to_owned()));
        assert!(names.contains(&"pack".to_owned()));
        assert!(!names.contains(&"12".to_owned()));

        // Truncated and broken: keep what precedes, scan the rest.
        let bad =
            b"<a><filename>x:/one.rpf</filename><b attr=\"unterminated><filename>two.ytd</fil";
        let names = names_from_xml(bad);
        assert!(names.contains(&"one.rpf".to_owned()), "{names:?}");
        assert!(names.contains(&"two.ytd".to_owned()), "{names:?}");

        assert!(names_from_xml(b"").is_empty());
        assert!(names_from_xml(&[0xFF, 0xFE, 0x00, 0x3C]).is_empty());
    }
}
