import { fromSSO } from "@aws-sdk/credential-providers";
import type { AwsCredentialIdentityProvider } from "@aws-sdk/types";

/**
 * Creates an SSO credential provider for a given AWS profile.
 */
export function createCredentialProvider(
  profile: string
): AwsCredentialIdentityProvider {
  return fromSSO({ profile });
}

/**
 * Validates that SSO credentials are available and not expired.
 * Returns true if valid, false if login is needed.
 */
export async function validateCredentials(
  profile: string
): Promise<boolean> {
  try {
    const provider = createCredentialProvider(profile);
    await provider();
    return true;
  } catch {
    return false;
  }
}
