//! Google publisher resources for express mode and project-scoped connections.

pub const ADAPTER: &str = "google-agent-platform";
pub const ENDPOINT: &str = "https://aiplatform.googleapis.com";

#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    #[error("Google Agent Platform derives its endpoint from the project and location")]
    Base,
    #[error("Google project and location must use lowercase ASCII letters, digits or hyphens")]
    Resource,
    #[error("Google project is required when specifying a location")]
    Project,
}

pub fn publisher(base: &str, project: &str, location: &str) -> Result<String, EndpointError> {
    if base.trim_end_matches('/') != ENDPOINT {
        return Err(EndpointError::Base);
    }
    if project.is_empty() {
        return if location.is_empty() {
            Ok(format!("{ENDPOINT}/v1/publishers/google/models"))
        } else {
            Err(EndpointError::Project)
        };
    }
    let location = if location.is_empty() {
        "global"
    } else {
        location
    };
    if !resource(project) || !resource(location) {
        return Err(EndpointError::Resource);
    }
    let host = match location {
        "global" => ENDPOINT.to_owned(),
        "us" | "eu" => format!("https://aiplatform.{location}.rep.googleapis.com"),
        _ => format!("https://{location}-aiplatform.googleapis.com"),
    };
    Ok(format!(
        "{host}/v1/projects/{project}/locations/{location}/publishers/google/models"
    ))
}

fn resource(value: &str) -> bool {
    (1..=63).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

pub fn valid_model(model: &str) -> bool {
    model.starts_with("gemini-")
        && model.len() > "gemini-".len()
        && model.len() <= 100
        && model.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
}
