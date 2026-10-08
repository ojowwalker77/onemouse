//! What a physical key on the main machine becomes on the other one, so
//! shortcuts keep their muscle memory.

use onemouse_protocol::KeyCode;
use onemouse_protocol::key::*;

/// Mac keyboard → Windows: Cmd → Ctrl (Cmd+C copies), Ctrl → Windows key.
/// Option is already Alt.
pub fn to_secondary(key: KeyCode) -> KeyCode {
    match key {
        LEFT_META => LEFT_CTRL,
        RIGHT_META => RIGHT_CTRL,
        LEFT_CTRL => LEFT_META,
        RIGHT_CTRL => RIGHT_META,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaps_cmd_to_ctrl_and_back() {
        assert_eq!(to_secondary(LEFT_META), LEFT_CTRL);
        assert_eq!(to_secondary(RIGHT_CTRL), RIGHT_META);
        assert_eq!(to_secondary(LEFT_ALT), LEFT_ALT);
        assert_eq!(to_secondary(A), A);
    }
}
