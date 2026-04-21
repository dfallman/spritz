use axum::{body::Body, response::Response};

pub fn parse_action(header_value: &str) -> String {
	let s = header_value.trim().trim_matches('"');
	s.rsplit('#').next().unwrap_or("").trim_matches('"').to_string()
}

/// Extract the text content of the first <LocalName> or <ns:LocalName> element.
pub fn extract_tag_value(xml: &str, local_name: &str) -> Option<String> {
	find_content(xml, &format!("<{local_name}>"))
		.or_else(|| find_content(xml, &format!(":{local_name}>")))
}

fn find_content(xml: &str, open_suffix: &str) -> Option<String> {
	let start = xml.find(open_suffix)? + open_suffix.len();
	let end = xml[start..].find('<')? + start;
	Some(xml[start..end].trim().to_string())
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
		.header("server", "Linux/5.0 UPnP/1.0 Spritz/0.1")
		.body(Body::from(body))
		.unwrap()
}
