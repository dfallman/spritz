use crate::soap;
use axum::{http::HeaderMap, response::Response};

const MRR_SERVICE: &str = "urn:microsoft.com:service:X_MS_MediaReceiverRegistrar:1";

pub async fn handle(headers: HeaderMap, _body: String) -> Response {
	let action = headers
		.get("soapaction")
		.and_then(|v| v.to_str().ok())
		.map(soap::parse_action)
		.unwrap_or_default();

	match action.as_str() {
		"IsAuthorized" | "IsValidated" => {
			soap::ok(soap::response(&action, MRR_SERVICE, "<Result>1</Result>"))
		}
		"RegisterDevice" => soap::ok(soap::response(
			"RegisterDevice",
			MRR_SERVICE,
			"<RegistrationRespMsg></RegistrationRespMsg>",
		)),
		_ => soap::err(soap::fault(401, "Invalid Action")),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use axum::http::{HeaderMap, HeaderValue};

	#[tokio::test]
	async fn is_authorized_always_allows() {
		let mut headers = HeaderMap::new();
		headers.insert(
			"soapaction",
			HeaderValue::from_static(
				r#""urn:microsoft.com:service:X_MS_MediaReceiverRegistrar:1#IsAuthorized""#,
			),
		);
		let res = handle(headers, String::new()).await;
		assert_eq!(res.status(), 200);
		let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
			.await
			.unwrap();
		let xml = String::from_utf8_lossy(&body);
		assert!(xml.contains("<Result>1</Result>"), "{xml}");
	}
}
