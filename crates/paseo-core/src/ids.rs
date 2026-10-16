use crate::error::{CoreError, Result};

/// `validateAgentId` (`z.guid()`): 8-4-4-4-12 hex digits, any version.
pub fn validate_agent_id(agent_id: &str, source: &str) -> Result<String> {
    if is_guid(agent_id) {
        Ok(agent_id.to_owned())
    } else {
        Err(CoreError::msg(format!("{source}: agentId must be a UUID")))
    }
}

fn is_guid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    const LENS: [usize; 5] = [8, 4, 4, 4, 12];
    groups.len() == 5
        && groups
            .iter()
            .zip(LENS)
            .all(|(g, len)| g.len() == len && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_validation() {
        assert!(validate_agent_id("00000000-0000-4000-8000-000000000001", "x").is_ok());
        assert!(validate_agent_id("not-a-uuid", "x").is_err());
        assert!(validate_agent_id("00000000-0000-4000-8000-00000000000g", "x").is_err());
        assert_eq!(
            validate_agent_id("nope", "subscribe")
                .unwrap_err()
                .to_string(),
            "subscribe: agentId must be a UUID"
        );
    }
}
