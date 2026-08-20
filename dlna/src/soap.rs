use axum::{body::Body, response::Response};

pub fn parse_action(header_value: &str) -> String {
	let s = header_value.trim().trim_matches('"');
	s.rsplit('#')
		.next()
		.unwrap_or("")
		.trim_matches('"')
		.to_string()
}

/// Extract the text content of the first <LocalName> or <ns:LocalName> element.
/// The opening tag may carry attributes (`<ObjectID xmlns="...">`).
pub fn extract_tag_value(xml: &str, local_name: &str) -> Option<String> {
	content_after_open(xml, &format!("<{local_name}"))
		.or_else(|| content_after_open(xml, &format!(":{local_name}")))
}

fn content_after_open(xml: &str, needle: &str) -> Option<String> {
	let mut search_from = 0;
	while let Some(rel) = xml[search_from..].find(needle) {
		let after_name = search_from + rel + needle.len();
		let rest = xml.get(after_name..)?;
		let first = rest.as_bytes().first().copied();
		if !matches!(
			first,
			Some(b'>') | Some(b'/') | Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r')
		) {
			search_from = after_name;
			continue;
		}
		let gt = rest.find('>')?;
		let content = xml.get(after_name + gt + 1..)?;
		let end = content.find('<')?;
		return Some(content[..end].trim().to_string());
	}
	None
}

pub fn response(action: &str, service_type: &str, inner: &str) -> String {
	format!(
		r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:{action}Response xmlns:u="{service_type}">
      {inner}
    </u:{action}Response>
  </s:Body>
</s:Envelope>"#
	)
}

pub fn fault(error_code: u32, description: &str) -> String {
	format!(
		r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <s:Fault>
      <faultcode>s:Client</faultcode>
      <faultstring>UPnPError</faultstring>
      <detail>
        <UPnPError xmlns="urn:schemas-upnp-org:control-1-0">
          <errorCode>{error_code}</errorCode>
          <errorDescription>{description}</errorDescription>
        </UPnPError>
      </detail>
    </s:Fault>
  </s:Body>
</s:Envelope>"#
	)
}

pub fn ok(body: String) -> Response {
	build(200, body)
}

pub fn err(body: String) -> Response {
	build(500, body)
}

fn build(status: u16, body: String) -> Response {
	Response::builder()
		.status(status)
		.header("content-type", "text/xml; charset=\"utf-8\"")
		.header("ext", "")
		.header("server", crate::SERVER)
		.body(Body::from(body))
		.unwrap()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parse_action_reads_the_name_after_the_hash() {
		assert_eq!(
			parse_action(r#""urn:schemas-upnp-org:service:ContentDirectory:1#Browse""#),
			"Browse"
		);
		assert_eq!(
			parse_action("urn:schemas-upnp-org:service:ContentDirectory:1#GetSystemUpdateID"),
			"GetSystemUpdateID"
		);
	}

	#[test]
	fn extract_tag_value_reads_plain_and_namespaced_tags() {
		assert_eq!(
			extract_tag_value("<ObjectID>V</ObjectID>", "ObjectID").as_deref(),
			Some("V")
		);
		assert_eq!(
			extract_tag_value("<u:ObjectID>A</u:ObjectID>", "ObjectID").as_deref(),
			Some("A")
		);
	}

	#[test]
	fn extract_tag_value_reads_tags_with_attributes() {
		assert_eq!(
			extract_tag_value(
				r#"<ObjectID xmlns="urn:schemas-upnp-org:service:ContentDirectory:1">f:2</ObjectID>"#,
				"ObjectID"
			)
			.as_deref(),
			Some("f:2")
		);
		assert_eq!(
			extract_tag_value("<u:ObjectID xmlns:u=\"urn:x\">m:1</u:ObjectID>", "ObjectID")
				.as_deref(),
			Some("m:1")
		);
	}
}
