//! iLink protocol version constants, shared by iLink HTTP callers (QR endpoints, wechat-bridge).
//!
//! The iLink backend (ilinkai.weixin.qq.com) gates clients by version: stale values
//! cause the WeChat app to show "Please upgrade WeChat interface version with OpenClaw"
//! on QR scan. Track these against `@tencent-weixin/openclaw-weixin` upstream.

/// Header value for `iLink-App-Id`, taken from upstream `package.json` → `ilink_appid`.
pub const ILINK_APP_ID: &str = "bot";

/// Upstream `@tencent-weixin/openclaw-weixin` pkg.version we impersonate.
/// Used as the `base_info.channel_version` body field.
pub const ILINK_CHANNEL_VERSION: &str = "2.1.7";

/// Header value for `iLink-App-ClientVersion` — decimal string of the packed uint32
/// `(major << 16) | (minor << 8) | patch` derived from [`ILINK_CHANNEL_VERSION`].
pub const ILINK_CLIENT_VERSION: &str = "131335"; // 2.1.7 → 0x020107

/// Pack a `"MAJOR.MINOR.PATCH"` semver into the uint32 the iLink backend expects.
pub fn encode_client_version(version: &str) -> u32 {
    let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let major = parts.next().unwrap_or(0) & 0xff;
    let minor = parts.next().unwrap_or(0) & 0xff;
    let patch = parts.next().unwrap_or(0) & 0xff;
    (major << 16) | (minor << 8) | patch
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_encode_2_1_7_as_131335() {
        assert_eq!(encode_client_version("2.1.7"), 131335);
        assert_eq!(encode_client_version("2.1.7"), 0x0002_0107);
    }

    #[test]
    fn should_encode_1_0_0_as_65536() {
        assert_eq!(encode_client_version("1.0.0"), 0x0001_0000);
    }

    #[test]
    fn should_zero_missing_components() {
        assert_eq!(encode_client_version("3"), 0x0003_0000);
        assert_eq!(encode_client_version("3.2"), 0x0003_0200);
    }

    #[test]
    fn should_mask_oversized_components() {
        assert_eq!(encode_client_version("256.0.0"), 0);
        assert_eq!(encode_client_version("255.255.255"), 0x00ff_ffff);
    }

    #[test]
    fn should_match_encoded_constant() {
        let expected = encode_client_version(ILINK_CHANNEL_VERSION).to_string();
        assert_eq!(ILINK_CLIENT_VERSION, expected);
    }
}
